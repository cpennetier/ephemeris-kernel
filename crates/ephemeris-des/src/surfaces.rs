//! Time-bucketed surface snapshots.
//!
//! Emitted at configurable intervals (default: 1 hour ATU).
//! Each snapshot captures node loads, lane flows, and scalar counters.

use crate::types::{Atu, CapacityUnits, Co2Grams, CostMicros};

/// A single surface snapshot at a point in time.
#[derive(Debug, Clone)]
pub struct SurfaceSnapshot {
    pub time_atu: Atu,
    pub node_loads: Vec<CapacityUnits>,
    pub node_backlogs: Vec<u32>,
    /// Flow units currently waiting in an overflow queue at each node.
    /// Strict subset of `node_backlogs`; non-zero only under capacity pressure.
    pub node_queue_depth: Vec<u32>,
    pub lane_departures: Vec<u32>,
    pub lane_flow_units: Vec<u64>,
    // Scalar counters (cumulative since last snapshot)
    pub delivered_count: u64,
    pub on_time_count: u64,
    pub late_count: u64,
    pub total_transit_atu: i64,
    pub total_cost_micros: CostMicros,
    pub total_co2_grams: Co2Grams,
    pub waits_recorded: u64,
    pub total_wait_atu: i64,
    /// Per-arrival records for intermediate-node arrivals occurring
    /// in this bucket. Each entry is `(flow_unit_id, node_id, atu)`. Final
    /// arrivals (delivery at the destination node) are NOT recorded here —
    /// they are visible via `delivered_count` and the flow unit is gone.
    /// Origin admissions (first hop after `Inject`) are also NOT recorded
    /// here — the bucket-boundary integration already sees admissions
    /// via the injected-demand queue. This vector exclusively carries
    /// mid-journey node arrivals: `Moving → Dwelling` transitions in the
    /// entity state taxonomy.
    pub intermediate_arrivals: Vec<(u64, u32, Atu)>,
}

/// Accumulator for per-bucket metrics. Reset after each surface emission.
pub struct BucketAccumulator {
    pub lane_departures: Vec<u32>,
    pub lane_flow_units: Vec<u64>,
    pub delivered_count: u64,
    pub on_time_count: u64,
    pub late_count: u64,
    pub total_transit_atu: i64,
    pub total_cost_micros: CostMicros,
    pub total_co2_grams: Co2Grams,
    pub waits_recorded: u64,
    pub total_wait_atu: i64,
    /// Intermediate-node arrival records accumulated this bucket.
    pub intermediate_arrivals: Vec<(u64, u32, Atu)>,
}

impl BucketAccumulator {
    pub fn new(num_lanes: usize) -> Self {
        BucketAccumulator {
            lane_departures: vec![0; num_lanes],
            lane_flow_units: vec![0; num_lanes],
            delivered_count: 0,
            on_time_count: 0,
            late_count: 0,
            total_transit_atu: 0,
            total_cost_micros: 0,
            total_co2_grams: 0,
            waits_recorded: 0,
            total_wait_atu: 0,
            intermediate_arrivals: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.lane_departures.fill(0);
        self.lane_flow_units.fill(0);
        self.delivered_count = 0;
        self.on_time_count = 0;
        self.late_count = 0;
        self.total_transit_atu = 0;
        self.total_cost_micros = 0;
        self.total_co2_grams = 0;
        self.waits_recorded = 0;
        self.total_wait_atu = 0;
        self.intermediate_arrivals.clear();
    }

    /// Record a `Moving → Dwelling` arrival at an intermediate node.
    /// The caller is responsible for filtering out final-node arrivals.
    pub fn record_intermediate_arrival(&mut self, flow_unit_id: u64, node: u32, at_atu: Atu) {
        self.intermediate_arrivals.push((flow_unit_id, node, at_atu));
    }

    pub fn record_wait(&mut self, wait_atu: Atu) {
        if wait_atu > 0 {
            self.waits_recorded += 1;
            self.total_wait_atu += wait_atu;
        }
    }

    pub fn record_departure(&mut self, csr_lane: usize, size_units: u32) {
        if csr_lane < self.lane_departures.len() {
            self.lane_departures[csr_lane] += 1;
            self.lane_flow_units[csr_lane] += size_units as u64;
        }
    }

    pub fn record_delivery(&mut self, transit_atu: Atu, on_time: bool, cost: CostMicros, co2: Co2Grams) {
        self.delivered_count += 1;
        if on_time {
            self.on_time_count += 1;
        } else {
            self.late_count += 1;
        }
        self.total_transit_atu += transit_atu;
        self.total_cost_micros += cost;
        self.total_co2_grams += co2;
    }
}

/// Surface store collecting snapshots over the simulation.
pub struct SurfaceStore {
    pub snapshots: Vec<SurfaceSnapshot>,
    pub bucket_atu: Atu,
    pub next_emit_atu: Atu,
    pub accumulator: BucketAccumulator,
}

impl SurfaceStore {
    pub fn new(_num_nodes: usize, num_lanes: usize, bucket_atu: Atu) -> Self {
        SurfaceStore {
            snapshots: Vec::new(),
            bucket_atu,
            next_emit_atu: bucket_atu,
            accumulator: BucketAccumulator::new(num_lanes),
        }
    }

    /// Emit a snapshot if the current time has passed the next emit boundary.
    pub fn maybe_emit(
        &mut self,
        current_atu: Atu,
        node_loads: &[CapacityUnits],
        node_backlogs: &[u32],
        node_queue_depth: &[u32],
    ) {
        while current_atu >= self.next_emit_atu {
            self.emit_snapshot(self.next_emit_atu, node_loads, node_backlogs, node_queue_depth);
            self.next_emit_atu += self.bucket_atu;
        }
    }

    /// Force-emit a final snapshot.
    pub fn emit_final(
        &mut self,
        current_atu: Atu,
        node_loads: &[CapacityUnits],
        node_backlogs: &[u32],
        node_queue_depth: &[u32],
    ) {
        self.emit_snapshot(current_atu, node_loads, node_backlogs, node_queue_depth);
    }

    /// Encode the currently collected snapshots as an Arrow IPC stream
    /// suitable for WebSocket frame tag `0x06`. Runs alongside the existing
    /// dict-based drain path while Session 2 wires the consumer side.
    ///
    /// `num_nodes` and `num_lanes` are required for schema-side length
    /// validation; pass the same values the engine was constructed with.
    /// `bucket_idx_base` is the index of the first snapshot in the buffer
    /// (use the running count across streaming-mode drains).
    pub fn snapshots_to_arrow_ipc(
        &self,
        num_nodes: usize,
        num_lanes: usize,
        bucket_idx_base: u32,
        cells: &crate::io::NodeCellLookup,
        lane_src_node: &[crate::types::NodeId],
        emit_cumulative: bool,
        cum: Option<&mut crate::surface_writer::CumulativeState>,
    ) -> Result<Vec<u8>, arrow::error::ArrowError> {
        crate::surface_writer::write_surface_block_ipc(
            &self.snapshots,
            self.bucket_atu,
            num_nodes,
            num_lanes,
            bucket_idx_base,
            cells,
            lane_src_node,
            emit_cumulative,
            cum,
            None,
        )
    }

    fn emit_snapshot(
        &mut self,
        time_atu: Atu,
        node_loads: &[CapacityUnits],
        node_backlogs: &[u32],
        node_queue_depth: &[u32],
    ) {
        let snapshot = SurfaceSnapshot {
            time_atu,
            node_loads: node_loads.to_vec(),
            node_backlogs: node_backlogs.to_vec(),
            node_queue_depth: node_queue_depth.to_vec(),
            lane_departures: self.accumulator.lane_departures.clone(),
            lane_flow_units: self.accumulator.lane_flow_units.clone(),
            delivered_count: self.accumulator.delivered_count,
            on_time_count: self.accumulator.on_time_count,
            late_count: self.accumulator.late_count,
            total_transit_atu: self.accumulator.total_transit_atu,
            total_cost_micros: self.accumulator.total_cost_micros,
            total_co2_grams: self.accumulator.total_co2_grams,
            waits_recorded: self.accumulator.waits_recorded,
            total_wait_atu: self.accumulator.total_wait_atu,
            intermediate_arrivals: std::mem::take(&mut self.accumulator.intermediate_arrivals),
        };
        self.snapshots.push(snapshot);
        self.accumulator.reset();
    }
}
