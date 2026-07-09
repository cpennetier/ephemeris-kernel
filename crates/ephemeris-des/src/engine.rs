//! DES engine — the main simulation event loop.
//!
//! Processes events in time order: Inject → Arrive → Depart → Deliver.
//! Capacity-blocked flow units enter a per-lane overflow queue ordered by
//! (priority_class, enqueued_atu, flow_unit_id) and wake on `WakeQueue`
//! events scheduled at each subsequent slot's depart_atu. All time is
//! i64 ATU. Mass conservation enforced. Deterministic ordering.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use crate::demand::DemandQueue;
use crate::events::{Event, EventKind, EventQueue};
use crate::graph::CSRGraph;
use crate::kpi::KPIAccumulator;
use crate::router::Router;
use crate::schedule::ScheduleStore;
use crate::surfaces::SurfaceStore;
use crate::types::*;

/// Why a flow unit is currently blocked from boarding. Drives the terminal strand
/// attribution: a flow unit whose last block was `Capacity` strands as
/// `CapacityExhausted`, otherwise `ScheduleInfeasible`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockCause {
    None,
    Capacity,
    Outage,
}

/// Additive per-flow unit strand record (OBSERVABILITY ONLY — does not change
/// strand logic or counts). Captures everything the offline feasibility oracle
/// needs to classify a stranded flow unit: id, reason, and its (origin, release,
/// dest, deadline) — all already present on `FlowUnitState` at strand time.
#[derive(Debug, Clone, Copy)]
pub struct StrandRecord {
    pub flow_unit_id: FlowUnitId,
    /// `StrandReason as u8` (ScheduleInfeasible == 1).
    pub reason: u8,
    pub origin_node: NodeId,
    pub appear_atu: Atu,
    pub dest_node: NodeId,
    pub due_atu: Atu,
    /// Node where the flow unit was stranded (where a recoverable flow unit was wasted).
    pub strand_node: NodeId,
    /// `StrandSite as u8` — WHERE in the engine the strand was decided
    /// (RouterNone=0, ChosenLaneNoSlot=1, QueueDeath=2, TopologyPrecheck=3,
    /// NodeOutageArrival=4, DeadlineFinalize=5). Observability only.
    pub site: u8,
    /// GATE-0: total held-but-not-in-transit time (i64 ATU) accumulated up
    /// to the strand decision: closed per-hop holds plus the open hold at
    /// the strand node. Additive observability; the engine MEASURES wait,
    /// it never prices it.
    pub held_atu: Atu,
}

/// Additive per-flow unit delivery record (OBSERVABILITY ONLY — the join the
/// plan-fidelity instrument needs: realized arrival + realized queue wait).
#[derive(Debug, Clone, Copy)]
pub struct DeliveryRecord {
    pub flow_unit_id: FlowUnitId,
    pub delivered_atu: Atu,
    /// Total time spent in overflow queues across all hops.
    pub wait_atu: Atu,
    /// GATE-0: total held-but-not-in-transit time (i64 ATU), summed across
    /// hops as Σ (depart_atu − ready_atu). STRICT SUPERSET of `wait_atu`:
    /// includes schedule dwell (waiting for the next slot with capacity
    /// free), not just overflow-queue time. Additive observability.
    pub held_atu: Atu,
}

/// One realized boarding hop (OBSERVABILITY ONLY; recorded only when the
/// off-by-default hop trace is enabled). The full route of a flow unit is the
/// sequence of its hops ordered by `depart_atu`.
#[derive(Debug, Clone, Copy)]
pub struct HopRecord {
    pub flow_unit_id: FlowUnitId,
    pub csr_lane: LaneIdx,
    pub depart_atu: Atu,
}

/// Per-flow unit tracking state.
struct FlowUnitState {
    /// Origin node (for the unreachable-O/D bottleneck seed).
    origin_node: NodeId,
    dest_node: NodeId,
    size_units: CapacityUnits,
    priority_class: PriorityClass,
    appear_atu: Atu,
    due_atu: Atu,
    /// Accumulated route cost (microdollars).
    route_cost: CostMicros,
    /// Accumulated route CO2 (grams).
    route_co2: Co2Grams,
    /// Total time spent in any overflow queue, summed across hops.
    wait_atu: Atu,
    /// GATE-0: total held-but-not-in-transit time, summed across hops as
    /// Σ (depart_atu − ready_atu). Superset of `wait_atu` (adds schedule
    /// dwell). Measured in the total-ordered event loop; never priced here.
    held_atu: Atu,
    /// Time the unit became ready at its current node (appear_atu at the
    /// origin, the Arrive event time at intermediate nodes). None while in
    /// transit — no hold accrues between a boarding commit and the next
    /// arrival. (A β-mutation cancel of a committed-but-not-departed unit
    /// keeps its already-closed hold: documented undercount on that path
    /// only.)
    ready_atu: Option<Atu>,
    /// True once the flow unit has boarded at least one lane (left its origin).
    /// Drives at_origin vs in_transit on strand.
    departed: bool,
    /// Why the flow unit last failed to board (terminal-attribution input).
    block_cause: BlockCause,
}

/// Entry in a per-lane overflow queue. Min-heap on
/// (priority_class ASC, enqueued_atu ASC, flow_unit_id ASC).
#[derive(Clone)]
struct QueueEntry {
    flow_unit_id: FlowUnitId,
    priority_class: PriorityClass,
    enqueued_atu: Atu,
}

/// Reverse-ordered wrapper so Rust's max-heap behaves as a min-heap.
#[derive(Clone)]
struct OrderedQueueEntry(QueueEntry);

impl PartialEq for OrderedQueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.0.priority_class == other.0.priority_class
            && self.0.enqueued_atu == other.0.enqueued_atu
            && self.0.flow_unit_id == other.0.flow_unit_id
    }
}

impl Eq for OrderedQueueEntry {}

impl Ord for OrderedQueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse for min-heap on (priority, enqueued, flow_unit_id).
        other.0.priority_class.cmp(&self.0.priority_class)
            .then_with(|| other.0.enqueued_atu.cmp(&self.0.enqueued_atu))
            .then_with(|| other.0.flow_unit_id.cmp(&self.0.flow_unit_id))
    }
}

impl PartialOrd for OrderedQueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The Tier 1 Discrete Event Simulator.
pub struct DESEngine {
    pub graph: CSRGraph,
    pub schedules: ScheduleStore,
    pub demand: DemandQueue,
    pub router: Box<dyn Router + Send + Sync>,

    queue: EventQueue,
    current_time_atu: Atu,
    next_event_id: u64,

    // State
    node_loads: Vec<CapacityUnits>,
    node_backlogs: Vec<u32>,
    /// Flow units currently waiting in an overflow queue, indexed by node.
    /// node_queue_depth[h] == sum of all lane_queues entries whose source is h.
    node_queue_depth: Vec<u32>,
    flow_units: HashMap<FlowUnitId, FlowUnitState>,
    /// Forced routes (Tier 3 injection). flow_unit_id → remaining lane path (reversed).
    forced_routes: HashMap<FlowUnitId, Vec<LaneIdx>>,
    /// Slot-PINNED forced routes (Option A commitment semantics):
    /// flow_unit_id → remaining (lane, depart_atu) hops (reversed, pop from
    /// back). A pinned flow unit boards exactly its reserved slot — never
    /// `next_departure` — so a capacity-legal plan realizes exactly.
    forced_routes_pinned: HashMap<FlowUnitId, Vec<(LaneIdx, Atu)>>,
    /// Outstanding (not yet consumed) reserved units per slot, parallel to
    /// `slot_usage`. Unplanned boarding sees `capacity − reserved`; pinned
    /// boarding consumes its reservation. Released on strand of the owner.
    slot_reserved: Vec<CapacityUnits>,
    /// Per-lane overflow queue. Lazily allocated — only lanes that ever
    /// experience capacity refusal get an entry.
    lane_queues: HashMap<LaneIdx, BinaryHeap<OrderedQueueEntry>>,
    /// Lanes that already have a pending WakeQueue event in flight, so we
    /// don't enqueue duplicate wakes when multiple flow units enqueue at once.
    pending_wake: HashMap<LaneIdx, Atu>,
    /// §E shock state — half-open `[t_start, t_end)` outage windows
    /// keyed by lane / node. Linear scan over per-id intervals; expected
    /// shock count is small (<100) so the constant-factor wins over a
    /// bitmap. See `is_lane_blocked`, `is_node_blocked`,
    /// `next_unblocked_atu_for_lane`, `next_unblocked_atu_for_node`.
    lane_outages: HashMap<LaneIdx, Vec<(Atu, Atu)>>,
    node_outages: HashMap<NodeId, Vec<(Atu, Atu)>>,
    /// Capacity booking: slot_idx → used capacity.
    slot_usage: Vec<CapacityUnits>,
    /// Realized per-departure occupancy: slot_idx → COUNT of flow units that boarded
    /// that scheduled departure (a flow unit count, not capacity units). Parallel to
    /// `slot_usage`. Surfaced per (lane_id, depart_atu) for the UI trip-builder's
    /// Batch dot sizing. Conserves: for each lane, the sum over its slots equals
    /// the lane's realized departures (`lane_departs_total`).
    slot_boardings: Vec<u32>,
    /// Non-resetting per-CSR-lane realized departure count (one per Depart event).
    /// The realized "flow" in flow units; the mass-check denominator for
    /// `slot_boardings`. (Surface `lane_departures` is the per-bucket view.)
    lane_departs_total: Vec<u64>,
    /// Additive per-flow unit strand log (observability for the offline
    /// feasibility oracle). Appended in `strand()` alongside the unchanged
    /// aggregate `kpis.record_strand`; never read by routing/strand logic.
    pub strand_log: Vec<StrandRecord>,
    /// Additive per-flow unit delivery log (observability for the plan-fidelity
    /// instrument). Appended on the delivered branch of `handle_arrive`; never
    /// read by routing logic.
    pub delivery_log: Vec<DeliveryRecord>,
    /// Off-by-default realized hop trace (one record per boarding). Enabled via
    /// `set_record_hop_trace`; never read by routing logic.
    pub hop_log: Vec<HopRecord>,
    record_hop_trace: bool,
    /// Reverse of `graph.lane_id_to_csr`: CSR index → original lane id, for the
    /// most-refused-lanes bottleneck seed.
    csr_to_lane_id: Vec<u32>,
    /// Memoized directed reachability for the topology pre-check: origin node →
    /// bitset of nodes reachable on the static graph (ignores time/capacity).
    reach_cache: HashMap<NodeId, Vec<bool>>,

    // Observability
    pub surfaces: SurfaceStore,
    pub kpis: KPIAccumulator,

    // Config
    pub enforce_capacity: bool,
}

impl DESEngine {
    /// Construct engine from pre-built components.
    pub fn new(
        graph: CSRGraph,
        schedules: ScheduleStore,
        demand: DemandQueue,
        router: Box<dyn Router + Send + Sync>,
        bucket_atu: Atu,
    ) -> Self {
        let n = graph.num_nodes as usize;
        let e = graph.num_lanes as usize;
        let total_slots = schedules.total_slots();

        // Build the CSR→original-lane-id reverse map (the forward map is
        // lane_id_to_csr[original] = csr). Real lanes have a unique csr, so the
        // reverse is well-defined for any lane that can be refused.
        let mut csr_to_lane_id = vec![0u32; e];
        for (orig, &csr) in graph.lane_id_to_csr.iter().enumerate() {
            let c = csr as usize;
            if c < e {
                csr_to_lane_id[c] = orig as u32;
            }
        }

        DESEngine {
            surfaces: SurfaceStore::new(n, e, bucket_atu),
            kpis: KPIAccumulator::new(),
            node_loads: vec![0; n],
            node_backlogs: vec![0; n],
            node_queue_depth: vec![0; n],
            flow_units: HashMap::new(),
            forced_routes: HashMap::new(),
            forced_routes_pinned: HashMap::new(),
            slot_reserved: vec![0; total_slots],
            lane_queues: HashMap::new(),
            pending_wake: HashMap::new(),
            lane_outages: HashMap::new(),
            node_outages: HashMap::new(),
            slot_usage: vec![0; total_slots],
            slot_boardings: vec![0; total_slots],
            lane_departs_total: vec![0; e],
            strand_log: Vec::new(),
            delivery_log: Vec::new(),
            hop_log: Vec::new(),
            record_hop_trace: false,
            csr_to_lane_id,
            reach_cache: HashMap::new(),
            queue: EventQueue::new(),
            current_time_atu: 0,
            next_event_id: 0,
            graph,
            schedules,
            demand,
            router,
            enforce_capacity: true,
        }
    }

    fn next_event_id(&mut self) -> u64 {
        let id = self.next_event_id;
        self.next_event_id += 1;
        id
    }

    /// Realized boardings per scheduled departure, keyed for the UI trip-builder.
    ///
    /// Returns `(lane_ids, depart_atus, flow units)` — one entry per departure slot,
    /// where `lane_ids[i]` is the original lane id (== `ScheduleEvent.lane_id`),
    /// `depart_atus[i]` the departure time, and `flow units[i]` the realized count of
    /// flow units that boarded that departure. The UI joins this to its schedule on
    /// `(lane_id, depart_atu)` for Batch dot sizing.
    pub fn boardings_by_departure(&self) -> (Vec<u32>, Vec<i64>, Vec<u32>) {
        let total = self.schedules.total_slots();
        let mut lane_ids = Vec::with_capacity(total);
        let mut depart_atus = Vec::with_capacity(total);
        let mut flow_units = Vec::with_capacity(total);
        let e = self.graph.num_lanes as usize;
        for csr in 0..e {
            let lane = self.csr_to_lane_id[csr];
            let start = self.schedules.sched_offsets[csr] as usize;
            let end = self.schedules.sched_offsets[csr + 1] as usize;
            for slot in start..end {
                lane_ids.push(lane);
                depart_atus.push(self.schedules.departures_atu[slot]);
                flow_units.push(self.slot_boardings[slot]);
            }
        }
        (lane_ids, depart_atus, flow_units)
    }

    /// Mass-check: for every lane, the realized boardings summed over its
    /// departure slots equal the lane's realized departures. True invariant
    /// (every committed boarding fires exactly one Depart); asserted in tests and
    /// a finalize debug-assert so a per-departure count can never silently drift
    /// from the lane's realized flow.
    pub fn boarding_conservation_holds(&self) -> bool {
        let e = self.graph.num_lanes as usize;
        for csr in 0..e {
            let start = self.schedules.sched_offsets[csr] as usize;
            let end = self.schedules.sched_offsets[csr + 1] as usize;
            let sum: u64 = self.slot_boardings[start..end]
                .iter()
                .map(|&b| b as u64)
                .sum();
            if sum != self.lane_departs_total[csr] {
                return false;
            }
        }
        true
    }

    /// OFFLINE feasibility oracle: exact-time earliest arrival from
    /// `(origin, release_atu)` to every node over this run's timetable, capacity
    /// ignored. `out[dest] == Atu::MAX` means unreachable. Read-only; reuses the
    /// production TD-Dijkstra relaxation (see `feasibility_oracle`).
    pub fn earliest_arrival_from(&self, origin: NodeId, release_atu: Atu) -> Vec<Atu> {
        crate::feasibility_oracle::earliest_arrival_all_dests(
            origin, release_atu, &self.graph, &self.schedules,
        )
    }

    // ── Closure — event-queue mutation API ───────────────────────────
    //
    // These functions are intended to be called *between* bucket
    // boundaries (after `step_until` returns, before the next
    // `step_until`). They mutate the event queue so that the closure's
    // hold/defer/fail decisions take observable effect on subsequent
    // DES processing.
    //
    // The hot path (`step_until` → `process_event`) is unchanged.
    // No callbacks, no policy register, no consultation of outside
    // state during dispatch. The DES remains source of truth.

    /// Cancel scheduled events for a flow unit at or after `at_or_after_atu`,
    /// matching the requested event kinds. Returns the count cancelled.
    pub fn cancel_events_for_flow_unit(
        &mut self,
        sid: crate::types::FlowUnitId,
        at_or_after_atu: crate::types::Atu,
        kinds: &[crate::events::EventKind],
    ) -> usize {
        self.queue.retain(|e| {
            !(e.flow_unit_id == sid
                && e.time_atu >= at_or_after_atu
                && kinds.contains(&e.kind))
        })
    }

    /// Cancel ALL future events for a flow unit regardless of kind (used
    /// for Failed-mid-journey terminations). Returns count cancelled.
    pub fn cancel_all_events_for_flow_unit(
        &mut self,
        sid: crate::types::FlowUnitId,
        at_or_after_atu: crate::types::Atu,
    ) -> usize {
        self.queue.retain(|e| {
            !(e.flow_unit_id == sid && e.time_atu >= at_or_after_atu)
        })
    }

    /// Insert a new event at the given time, assigning a fresh
    /// `event_id` for deterministic tiebreaking.
    pub fn insert_event_raw(
        &mut self,
        sid: crate::types::FlowUnitId,
        kind: crate::events::EventKind,
        time_atu: crate::types::Atu,
        location: u32,
    ) {
        let eid = self.next_event_id();
        self.queue.push(crate::events::Event {
            time_atu,
            kind,
            flow_unit_id: sid,
            location,
            event_id: eid,
        });
    }

    /// Test-only: pop the next event from the queue. Used by the Sprint
    /// 4.5 mutation primitives' unit tests.
    #[cfg(test)]
    pub fn pop_event_for_test(&mut self) -> Option<crate::events::Event> {
        self.queue.pop()
    }

    /// Inject all pending demand events up to current_time.
    fn inject_pending_demand(&mut self) {
        // Collect into temp vec to avoid borrow conflict with self
        let due: Vec<_> = self
            .demand
            .drain_until(self.current_time_atu)
            .iter()
            .map(|e| (e.flow_unit_id, e.appear_atu, e.due_atu, e.origin_node_id, e.dest_node_id, e.size_units, e.priority_class))
            .collect();

        for (sid, appear, due_atu, origin, dest, size, prio) in due {
            let eid = self.next_event_id();
            self.kpis.record_inject();
            self.flow_units.insert(
                sid,
                FlowUnitState {
                    origin_node: origin,
                    dest_node: dest,
                    size_units: size,
                    priority_class: prio,
                    appear_atu: appear,
                    due_atu,
                    route_cost: 0,
                    route_co2: 0,
                    wait_atu: 0,
                    held_atu: 0,
                    ready_atu: Some(appear),
                    departed: false,
                    block_cause: BlockCause::None,
                },
            );
            self.queue.push(Event {
                time_atu: appear,
                kind: EventKind::Inject,
                flow_unit_id: sid,
                location: origin,
                event_id: eid,
            });
        }
    }

    /// Run the simulation for the given horizon.
    pub fn run(&mut self, horizon_atu: Atu) {
        let end_time = self.current_time_atu + horizon_atu;

        // Seed initial demand
        self.inject_pending_demand();

        loop {
            // Check if there's more demand to inject before next event
            let next_event_time = self.queue.peek_time();
            let next_demand_time = self.demand.peek_time();

            let next_time = match (next_event_time, next_demand_time) {
                (Some(et), Some(dt)) => Some(et.min(dt)),
                (Some(et), None) => Some(et),
                (None, Some(dt)) => Some(dt),
                (None, None) => None,
            };

            match next_time {
                Some(t) if t <= end_time => {
                    // Inject pending demand if it's due before (or at) the next event
                    if let Some(dt) = next_demand_time {
                        if next_event_time.is_none() || dt <= next_event_time.unwrap() {
                            self.current_time_atu = dt;
                            self.inject_pending_demand();
                        }
                    }

                    // Process event if available
                    if let Some(peek_t) = self.queue.peek_time() {
                        if peek_t <= end_time {
                            let event = self.queue.pop().unwrap();
                            self.current_time_atu = event.time_atu;

                            // Surface emission check
                            self.surfaces.maybe_emit(
                                self.current_time_atu,
                                &self.node_loads,
                                &self.node_backlogs,
                                &self.node_queue_depth,
                            );

                            self.kpis.record_event();
                            self.process_event(event);
                        } else {
                            break;
                        }
                    } else if self.demand.remaining() == 0 {
                        break;
                    }
                }
                _ => break,
            }
        }

        // Sweep any remaining tracked flow_units (DeadlineMissed vs censored
        // in-flight) before emitting the final surface — the histogram needs to
        // be complete. Account against the horizon `end_time`, not the lagging
        // event clock, so batch matches streaming.
        self.finalize_strands(end_time);

        // Final surface snapshot
        self.surfaces.emit_final(
            self.current_time_atu,
            &self.node_loads,
            &self.node_backlogs,
            &self.node_queue_depth,
        );
    }

    /// Run simulation up to a target time (exclusive). Does NOT emit final surface.
    ///
    /// Used for streaming mode: the caller advances by one bucket at a time,
    /// drains the latest surface snapshot, sends it over WebSocket, then calls
    /// step_until again with the next bucket boundary.
    pub fn step_until(&mut self, target_atu: Atu) {
        // Seed initial demand on first call
        if self.current_time_atu == 0 && self.demand.remaining() > 0 {
            self.inject_pending_demand();
        }

        loop {
            let next_event_time = self.queue.peek_time();
            let next_demand_time = self.demand.peek_time();

            let next_time = match (next_event_time, next_demand_time) {
                (Some(et), Some(dt)) => Some(et.min(dt)),
                (Some(et), None) => Some(et),
                (None, Some(dt)) => Some(dt),
                (None, None) => None,
            };

            match next_time {
                Some(t) if t < target_atu => {
                    if let Some(dt) = next_demand_time {
                        if next_event_time.is_none() || dt <= next_event_time.unwrap() {
                            self.current_time_atu = dt;
                            self.inject_pending_demand();
                        }
                    }

                    if let Some(peek_t) = self.queue.peek_time() {
                        if peek_t < target_atu {
                            let event = self.queue.pop().unwrap();
                            self.current_time_atu = event.time_atu;
                            self.surfaces.maybe_emit(
                                self.current_time_atu,
                                &self.node_loads,
                                &self.node_backlogs,
                                &self.node_queue_depth,
                            );
                            self.kpis.record_event();
                            self.process_event(event);
                        } else {
                            break;
                        }
                    } else if self.demand.remaining() == 0 {
                        break;
                    }
                }
                _ => break,
            }
        }

        self.current_time_atu = target_atu;
        // Emit surface at the bucket boundary
        self.surfaces.maybe_emit(
            target_atu,
            &self.node_loads,
            &self.node_backlogs,
            &self.node_queue_depth,
        );
    }

    /// Emit a final surface snapshot (call after the last step_until).
    pub fn finalize_surfaces(&mut self) {
        // Streaming pins `current_time_atu` to the final bucket boundary (the
        // horizon), so it is the correct cutoff here.
        self.finalize_strands(self.current_time_atu);
        self.surfaces.emit_final(
            self.current_time_atu,
            &self.node_loads,
            &self.node_backlogs,
            &self.node_queue_depth,
        );
        // Mass-check: per-departure boardings reconcile with realized lane flow.
        debug_assert!(
            self.boarding_conservation_holds(),
            "per-departure boardings must sum to each lane's realized departures",
        );
    }

    /// Drain all surface snapshots collected so far (moves them out).
    pub fn drain_surfaces(&mut self) -> Vec<crate::surfaces::SurfaceSnapshot> {
        std::mem::take(&mut self.surfaces.snapshots)
    }

    /// Process N events (for debugging / progressive playback).
    pub fn step(&mut self, max_steps: usize) -> usize {
        let mut steps = 0;
        while steps < max_steps {
            // Inject demand if needed
            if let Some(dt) = self.demand.peek_time() {
                let et = self.queue.peek_time().unwrap_or(Atu::MAX);
                if dt <= et {
                    self.current_time_atu = dt;
                    self.inject_pending_demand();
                }
            }

            if let Some(event) = self.queue.pop() {
                self.current_time_atu = event.time_atu;
                self.kpis.record_event();
                self.process_event(event);
                steps += 1;
            } else {
                break;
            }
        }
        steps
    }

    fn process_event(&mut self, event: Event) {
        match event.kind {
            EventKind::Inject => self.handle_inject(event),
            EventKind::Arrive => self.handle_arrive(event),
            EventKind::Depart => self.handle_depart(event),
            EventKind::Deliver => self.handle_deliver(event),
            EventKind::WakeQueue => self.handle_wake_queue(event),
        }
    }

    fn handle_inject(&mut self, event: Event) {
        let sid = event.flow_unit_id;
        let (origin, dest) = match self.flow_units.get(&sid) {
            Some(s) => (s.origin_node, s.dest_node),
            None => return,
        };

        // Topology pre-check: if no directed path origin→dest exists on the
        // static graph (ignoring time/capacity), the flow unit can never deliver —
        // strand it now as TopologyUnreachable rather than letting it fail
        // mid-route, and seed the unreachable-O/D ranking.
        if origin != dest && !self.reachable(origin, dest) {
            self.kpis.record_unreachable_od(origin as u32, dest as u32);
            self.strand(sid, origin, StrandReason::TopologyUnreachable, StrandSite::TopologyPrecheck);
            return;
        }

        // Inject pushes ArrivedAtHub to trigger load + routing logic
        let eid = self.next_event_id();
        self.queue.push(Event {
            time_atu: event.time_atu,
            kind: EventKind::Arrive,
            flow_unit_id: sid,
            location: origin,
            event_id: eid,
        });
    }

    /// Directed reachability origin→dest on the static CSR graph (ignores
    /// time/capacity/outages — topology only). Memoized per origin: the first
    /// query for an origin runs one BFS and caches its reachable bitset.
    fn reachable(&mut self, origin: NodeId, dest: NodeId) -> bool {
        if let Some(set) = self.reach_cache.get(&origin) {
            return set.get(dest as usize).copied().unwrap_or(false);
        }
        let n = self.graph.num_nodes as usize;
        let mut visited = vec![false; n];
        if (origin as usize) < n {
            visited[origin as usize] = true;
        }
        let mut stack = vec![origin];
        while let Some(u) = stack.pop() {
            let ui = u as usize;
            if ui >= n {
                continue;
            }
            let start = self.graph.node_offsets[ui] as usize;
            let end = self.graph.node_offsets[ui + 1] as usize;
            for e in start..end {
                let v = self.graph.lanes[e].target;
                let vi = v as usize;
                if vi < n && !visited[vi] {
                    visited[vi] = true;
                    stack.push(v);
                }
            }
        }
        let result = visited.get(dest as usize).copied().unwrap_or(false);
        self.reach_cache.insert(origin, visited);
        result
    }

    fn handle_arrive(&mut self, event: Event) {
        let node = event.location;
        let sid = event.flow_unit_id;

        // GATE-0 held-time: the unit becomes ready at this node NOW (no
        // hold accrues while in transit). Set BEFORE the outage check so a
        // NodeOutageArrival strand records zero terminal hold at this node.
        if let Some(s) = self.flow_units.get_mut(&sid) {
            s.ready_atu = Some(event.time_atu);
        }

        // §E: node-outage check on arrival. If the destination node of this
        // Arrive is blocked at the event time, strand the flow unit rather
        // than letting it accumulate into node_loads.
        if self.is_node_blocked(node, event.time_atu) {
            // Fold through strand() so per-cause stats + the mass-conservation
            // invariant hold (this path formerly bypassed strand()).
            self.strand(sid, node, StrandReason::NodeOutage, StrandSite::NodeOutageArrival);
            return;
        }

        // Get flow unit state
        let (dest, size, appear, due) = {
            let s = match self.flow_units.get(&sid) {
                Some(s) => s,
                None => return, // FlowUnit already delivered or removed
            };
            (s.dest_node, s.size_units, s.appear_atu, s.due_atu)
        };

        // Mass conservation: increment node load
        self.node_loads[node as usize] += size;

        // Update max node load for KPIs
        self.kpis.update_max_node_load(self.node_loads[node as usize]);

        if node == dest {
            // Delivered!
            self.node_loads[node as usize] -= size;
            let transit = event.time_atu - appear;
            let on_time = event.time_atu <= due;

            let (cost, co2, wait, held) = {
                let s = self.flow_units.get(&sid).unwrap();
                (s.route_cost, s.route_co2, s.wait_atu, s.held_atu)
            };

            // Additive observability — the realized-arrival join for the
            // plan-fidelity instrument. Does not affect any count below.
            self.delivery_log.push(DeliveryRecord {
                flow_unit_id: sid,
                delivered_atu: event.time_atu,
                wait_atu: wait,
                held_atu: held,
            });

            self.kpis.record_delivery(transit, on_time, cost, co2, size);
            self.surfaces
                .accumulator
                .record_delivery(transit, on_time, cost, co2);

            let eid = self.next_event_id();
            self.queue.push(Event {
                time_atu: event.time_atu,
                kind: EventKind::Deliver,
                flow_unit_id: sid,
                location: node,
                event_id: eid,
            });
        } else {
            // Route to next hop
            self.node_backlogs[node as usize] += 1;
            // Record this intermediate-node arrival so the
            // bucket-boundary integration (closure) can re-evaluate the
            // flow unit's policy at this mid-journey waypoint. This is the
            // `Moving → Dwelling` transition.
            self.surfaces
                .accumulator
                .record_intermediate_arrival(sid as u64, node as u32, event.time_atu);
            self.try_board(sid, node);
        }
    }

    fn try_board(&mut self, sid: FlowUnitId, current_node: NodeId) {
        let (size, prio, dest) = match self.flow_units.get(&sid) {
            Some(s) => (s.size_units, s.priority_class, s.dest_node),
            None => return,
        };

        // Slot-PINNED hop (Option A): board exactly the reserved slot,
        // consuming the reservation — never `next_departure`, never the
        // overflow queue. With a capacity-legal plan this cannot fail; the
        // MissedPinnedSlot strand is a bug detector, not an outcome.
        if let Some(route) = self.forced_routes_pinned.get_mut(&sid) {
            let (csr_lane, pinned_depart) = match route.pop() {
                Some(hop) => hop,
                None => {
                    self.forced_routes_pinned.remove(&sid);
                    self.strand(
                        sid, current_node,
                        StrandReason::ScheduleInfeasible, StrandSite::MissedPinnedSlot,
                    );
                    return;
                }
            };
            if route.is_empty() {
                self.forced_routes_pinned.remove(&sid);
            }
            let slot = self
                .schedules
                .next_departure(csr_lane, pinned_depart)
                .filter(|s| s.depart_atu == pinned_depart);
            let slot = match slot {
                Some(s) if self.current_time_atu <= pinned_depart => s,
                _ => {
                    // Late arrival or vanished slot — release what remains
                    // and surface the bug detector.
                    self.release_pinned_reservations(sid, size);
                    self.strand(
                        sid, current_node,
                        StrandReason::ScheduleInfeasible, StrandSite::MissedPinnedSlot,
                    );
                    return;
                }
            };
            let idx = slot.slot_idx as usize;
            if self.enforce_capacity && self.slot_usage[idx] + size > slot.capacity {
                self.release_pinned_reservations(sid, size);
                self.strand(
                    sid, current_node,
                    StrandReason::ScheduleInfeasible, StrandSite::MissedPinnedSlot,
                );
                return;
            }
            self.slot_reserved[idx] = self.slot_reserved[idx].saturating_sub(size);
            self.slot_usage[idx] += size;
            self.slot_boardings[idx] += 1;
            self.schedule_departure(sid, csr_lane, pinned_depart, size);
            return;
        }

        // Forced-route hop (Tier 3 planner) takes priority over the router.
        let csr_lane = if let Some(route) = self.forced_routes.get_mut(&sid) {
            let lane = route.pop();
            if route.is_empty() {
                self.forced_routes.remove(&sid);
            }
            lane
        } else {
            self.router.next_hop(
                current_node, dest, self.current_time_atu, &self.graph, &self.schedules,
            )
        };

        let csr_lane = match csr_lane {
            Some(e) => e,
            None => {
                // Topology is pre-checked at inject, so a None here is a
                // dynamic dead-end (no usable hop now) — a schedule failure.
                self.strand(sid, current_node, StrandReason::ScheduleInfeasible, StrandSite::RouterNone);
                return;
            }
        };

        // If the queue for this lane already has waiters, this flow unit goes
        // behind them — preserves FIFO-within-priority and keeps wake events
        // doing all the boarding work.
        let lane_queue_pending = self.lane_queues
            .get(&csr_lane)
            .is_some_and(|q| !q.is_empty());

        if lane_queue_pending {
            // Behind other waiters = capacity/congestion pressure.
            self.enqueue_for_lane(sid, csr_lane, current_node, prio, BlockCause::Capacity);
            return;
        }

        // §E shock checks. Compute the earliest time the flow unit can board
        // by skipping past any source-node or lane outage active right now.
        let mut search_time = self.current_time_atu;
        if self.is_node_blocked(current_node, search_time) {
            search_time = self.next_unblocked_atu_for_node(current_node, search_time);
        }
        if self.is_lane_blocked(csr_lane, search_time) {
            search_time = self.next_unblocked_atu_for_lane(csr_lane, search_time);
        }

        let slot = match self.schedules.next_departure(csr_lane, search_time) {
            Some(s) => s,
            None => {
                self.strand(sid, current_node, StrandReason::ScheduleInfeasible, StrandSite::ChosenLaneNoSlot);
                return;
            }
        };

        // The slot we found may itself fall inside an lane outage that
        // starts after `search_time`. If so, treat it as not-bookable and
        // enqueue with a wake at the outage end.
        if self.is_lane_blocked(csr_lane, slot.depart_atu) {
            self.enqueue_for_lane(sid, csr_lane, current_node, prio, BlockCause::Outage);
            let resume_at = self.next_unblocked_atu_for_lane(csr_lane, slot.depart_atu);
            self.schedule_wake_for_lane(csr_lane, resume_at);
            return;
        }
        // Same for node outages on the source.
        if self.is_node_blocked(current_node, slot.depart_atu) {
            self.enqueue_for_lane(sid, csr_lane, current_node, prio, BlockCause::Outage);
            let resume_at = self.next_unblocked_atu_for_node(current_node, slot.depart_atu);
            self.schedule_wake_for_lane(csr_lane, resume_at);
            return;
        }
        // §E: also block boarding lanes whose target node will be in
        // outage at the projected arrival time. Otherwise the flow unit
        // would deliver into a closed node and strand on Arrive.
        let lane_target = self.graph.lanes[csr_lane as usize].target;
        let lane_duration = self.graph.lanes[csr_lane as usize].duration_atu;
        let arrival_atu = slot.depart_atu + lane_duration;
        if self.is_node_blocked(lane_target, arrival_atu) {
            self.enqueue_for_lane(sid, csr_lane, current_node, prio, BlockCause::Outage);
            let resume_at = self.next_unblocked_atu_for_node(lane_target, arrival_atu);
            // Wake at the slot whose arrival lands after the dest outage clears.
            let wake_at = resume_at.saturating_sub(lane_duration).max(slot.depart_atu + 1);
            self.schedule_wake_for_lane(csr_lane, wake_at);
            return;
        }

        if !self.enforce_capacity {
            // Infinite capacity bypasses all queue logic.
            self.slot_boardings[slot.slot_idx as usize] += 1;
            self.schedule_departure(sid, csr_lane, slot.depart_atu, size);
            return;
        }

        // Unplanned admission sees capacity − outstanding reservations: the
        // reserved units belong to pinned flow units that have not boarded yet.
        let current_load = self.slot_usage[slot.slot_idx as usize];
        let reserved = self.slot_reserved[slot.slot_idx as usize];
        if current_load + reserved + size <= slot.capacity {
            self.slot_usage[slot.slot_idx as usize] = current_load + size;
            self.slot_boardings[slot.slot_idx as usize] += 1;
            self.schedule_departure(sid, csr_lane, slot.depart_atu, size);
        } else {
            // Slot full — capacity refusal. Record it for the most-refused-lanes
            // ranking, mark the flow unit capacity-blocked, enqueue, and wake at the
            // next slot. If none exists, strand_lane_queue attributes it as
            // CapacityExhausted.
            self.kpis.record_refusal(self.csr_to_lane_id[csr_lane as usize]);
            self.enqueue_for_lane(sid, csr_lane, current_node, prio, BlockCause::Capacity);
            self.schedule_wake_for_lane(csr_lane, slot.depart_atu + 1);
        }
    }

    /// Add a flow unit to the per-lane overflow queue and bump node_queue_depth.
    fn enqueue_for_lane(
        &mut self,
        sid: FlowUnitId,
        csr_lane: LaneIdx,
        current_node: NodeId,
        prio: PriorityClass,
        cause: BlockCause,
    ) {
        // Record WHY this flow unit is blocked — drives terminal strand
        // attribution (Capacity ⇒ CapacityExhausted, else ScheduleInfeasible).
        if let Some(s) = self.flow_units.get_mut(&sid) {
            s.block_cause = cause;
        }
        let entry = QueueEntry {
            flow_unit_id: sid,
            priority_class: prio,
            enqueued_atu: self.current_time_atu,
        };
        self.lane_queues
            .entry(csr_lane)
            .or_insert_with(BinaryHeap::new)
            .push(OrderedQueueEntry(entry));
        self.node_queue_depth[current_node as usize] += 1;
        self.kpis
            .update_max_queue_depth(self.node_queue_depth[current_node as usize]);
    }

    /// Schedule a WakeQueue event for the next slot at or after `after_atu`.
    /// Idempotent per lane — if a wake is already pending at or before the
    /// next slot, skip (avoids a thundering herd of duplicate wakes).
    fn schedule_wake_for_lane(&mut self, csr_lane: LaneIdx, after_atu: Atu) {
        let next_slot = match self.schedules.next_departure(csr_lane, after_atu) {
            Some(s) => s,
            None => {
                // No future slot — strand everyone in this lane's queue now.
                self.strand_lane_queue(csr_lane);
                return;
            }
        };

        if let Some(&pending_at) = self.pending_wake.get(&csr_lane) {
            if pending_at <= next_slot.depart_atu {
                return;
            }
        }
        self.pending_wake.insert(csr_lane, next_slot.depart_atu);

        let eid = self.next_event_id();
        self.queue.push(Event {
            time_atu: next_slot.depart_atu,
            kind: EventKind::WakeQueue,
            flow_unit_id: 0,
            location: csr_lane,
            event_id: eid,
        });
    }

    fn handle_wake_queue(&mut self, event: Event) {
        let csr_lane = event.location as LaneIdx;
        self.pending_wake.remove(&csr_lane);

        let src = self.graph.find_source(csr_lane);
        let lane_meta = self.graph.lanes[csr_lane as usize].clone();

        // §E: skip past any active outages at this wake time.
        let mut search_time = self.current_time_atu;
        if self.is_node_blocked(src, search_time) {
            search_time = self.next_unblocked_atu_for_node(src, search_time);
        }
        if self.is_lane_blocked(csr_lane, search_time) {
            search_time = self.next_unblocked_atu_for_lane(csr_lane, search_time);
        }

        let slot = match self.schedules.next_departure(csr_lane, search_time) {
            Some(s) => s,
            None => {
                self.strand_lane_queue(csr_lane);
                return;
            }
        };

        // If the chosen slot is itself inside an outage, defer the wake.
        if self.is_lane_blocked(csr_lane, slot.depart_atu)
            || self.is_node_blocked(src, slot.depart_atu)
            || self.is_node_blocked(
                lane_meta.target,
                slot.depart_atu + lane_meta.duration_atu,
            )
        {
            // Try to wake at the latest of the three clear times.
            let lane_clear = self.next_unblocked_atu_for_lane(csr_lane, slot.depart_atu);
            let src_clear = self.next_unblocked_atu_for_node(src, slot.depart_atu);
            let dst_clear_arrive = self.next_unblocked_atu_for_node(
                lane_meta.target,
                slot.depart_atu + lane_meta.duration_atu,
            );
            let dst_clear_depart = dst_clear_arrive.saturating_sub(lane_meta.duration_atu);
            let next_try = lane_clear.max(src_clear).max(dst_clear_depart);
            let next_try = next_try.max(slot.depart_atu + 1);
            self.schedule_wake_for_lane(csr_lane, next_try);
            return;
        }

        let mut booked_any = false;

        // Drain the queue against this slot's remaining capacity (minus the
        // outstanding reservations of pinned flow units yet to board).
        loop {
            let mut remaining = slot
                .capacity
                .saturating_sub(self.slot_usage[slot.slot_idx as usize])
                .saturating_sub(self.slot_reserved[slot.slot_idx as usize]);
            if remaining == 0 {
                break;
            }

            // Peek at top entry; only pop if it fits.
            let top = match self.lane_queues.get(&csr_lane).and_then(|q| q.peek().cloned()) {
                Some(e) => e,
                None => break,
            };

            let (size, prio) = match self.flow_units.get(&top.0.flow_unit_id) {
                Some(s) => (s.size_units, s.priority_class),
                None => {
                    // Stale entry — flow unit gone. Drop it and keep draining.
                    if let Some(q) = self.lane_queues.get_mut(&csr_lane) {
                        q.pop();
                    }
                    self.node_queue_depth[src as usize] =
                        self.node_queue_depth[src as usize].saturating_sub(1);
                    continue;
                }
            };

            if size > remaining {
                break;
            }

            // Commit: pop, book the slot, accumulate wait, dispatch Depart.
            let entry = self.lane_queues.get_mut(&csr_lane).unwrap().pop().unwrap();
            let wait = self.current_time_atu - entry.0.enqueued_atu;
            if let Some(state) = self.flow_units.get_mut(&entry.0.flow_unit_id) {
                state.wait_atu += wait;
            }
            self.kpis.record_wait(wait, prio);
            self.surfaces.accumulator.record_wait(wait);

            self.slot_usage[slot.slot_idx as usize] += size;
            self.slot_boardings[slot.slot_idx as usize] += 1;
            remaining -= size;
            self.node_queue_depth[src as usize] =
                self.node_queue_depth[src as usize].saturating_sub(1);
            self.schedule_departure(entry.0.flow_unit_id, csr_lane, slot.depart_atu, size);
            booked_any = true;
            let _ = remaining; // suppress unused warning; consumed by next iter peek
        }

        // If anything still queued, schedule the next wake at the slot AFTER this one.
        let still_pending = self
            .lane_queues
            .get(&csr_lane)
            .is_some_and(|q| !q.is_empty());
        if still_pending {
            self.schedule_wake_for_lane(csr_lane, slot.depart_atu + 1);
        }

        let _ = booked_any;
    }

    /// Mark a flow unit stranded with a reason and remove it from tracking.
    /// Records the strand (with at_origin/wait/transit stats) ONLY if the
    /// flow unit was still tracked — so a flow unit is counted at most once, which
    /// the mass-conservation invariant relies on.
    fn strand(&mut self, sid: FlowUnitId, node: NodeId, reason: StrandReason, site: StrandSite) {
        if let Some(s) = self.flow_units.remove(&sid) {
            // Mass-conservation: if the flow unit had been counted as load on
            // a node, release it. (Inject hasn't run handle_arrive yet for
            // brand-new flow units, so this saturates safely.)
            self.node_loads[node as usize] =
                self.node_loads[node as usize].saturating_sub(s.size_units);
            self.node_backlogs[node as usize] =
                self.node_backlogs[node as usize].saturating_sub(1);
            let at_origin = !s.departed;
            let wait = s.wait_atu.max(0);
            let transit = (self.current_time_atu - s.appear_atu).max(0);
            self.kpis.record_strand(reason, at_origin, wait, transit);
            // GATE-0 held-time: closed per-hop holds plus the open hold at
            // the strand node (zero if the unit was in transit).
            let held = s.held_atu
                + s.ready_atu
                    .map(|r| (self.current_time_atu - r).max(0))
                    .unwrap_or(0);
            // Additive observability — does not affect the count above.
            self.strand_log.push(StrandRecord {
                flow_unit_id: sid,
                reason: reason as u8,
                origin_node: s.origin_node,
                appear_atu: s.appear_atu,
                dest_node: s.dest_node,
                due_atu: s.due_atu,
                strand_node: node,
                site: site as u8,
                held_atu: held,
            });
            // Release any outstanding pinned reservations so unplanned
            // traffic regains the capacity (idempotent if already released).
            self.release_pinned_reservations(sid, s.size_units);
        }
        self.forced_routes.remove(&sid);
        self.forced_routes_pinned.remove(&sid);
    }

    /// Strand every flow unit currently in `lane_queues[csr_lane]`, attributing
    /// each to CapacityExhausted (if its last block was a capacity refusal) or
    /// ScheduleInfeasible (otherwise).
    fn strand_lane_queue(&mut self, csr_lane: LaneIdx) {
        let src = self.graph.find_source(csr_lane);
        let entries: Vec<OrderedQueueEntry> = self
            .lane_queues
            .remove(&csr_lane)
            .map(|q| q.into_sorted_vec())
            .unwrap_or_default();
        for entry in entries {
            self.node_queue_depth[src as usize] =
                self.node_queue_depth[src as usize].saturating_sub(1);
            let sid = entry.0.flow_unit_id;
            let reason = match self.flow_units.get(&sid).map(|s| s.block_cause) {
                Some(BlockCause::Capacity) => StrandReason::CapacityExhausted,
                _ => StrandReason::ScheduleInfeasible,
            };
            self.strand(sid, src, reason, StrandSite::QueueDeath);
        }
    }

    /// At end-of-simulation, split every still-tracked flow unit by its deadline:
    /// due already passed ⇒ DeadlineMissed (a strand); deadline still in the
    /// future ⇒ in_flight_at_cutoff (censored, NOT a strand). Queued flow units are
    /// also in `flow_units`, so iterating it covers both queued and in-flight.
    /// Split every still-tracked flow unit at end-of-run by its deadline relative
    /// to `cutoff_atu` (the simulation horizon): due already passed ⇒
    /// `DeadlineMissed` (a strand); deadline still in the future ⇒
    /// `in_flight_at_cutoff` (censored, NOT a strand). Queued flow units are also
    /// in `flow_units`, so iterating it covers queued and in-flight alike.
    ///
    /// `cutoff_atu` is the horizon, NOT `current_time_atu`: in batch mode the
    /// clock lags at the last processed event, while streaming pins it to the
    /// final bucket boundary. Using the horizon makes the per-cause time-in-
    /// system stats and the deadline split identical across batch/stream.
    fn finalize_strands(&mut self, cutoff_atu: Atu) {
        // `strand()` and the in-flight path read `current_time_atu` for their
        // time-in-system stats (and `strand()` releases node load, which feeds
        // the final surface). Pin the clock to the horizon for the duration of
        // the accounting, then restore it so the caller's final surface
        // snapshot is emitted from the original (mode-specific) clock and stays
        // byte-identical.
        let saved_now = self.current_time_atu;
        self.current_time_atu = cutoff_atu;

        // Iterate in flow unit-id order, NOT HashMap order. The per-cause
        // accumulators sum squares in f64 (for variance/std), so accumulation
        // order is numerically significant; HashMap iteration is randomized per
        // run, which would make `std_*` differ bit-for-bit across otherwise
        // identical runs and between batch/stream modes. Sorting restores
        // run-to-run and cross-mode determinism.
        let mut remaining: Vec<FlowUnitId> = self.flow_units.keys().copied().collect();
        remaining.sort_unstable();
        for sid in remaining {
            let due = match self.flow_units.get(&sid) {
                Some(s) => s.due_atu,
                None => continue,
            };
            if due < cutoff_atu {
                self.strand(sid, 0, StrandReason::DeadlineMissed, StrandSite::DeadlineFinalize);
            } else if let Some(s) = self.flow_units.remove(&sid) {
                let at_origin = !s.departed;
                let wait = s.wait_atu.max(0);
                let transit = (cutoff_atu - s.appear_atu).max(0);
                self.kpis.record_in_flight_at_cutoff(at_origin, wait, transit);
                self.forced_routes.remove(&sid);
                self.release_pinned_reservations(sid, s.size_units);
            }
        }
        self.current_time_atu = saved_now;
        // Queue entries now reference removed flow_units; drop them.
        self.lane_queues.clear();
    }

    fn schedule_departure(
        &mut self,
        sid: FlowUnitId,
        csr_lane: LaneIdx,
        depart_atu: Atu,
        _size: CapacityUnits,
    ) {
        // The flow unit has committed to boarding an lane — it has left its origin.
        if let Some(s) = self.flow_units.get_mut(&sid) {
            s.departed = true;
            // GATE-0 held-time: close this node's hold interval at the
            // committed departure; None afterwards — no hold accrues in
            // transit. Covers dwell AND queue time in one measure
            // (ready ≤ enqueue ≤ commit ≤ depart).
            if let Some(r) = s.ready_atu.take() {
                s.held_atu += (depart_atu - r).max(0);
            }
        }
        // Off-by-default observability: one record per realized boarding.
        if self.record_hop_trace {
            self.hop_log.push(HopRecord { flow_unit_id: sid, csr_lane, depart_atu });
        }
        let eid = self.next_event_id();
        self.queue.push(Event {
            time_atu: depart_atu,
            kind: EventKind::Depart,
            flow_unit_id: sid,
            location: csr_lane,
            event_id: eid,
        });
    }

    fn handle_depart(&mut self, event: Event) {
        let csr_lane = event.location;
        let sid = event.flow_unit_id;
        let lane_meta = &self.graph.lanes[csr_lane as usize];
        let target = lane_meta.target;
        let arrival_atu = event.time_atu + lane_meta.duration_atu;

        // Mass conservation: decrement load at source node
        let source = self.graph.find_source(csr_lane);
        if let Some(s) = self.flow_units.get(&sid) {
            let size = s.size_units;
            self.node_loads[source as usize] = self.node_loads[source as usize].saturating_sub(size);
            self.node_backlogs[source as usize] =
                self.node_backlogs[source as usize].saturating_sub(1);

            // Record lane flow and accumulate route cost/CO2
            self.surfaces
                .accumulator
                .record_departure(csr_lane as usize, size);
            // Non-resetting realized departure count (mass-check denominator).
            self.lane_departs_total[csr_lane as usize] += 1;
        }

        // Accumulate cost and CO2 on the flow unit
        if let Some(s) = self.flow_units.get_mut(&sid) {
            s.route_cost += lane_meta.cost_micros;
            s.route_co2 += lane_meta.co2_grams;
        }

        // Schedule arrival at destination node
        let eid = self.next_event_id();
        self.queue.push(Event {
            time_atu: arrival_atu,
            kind: EventKind::Arrive,
            flow_unit_id: sid,
            location: target,
            event_id: eid,
        });
    }

    fn handle_deliver(&mut self, event: Event) {
        self.flow_units.remove(&event.flow_unit_id);
        self.forced_routes.remove(&event.flow_unit_id);
        // Pinned route is fully consumed by delivery (last hop popped it);
        // plain remove for hygiene.
        self.forced_routes_pinned.remove(&event.flow_unit_id);
    }

    // ── Accessors ──

    pub fn current_time_atu(&self) -> Atu {
        self.current_time_atu
    }

    pub fn node_loads(&self) -> &[CapacityUnits] {
        &self.node_loads
    }

    pub fn node_queue_depth(&self) -> &[u32] {
        &self.node_queue_depth
    }

    pub fn total_queued(&self) -> u64 {
        self.node_queue_depth.iter().map(|&d| d as u64).sum()
    }

    // ── §E shock APIs ──────────────────────────────────────────────

    /// Install lane outage windows. Replaces any previously set windows.
    /// `intervals` are `(csr_lane, t_start_atu, t_end_atu)` half-open.
    pub fn set_lane_outages(&mut self, intervals: Vec<(LaneIdx, Atu, Atu)>) {
        self.lane_outages.clear();
        for (lane, start, end) in intervals {
            if end > start {
                self.lane_outages.entry(lane).or_default().push((start, end));
            }
        }
        for v in self.lane_outages.values_mut() {
            v.sort();
        }
    }

    /// Install node outage windows. Replaces any previously set windows.
    pub fn set_node_outages(&mut self, intervals: Vec<(NodeId, Atu, Atu)>) {
        self.node_outages.clear();
        for (node, start, end) in intervals {
            if end > start {
                self.node_outages.entry(node).or_default().push((start, end));
            }
        }
        for v in self.node_outages.values_mut() {
            v.sort();
        }
    }

    /// Inject additional demand events into the queue (§E demand-modulation
    /// shock). Events with `appear_atu < current_time` will fire on the
    /// next `inject_pending_demand` sweep — no event is dropped.
    pub fn inject_demand_events(&mut self, events: Vec<crate::demand::DemandEvent>) {
        self.demand.append_events(events);
    }

    fn is_lane_blocked(&self, lane: LaneIdx, atu: Atu) -> bool {
        match self.lane_outages.get(&lane) {
            Some(v) => v.iter().any(|(s, e)| atu >= *s && atu < *e),
            None => false,
        }
    }

    fn is_node_blocked(&self, node: NodeId, atu: Atu) -> bool {
        match self.node_outages.get(&node) {
            Some(v) => v.iter().any(|(s, e)| atu >= *s && atu < *e),
            None => false,
        }
    }

    /// Smallest `t >= after` at which `lane` is *not* in any outage.
    /// Walks the (sorted, non-overlapping) intervals. Returns `after` if
    /// no outages cover it.
    fn next_unblocked_atu_for_lane(&self, lane: LaneIdx, after: Atu) -> Atu {
        let intervals = match self.lane_outages.get(&lane) {
            Some(v) => v,
            None => return after,
        };
        let mut t = after;
        for &(s, e) in intervals {
            if t >= s && t < e {
                t = e;
            } else if t < s {
                break;
            }
        }
        t
    }

    fn next_unblocked_atu_for_node(&self, node: NodeId, after: Atu) -> Atu {
        let intervals = match self.node_outages.get(&node) {
            Some(v) => v,
            None => return after,
        };
        let mut t = after;
        for &(s, e) in intervals {
            if t >= s && t < e {
                t = e;
            } else if t < s {
                break;
            }
        }
        t
    }

    pub fn total_delivered(&self) -> u64 {
        self.kpis.total_delivered
    }

    pub fn events_in_queue(&self) -> usize {
        self.queue.len()
    }

    /// Insert forced routes for Tier 3 planner injection.
    pub fn insert_forced_route(&mut self, flow_unit_id: FlowUnitId, path: Vec<LaneIdx>) {
        self.forced_routes.insert(flow_unit_id, path);
    }

    /// Enable/disable the per-boarding hop trace (off by default; observability
    /// only — has no effect on routing, strand logic, or any count).
    pub fn set_record_hop_trace(&mut self, enabled: bool) {
        self.record_hop_trace = enabled;
    }

    /// Install slot-PINNED forced routes and charge their reservations
    /// (Option A commitment semantics). `routes` carries FORWARD-order
    /// `(lane, depart_atu)` hops per flow unit.
    ///
    /// Engine-side legality is enforced at install time — the call errors
    /// (installing nothing) if any hop's `(lane, depart_atu)` is not an exact
    /// timetable slot, a flow unit is unknown or duplicated, or any slot's
    /// `usage + reserved` would exceed capacity. With a legal plan, pinned
    /// execution realizes it exactly; the `MissedPinnedSlot` strand site is a
    /// bug detector that must stay at zero.
    ///
    /// Scope note: pinned boarding is not §E-outage-aware (this campaign's
    /// scenarios carry no outages); a pinned slot inside a future outage
    /// window will surface as a `MissedPinnedSlot`-adjacent divergence, not
    /// a deferral.
    pub fn set_pinned_routes(
        &mut self,
        routes: Vec<(FlowUnitId, Vec<(LaneIdx, Atu)>)>,
    ) -> Result<usize, String> {
        // Resolve flow unit sizes from the (not yet drained) demand table.
        let mut size_of: HashMap<FlowUnitId, CapacityUnits> = HashMap::new();
        for ev in self.demand.all_events() {
            size_of.insert(ev.flow_unit_id, ev.size_units);
        }

        // Validate everything before mutating anything (atomic install).
        let mut charges: Vec<(usize, CapacityUnits)> = Vec::new();
        let mut staged: Vec<(FlowUnitId, Vec<(LaneIdx, Atu)>)> = Vec::new();
        let mut staged_ids: std::collections::HashSet<FlowUnitId> =
            std::collections::HashSet::new();
        let mut planned_reserved = self.slot_reserved.clone();
        for (sid, path) in routes {
            // O(1) duplicate check — a linear scan over `staged` here was
            // O(n²) and dominated 1M installs (measured 116-202 s).
            if self.forced_routes_pinned.contains_key(&sid) || !staged_ids.insert(sid) {
                return Err(format!("duplicate pinned route for flow unit {sid}"));
            }
            let size = *size_of
                .get(&sid)
                .ok_or_else(|| format!("unknown flow unit {sid} in pinned routes"))?;
            for &(lane, depart) in &path {
                let slot = self
                    .schedules
                    .next_departure(lane, depart)
                    .filter(|s| s.depart_atu == depart)
                    .ok_or_else(|| {
                        format!(
                            "pinned slot (lane {lane}, depart {depart}) not in timetable"
                        )
                    })?;
                let idx = slot.slot_idx as usize;
                if self.slot_usage[idx] + planned_reserved[idx] + size > slot.capacity {
                    return Err(format!(
                        "over-reservation on lane {lane} depart {depart}: \
                         usage {} + reserved {} + size {size} > capacity {}",
                        self.slot_usage[idx], planned_reserved[idx], slot.capacity
                    ));
                }
                planned_reserved[idx] += size;
                charges.push((idx, size));
            }
            let mut rev = path;
            rev.reverse(); // engine pops from the back
            staged.push((sid, rev));
        }
        for (idx, size) in charges {
            self.slot_reserved[idx] += size;
        }
        let n = staged.len();
        for (sid, rev) in staged {
            self.forced_routes_pinned.insert(sid, rev);
        }
        Ok(n)
    }

    /// Cancel the pinned routes of `sids`, releasing the outstanding
    /// reservations of their REMAINING hops (C2: the disruption-invalidation
    /// release — physics, not choice). Already-boarded hops are untouched;
    /// flow_units without a pinned route are skipped. Sizes resolve from the
    /// live flow unit map (mid-journey flow units) or the demand table (flow units
    /// not yet injected). Returns the number of routes cancelled. ADDITIVE:
    /// no routing or strand behavior changes; the freed capacity simply
    /// becomes visible to unplanned boarding and future re-pinning.
    pub fn cancel_pinned_routes(&mut self, sids: &[FlowUnitId]) -> usize {
        let mut demand_size: Option<HashMap<FlowUnitId, CapacityUnits>> = None;
        let mut n = 0usize;
        for &sid in sids {
            if !self.forced_routes_pinned.contains_key(&sid) {
                continue;
            }
            let size = match self.flow_units.get(&sid) {
                Some(s) => s.size_units,
                None => {
                    let map = demand_size.get_or_insert_with(|| {
                        self.demand
                            .all_events()
                            .iter()
                            .map(|e| (e.flow_unit_id, e.size_units))
                            .collect()
                    });
                    match map.get(&sid) {
                        Some(&sz) => sz,
                        None => continue, // delivered/stranded: nothing pinned
                    }
                }
            };
            self.release_pinned_reservations(sid, size);
            n += 1;
        }
        n
    }

    /// Per-slot capacity state (C2 observability): parallel arrays over the
    /// global slot index — original lane_id, depart_atu, capacity, usage,
    /// outstanding reserved. Read-only; the live-loop harness mirrors
    /// residuals (capacity − usage − reserved) from this.
    pub fn slot_state(&self) -> (Vec<u32>, Vec<Atu>, Vec<u32>, Vec<u32>, Vec<u32>) {
        let total = self.schedules.total_slots();
        let mut lane_ids = Vec::with_capacity(total);
        let mut departs = Vec::with_capacity(total);
        let mut caps = Vec::with_capacity(total);
        for csr in 0..self.graph.num_lanes as usize {
            let start = self.schedules.sched_offsets[csr] as usize;
            let end = self.schedules.sched_offsets[csr + 1] as usize;
            for s in start..end {
                lane_ids.push(self.csr_to_lane_id[csr]);
                departs.push(self.schedules.departures_atu[s]);
                caps.push(self.schedules.capacities[s]);
            }
        }
        (lane_ids, departs, caps, self.slot_usage.clone(), self.slot_reserved.clone())
    }

    /// Release the outstanding reservations of a pinned flow unit's REMAINING
    /// hops (called when it strands or is censored mid-plan, so unplanned
    /// traffic regains the capacity).
    fn release_pinned_reservations(&mut self, sid: FlowUnitId, size: CapacityUnits) {
        if let Some(route) = self.forced_routes_pinned.remove(&sid) {
            for (lane, depart) in route {
                if let Some(slot) = self
                    .schedules
                    .next_departure(lane, depart)
                    .filter(|s| s.depart_atu == depart)
                {
                    let idx = slot.slot_idx as usize;
                    self.slot_reserved[idx] = self.slot_reserved[idx].saturating_sub(size);
                }
            }
        }
    }
}
