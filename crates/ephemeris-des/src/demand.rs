//! Demand event reader from Arrow IPC.
//!
//! Reads the demand table (sorted by appear_atu) and provides a cursor-based
//! drain for the simulation loop.

use crate::types::{Atu, CapacityUnits, NodeId, PriorityClass, FlowUnitId};

/// A single demand event (flow unit request).
#[derive(Debug, Clone)]
pub struct DemandEvent {
    pub flow_unit_id: FlowUnitId,
    pub appear_atu: Atu,
    pub due_atu: Atu,
    pub origin_node_id: NodeId,
    pub dest_node_id: NodeId,
    pub size_units: CapacityUnits,
    pub priority_class: PriorityClass,
    pub handling_flags: u8,
}

/// Queue of demand events, draining in chronological order.
pub struct DemandQueue {
    events: Vec<DemandEvent>,
    cursor: usize,
}

impl DemandQueue {
    /// Build from arrays (as read from Arrow IPC).
    pub fn from_arrays(
        flow_unit_ids: &[u64],
        appear_atus: &[i64],
        due_atus: &[i64],
        origin_node_ids: &[u32],
        dest_node_ids: &[u32],
        size_units: &[u32],
        priority_classes: &[u8],
        handling_flags: &[u8],
    ) -> Self {
        let n = flow_unit_ids.len();
        let mut events = Vec::with_capacity(n);
        for i in 0..n {
            events.push(DemandEvent {
                flow_unit_id: flow_unit_ids[i],
                appear_atu: appear_atus[i],
                due_atu: due_atus[i],
                origin_node_id: origin_node_ids[i],
                dest_node_id: dest_node_ids[i],
                size_units: size_units[i],
                priority_class: priority_classes[i],
                handling_flags: handling_flags[i],
            });
        }
        // Should already be sorted by appear_atu from Python
        debug_assert!(events.windows(2).all(|w| w[0].appear_atu <= w[1].appear_atu));
        DemandQueue { events, cursor: 0 }
    }

    /// All events regardless of cursor position (read-only). Used by the
    /// slot-reservation installer to resolve flow unit sizes before any
    /// event has been injected.
    pub fn all_events(&self) -> &[DemandEvent] {
        &self.events
    }

    /// Drain all events with `appear_atu <= until_atu`. Advances cursor.
    pub fn drain_until(&mut self, until_atu: Atu) -> &[DemandEvent] {
        let start = self.cursor;
        while self.cursor < self.events.len()
            && self.events[self.cursor].appear_atu <= until_atu
        {
            self.cursor += 1;
        }
        &self.events[start..self.cursor]
    }

    /// Peek next event time without advancing cursor.
    pub fn peek_time(&self) -> Option<Atu> {
        self.events.get(self.cursor).map(|e| e.appear_atu)
    }

    /// Total number of demand events.
    pub fn total(&self) -> usize {
        self.events.len()
    }

    /// Number of events already injected.
    pub fn injected(&self) -> usize {
        self.cursor
    }

    /// Number of events remaining.
    pub fn remaining(&self) -> usize {
        self.events.len() - self.cursor
    }

    /// Consume the queue and return the remaining (un-drained) events.
    /// Used by `PyEngine::inject_demand` to forward IPC-parsed events
    /// into a running engine.
    pub fn into_events(mut self) -> Vec<DemandEvent> {
        if self.cursor == 0 {
            self.events
        } else {
            self.events.drain(self.cursor..).collect()
        }
    }

    /// Append additional demand events (§E.3 shock injection).
    ///
    /// Sorts everything from `cursor..` so the cursor-forward window
    /// remains in non-decreasing `appear_atu` order. New events with
    /// `appear_atu < current_time` are still inserted but will fire on
    /// the next `inject_pending_demand` sweep.
    pub fn append_events(&mut self, mut new_events: Vec<DemandEvent>) {
        if new_events.is_empty() {
            return;
        }
        self.events.append(&mut new_events);
        // Sort the un-drained window by (appear_atu, flow_unit_id) — stable
        // in `flow_unit_id` for deterministic event-loop ordering.
        let cursor = self.cursor;
        self.events[cursor..].sort_by_key(|e| (e.appear_atu, e.flow_unit_id));
        debug_assert!(self.events[cursor..]
            .windows(2)
            .all(|w| w[0].appear_atu <= w[1].appear_atu));
    }
}
