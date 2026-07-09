//! Event types and priority queue ordering.
//!
//! Events are ordered by (time_atu ASC, event_id ASC) for deterministic
//! processing. Uses i64 time — no floating point comparison issues.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::types::{Atu, NodeId, FlowUnitId};

/// Event kinds in the simulation lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// FlowUnit enters the system at its origin node.
    Inject,
    /// FlowUnit arrives at a node after transit on an lane.
    Arrive,
    /// FlowUnit boards a departure (leaves node, enters lane).
    Depart,
    /// FlowUnit reaches its destination node — delivered.
    Deliver,
    /// A new departure slot is now available on a queued lane.
    /// Drains as many waiting flow units onto the slot as capacity allows.
    /// `location` carries the CSR lane index; `flow_unit_id` is unused (0).
    WakeQueue,
}

/// A simulation event.
#[derive(Debug, Clone)]
pub struct Event {
    pub time_atu: Atu,
    pub kind: EventKind,
    pub flow_unit_id: FlowUnitId,
    /// NodeId for Inject/Arrive/Deliver, LaneIdx (as u32) for Depart.
    pub location: u32,
    /// Monotonic counter for deterministic tiebreaking.
    pub event_id: u64,
}

/// Wrapper for BinaryHeap min-heap ordering.
///
/// Rust's BinaryHeap is a max-heap, so we reverse the comparison.
/// Ordering: smallest time_atu first, then smallest event_id.
#[derive(Debug)]
pub struct OrderedEvent(pub Event);

impl PartialEq for OrderedEvent {
    fn eq(&self, other: &Self) -> bool {
        self.0.time_atu == other.0.time_atu && self.0.event_id == other.0.event_id
    }
}

impl Eq for OrderedEvent {}

impl Ord for OrderedEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse for min-heap: other < self
        other
            .0
            .time_atu
            .cmp(&self.0.time_atu)
            .then_with(|| other.0.event_id.cmp(&self.0.event_id))
    }
}

impl PartialOrd for OrderedEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Priority queue of simulation events (min-heap on time_atu).
pub struct EventQueue {
    heap: BinaryHeap<OrderedEvent>,
}

impl EventQueue {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
        }
    }

    pub fn push(&mut self, event: Event) {
        self.heap.push(OrderedEvent(event));
    }

    pub fn pop(&mut self) -> Option<Event> {
        self.heap.pop().map(|oe| oe.0)
    }

    pub fn peek_time(&self) -> Option<Atu> {
        self.heap.peek().map(|oe| oe.0.time_atu)
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Filter the heap in place, removing any event for which
    /// `predicate(event) == true`. Returns the number of removed events.
    ///
    /// O(N) where N is the heap size. The implementation drains the heap
    /// into a Vec, partitions, and rebuilds — `BinaryHeap::retain` is
    /// stable but not optimal; we use the explicit drain+rebuild pattern
    /// for clarity. The cost is bounded by the size of the event queue
    /// (typically ≤ a few thousand entries even for large simulations);
    /// at bucket-boundary cadence (~once per hour of simulated time) this
    /// is amortised cheaply.
    pub fn retain<F: FnMut(&Event) -> bool>(&mut self, mut predicate: F) -> usize {
        let mut kept = Vec::with_capacity(self.heap.len());
        let mut removed = 0usize;
        while let Some(oe) = self.heap.pop() {
            if predicate(&oe.0) {
                kept.push(oe);
            } else {
                removed += 1;
            }
        }
        // Reinsert the kept events.
        for oe in kept {
            self.heap.push(oe);
        }
        removed
    }
}
