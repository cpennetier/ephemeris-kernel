//! KPI aggregation — final statistics after simulation.

use std::collections::HashMap;

use serde::Serialize;

use crate::types::{Atu, CapacityUnits, Co2Grams, CostMicros, PriorityClass, StrandReason};

/// Per-cause accumulator (raw sums). `finalize` derives count/%/mean/var/std.
///
/// Variance uses Σx² − (Σx)²/n; fine at this scale. Welford's online algorithm
/// is the numerically-stable alternative if these sums ever grow large enough
/// for catastrophic cancellation.
#[derive(Debug, Clone, Default)]
pub struct StrandCauseAccum {
    pub count: u64,
    pub at_origin: u64,
    pub in_transit: u64,
    pub sum_wait: i64,
    pub sum_wait_sq: f64,
    pub sum_transit: i64,
    pub sum_transit_sq: f64,
}

impl StrandCauseAccum {
    fn record(&mut self, at_origin: bool, wait_atu: Atu, transit_atu: Atu) {
        self.count += 1;
        if at_origin {
            self.at_origin += 1;
        } else {
            self.in_transit += 1;
        }
        self.sum_wait += wait_atu;
        self.sum_wait_sq += (wait_atu as f64) * (wait_atu as f64);
        self.sum_transit += transit_atu;
        self.sum_transit_sq += (transit_atu as f64) * (transit_atu as f64);
    }

    /// Derive display stats. `denom` is the set the % is taken against.
    fn stats(&self, denom: u64) -> StrandCauseStats {
        let n = self.count;
        let nf = n as f64;
        let mean_wait = if n > 0 { self.sum_wait / n as i64 } else { 0 };
        let mean_transit = if n > 0 { self.sum_transit / n as i64 } else { 0 };
        let var_wait = if n > 0 {
            (self.sum_wait_sq / nf) - (self.sum_wait as f64 / nf).powi(2)
        } else {
            0.0
        };
        let var_transit = if n > 0 {
            (self.sum_transit_sq / nf) - (self.sum_transit as f64 / nf).powi(2)
        } else {
            0.0
        };
        StrandCauseStats {
            count: n,
            pct: if denom > 0 { nf / denom as f64 } else { 0.0 },
            at_origin: self.at_origin,
            in_transit: self.in_transit,
            mean_wait_atu: mean_wait,
            std_wait_atu: var_wait.max(0.0).sqrt(),
            mean_transit_atu: mean_transit,
            std_transit_atu: var_transit.max(0.0).sqrt(),
        }
    }
}

/// Derived per-cause stats (JSON output).
#[derive(Debug, Clone, Default, Serialize)]
pub struct StrandCauseStats {
    pub count: u64,
    pub pct: f64,
    pub at_origin: u64,
    pub in_transit: u64,
    pub mean_wait_atu: Atu,
    pub std_wait_atu: f64,
    pub mean_transit_atu: Atu,
    pub std_transit_atu: f64,
}

/// Per-cause strand accumulators (internal). One slot per `StrandReason`.
#[derive(Debug, Clone, Default)]
pub struct StrandReasonHistogram {
    pub topology_unreachable: StrandCauseAccum,
    pub schedule_infeasible: StrandCauseAccum,
    pub capacity_exhausted: StrandCauseAccum,
    pub deadline_missed: StrandCauseAccum,
    pub lane_outage: StrandCauseAccum,
    pub node_outage: StrandCauseAccum,
}

impl StrandReasonHistogram {
    pub fn total(&self) -> u64 {
        self.topology_unreachable.count
            + self.schedule_infeasible.count
            + self.capacity_exhausted.count
            + self.deadline_missed.count
            + self.lane_outage.count
            + self.node_outage.count
    }

    pub fn record(&mut self, reason: StrandReason, at_origin: bool, wait_atu: Atu, transit_atu: Atu) {
        let slot = match reason {
            StrandReason::TopologyUnreachable => &mut self.topology_unreachable,
            StrandReason::ScheduleInfeasible => &mut self.schedule_infeasible,
            StrandReason::CapacityExhausted => &mut self.capacity_exhausted,
            StrandReason::DeadlineMissed => &mut self.deadline_missed,
            StrandReason::LaneOutage => &mut self.lane_outage,
            StrandReason::NodeOutage => &mut self.node_outage,
        };
        slot.record(at_origin, wait_atu, transit_atu);
    }

    fn stats(&self, total: u64) -> StrandReasonStats {
        StrandReasonStats {
            topology_unreachable: self.topology_unreachable.stats(total),
            schedule_infeasible: self.schedule_infeasible.stats(total),
            capacity_exhausted: self.capacity_exhausted.stats(total),
            deadline_missed: self.deadline_missed.stats(total),
            lane_outage: self.lane_outage.stats(total),
            node_outage: self.node_outage.stats(total),
        }
    }
}

/// Derived per-cause strand stats (JSON output).
#[derive(Debug, Clone, Default, Serialize)]
pub struct StrandReasonStats {
    pub topology_unreachable: StrandCauseStats,
    pub schedule_infeasible: StrandCauseStats,
    pub capacity_exhausted: StrandCauseStats,
    pub deadline_missed: StrandCauseStats,
    pub lane_outage: StrandCauseStats,
    pub node_outage: StrandCauseStats,
}

/// Bottleneck seed: an lane ranked by how many times it refused a flow unit for
/// capacity.
#[derive(Debug, Clone, Serialize)]
pub struct RefusedEdge {
    pub lane_id: u32,
    pub refusals: u64,
}

/// Bottleneck seed: an origin→destination pair with no topological path.
#[derive(Debug, Clone, Serialize)]
pub struct UnreachableOd {
    pub origin_node: u32,
    pub dest_node: u32,
    pub count: u64,
}

/// How many top entries to emit in each bottleneck ranking.
const BOTTLENECK_TOP_K: usize = 20;

/// Wait-time distribution by priority class. Mean computed at finalize.
#[derive(Debug, Clone, Default, Serialize)]
pub struct WaitByPriority {
    pub vip_count: u64,
    pub vip_sum_atu: i64,
    pub express_count: u64,
    pub express_sum_atu: i64,
    pub standard_count: u64,
    pub standard_sum_atu: i64,
    pub economy_count: u64,
    pub economy_sum_atu: i64,
}

impl WaitByPriority {
    pub fn record(&mut self, priority: PriorityClass, wait_atu: Atu) {
        match priority {
            0 => { self.vip_count += 1; self.vip_sum_atu += wait_atu; }
            1 => { self.express_count += 1; self.express_sum_atu += wait_atu; }
            2 => { self.standard_count += 1; self.standard_sum_atu += wait_atu; }
            _ => { self.economy_count += 1; self.economy_sum_atu += wait_atu; }
        }
    }
}

/// Aggregate KPIs for a completed simulation run.
#[derive(Debug, Clone, Serialize)]
pub struct SimulationKPIs {
    pub total_flow_units: u64,
    pub total_injected: u64,
    pub total_delivered: u64,
    pub total_late: u64,
    pub total_on_time: u64,
    pub on_time_pct: f64,
    pub mean_transit_atu: Atu,
    pub total_cost_micros: CostMicros,
    pub total_co2_grams: Co2Grams,
    pub total_capacity_units_delivered: u64,
    pub stranded_flow_units: u64,
    pub strand_reasons: StrandReasonStats,
    /// Flow units still in flight when the horizon ended with their deadline NOT
    /// yet passed — censored, NOT failed. Excluded from `strand_reasons`.
    pub in_flight_at_cutoff: StrandCauseStats,
    pub most_refused_lanes: Vec<RefusedEdge>,
    pub unreachable_od_pairs: Vec<UnreachableOd>,
    pub max_node_load: CapacityUnits,
    pub max_queue_depth: u32,
    pub mean_wait_atu: Atu,
    pub max_wait_atu: Atu,
    pub waits_with_queueing: u64,
    pub wait_by_priority: WaitByPriority,
    pub simulation_wall_time_ms: u64,
    pub events_processed: u64,
}

/// Builder that accumulates delivery events and produces final KPIs.
pub struct KPIAccumulator {
    pub total_injected: u64,
    pub total_delivered: u64,
    pub total_late: u64,
    pub total_on_time: u64,
    pub sum_transit_atu: i64,
    pub total_cost_micros: CostMicros,
    pub total_co2_grams: Co2Grams,
    pub total_capacity_delivered: u64,
    pub max_node_load: CapacityUnits,
    pub events_processed: u64,
    pub strand_reasons: StrandReasonHistogram,
    /// Censored-at-horizon set (deadline not yet passed). Not a strand cause.
    pub in_flight: StrandCauseAccum,
    /// Capacity refusals per ORIGINAL lane id (bottleneck seed).
    pub refused_by_lane: HashMap<u32, u64>,
    /// Topologically-unreachable origin→dest pairs (bottleneck seed).
    pub unreachable_od: HashMap<(u32, u32), u64>,
    pub max_queue_depth: u32,
    pub sum_wait_atu: i64,
    pub max_wait_atu: i64,
    pub waits_with_queueing: u64,
    pub wait_by_priority: WaitByPriority,
}

impl KPIAccumulator {
    pub fn new() -> Self {
        KPIAccumulator {
            total_injected: 0,
            total_delivered: 0,
            total_late: 0,
            total_on_time: 0,
            sum_transit_atu: 0,
            total_cost_micros: 0,
            total_co2_grams: 0,
            total_capacity_delivered: 0,
            max_node_load: 0,
            events_processed: 0,
            strand_reasons: StrandReasonHistogram::default(),
            in_flight: StrandCauseAccum::default(),
            refused_by_lane: HashMap::new(),
            unreachable_od: HashMap::new(),
            max_queue_depth: 0,
            sum_wait_atu: 0,
            max_wait_atu: 0,
            waits_with_queueing: 0,
            wait_by_priority: WaitByPriority::default(),
        }
    }

    /// Record a strand with its per-cause stats inputs.
    pub fn record_strand(&mut self, reason: StrandReason, at_origin: bool, wait_atu: Atu, transit_atu: Atu) {
        self.strand_reasons.record(reason, at_origin, wait_atu, transit_atu);
    }

    /// Record a flow unit censored at the horizon (still in flight, deadline not
    /// yet passed). Counted separately from strands.
    pub fn record_in_flight_at_cutoff(&mut self, at_origin: bool, wait_atu: Atu, transit_atu: Atu) {
        self.in_flight.record(at_origin, wait_atu, transit_atu);
    }

    /// Record a capacity refusal on `lane_id` (original id) for pressure ranking.
    pub fn record_refusal(&mut self, lane_id: u32) {
        *self.refused_by_lane.entry(lane_id).or_insert(0) += 1;
    }

    /// Record a topologically-unreachable origin→dest pair.
    pub fn record_unreachable_od(&mut self, origin: u32, dest: u32) {
        *self.unreachable_od.entry((origin, dest)).or_insert(0) += 1;
    }

    pub fn record_wait(&mut self, wait_atu: Atu, priority: PriorityClass) {
        if wait_atu <= 0 {
            return;
        }
        self.sum_wait_atu += wait_atu;
        if wait_atu > self.max_wait_atu {
            self.max_wait_atu = wait_atu;
        }
        self.waits_with_queueing += 1;
        self.wait_by_priority.record(priority, wait_atu);
    }

    pub fn update_max_queue_depth(&mut self, depth: u32) {
        if depth > self.max_queue_depth {
            self.max_queue_depth = depth;
        }
    }

    pub fn record_inject(&mut self) {
        self.total_injected += 1;
    }

    pub fn record_delivery(
        &mut self,
        transit_atu: Atu,
        on_time: bool,
        cost_micros: CostMicros,
        co2_grams: Co2Grams,
        size_units: CapacityUnits,
    ) {
        self.total_delivered += 1;
        self.sum_transit_atu += transit_atu;
        self.total_cost_micros += cost_micros;
        self.total_co2_grams += co2_grams;
        self.total_capacity_delivered += size_units as u64;
        if on_time {
            self.total_on_time += 1;
        } else {
            self.total_late += 1;
        }
    }

    pub fn update_max_node_load(&mut self, load: CapacityUnits) {
        if load > self.max_node_load {
            self.max_node_load = load;
        }
    }

    pub fn record_event(&mut self) {
        self.events_processed += 1;
    }

    fn top_refused_lanes(&self) -> Vec<RefusedEdge> {
        let mut v: Vec<RefusedEdge> = self
            .refused_by_lane
            .iter()
            .map(|(&lane_id, &refusals)| RefusedEdge { lane_id, refusals })
            .collect();
        // Descending by refusals, then lane_id for determinism.
        v.sort_by(|a, b| b.refusals.cmp(&a.refusals).then(a.lane_id.cmp(&b.lane_id)));
        v.truncate(BOTTLENECK_TOP_K);
        v
    }

    fn unreachable_pairs(&self) -> Vec<UnreachableOd> {
        let mut v: Vec<UnreachableOd> = self
            .unreachable_od
            .iter()
            .map(|(&(origin_node, dest_node), &count)| UnreachableOd { origin_node, dest_node, count })
            .collect();
        v.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then(a.origin_node.cmp(&b.origin_node))
                .then(a.dest_node.cmp(&b.dest_node))
        });
        v.truncate(BOTTLENECK_TOP_K);
        v
    }

    /// Produce final KPIs. Reconciliation: `stranded = strand_reasons.total()`
    /// and the mass-conservation invariant injected = delivered + stranded +
    /// in_flight_at_cutoff is asserted (debug builds).
    pub fn finalize(
        &self,
        total_demand: u64,
        wall_time_ms: u64,
    ) -> SimulationKPIs {
        let mean_transit = if self.total_delivered > 0 {
            self.sum_transit_atu / self.total_delivered as i64
        } else {
            0
        };

        let on_time_pct = if self.total_delivered > 0 {
            self.total_on_time as f64 / self.total_delivered as f64
        } else {
            0.0
        };

        // Authoritative: stranded is the sum of per-cause strand counts (no
        // longer demand − delivered).
        let stranded = self.strand_reasons.total();
        let in_flight_count = self.in_flight.count;

        // Mass conservation: every injected flow unit is delivered, stranded, or
        // censored in flight. (debug-only; a dedicated test asserts it too.)
        debug_assert_eq!(
            self.total_injected,
            self.total_delivered + stranded + in_flight_count,
            "mass conservation: injected != delivered + stranded + in_flight_at_cutoff",
        );

        let mean_wait = if self.waits_with_queueing > 0 {
            self.sum_wait_atu / self.waits_with_queueing as i64
        } else {
            0
        };

        SimulationKPIs {
            total_flow_units: total_demand,
            total_injected: self.total_injected,
            total_delivered: self.total_delivered,
            total_late: self.total_late,
            total_on_time: self.total_on_time,
            on_time_pct,
            mean_transit_atu: mean_transit,
            total_cost_micros: self.total_cost_micros,
            total_co2_grams: self.total_co2_grams,
            total_capacity_units_delivered: self.total_capacity_delivered,
            stranded_flow_units: stranded,
            strand_reasons: self.strand_reasons.stats(stranded),
            in_flight_at_cutoff: self.in_flight.stats(in_flight_count),
            most_refused_lanes: self.top_refused_lanes(),
            unreachable_od_pairs: self.unreachable_pairs(),
            max_node_load: self.max_node_load,
            max_queue_depth: self.max_queue_depth,
            mean_wait_atu: mean_wait,
            max_wait_atu: self.max_wait_atu,
            waits_with_queueing: self.waits_with_queueing,
            wait_by_priority: self.wait_by_priority.clone(),
            simulation_wall_time_ms: wall_time_ms,
            events_processed: self.events_processed,
        }
    }
}
