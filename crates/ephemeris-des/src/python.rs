//! PyO3 bindings for the DES engine.
//!
//! Exposes PyEngine class to Python with Arrow IPC I/O.
//! Supports batch mode (run to completion) and streaming mode
//! (step_until one bucket at a time, drain surfaces between chunks).

use pyo3::prelude::*;
use std::time::Instant;

use crate::engine::DESEngine;
use crate::io;
use crate::router::StaticRouter;
use crate::surfaces::SurfaceSnapshot;
use crate::time_router::TimeAwareRouter;
use crate::types::{Atu, LaneIdx, NodeId};

/// Build the legacy per-bucket surface dict (the JSON `surface` channel). Shared
/// by `drain_surfaces` and `drain_surfaces_dual` so the two never diverge.
fn snapshot_to_pydict(py: Python, s: &SurfaceSnapshot) -> PyObject {
    let dict = pyo3::types::PyDict::new(py);
    dict.set_item("time_atu", s.time_atu).unwrap();
    dict.set_item("node_loads", &s.node_loads).unwrap();
    dict.set_item("node_backlogs", &s.node_backlogs).unwrap();
    dict.set_item("node_queue_depth", &s.node_queue_depth).unwrap();
    dict.set_item("lane_departures", &s.lane_departures).unwrap();
    dict.set_item("lane_flow_units", &s.lane_flow_units).unwrap();
    dict.set_item("delivered_count", s.delivered_count).unwrap();
    dict.set_item("on_time_count", s.on_time_count).unwrap();
    dict.set_item("late_count", s.late_count).unwrap();
    dict.set_item("total_transit_atu", s.total_transit_atu).unwrap();
    dict.set_item("total_cost_micros", s.total_cost_micros).unwrap();
    dict.set_item("total_co2_grams", s.total_co2_grams).unwrap();
    dict.set_item("waits_recorded", s.waits_recorded).unwrap();
    dict.set_item("total_wait_atu", s.total_wait_atu).unwrap();
    // Intermediate-node arrival records. Each entry: (sid, node, atu).
    let arrivals: Vec<(u64, u32, i64)> = s.intermediate_arrivals.iter().copied().collect();
    dict.set_item("intermediate_arrivals", arrivals).unwrap();
    dict.into_any().unbind()
}

#[pyclass]
pub struct PyEngine {
    inner: DESEngine,
    wall_start: Option<Instant>,
    total_demand: u64,
    /// Cumulative number of surface snapshots drained so far. Provides the
    /// `bucket_idx_base` argument for `drain_surfaces_arrow` so the Arrow
    /// `bucket_idx` column is monotonic across streaming-mode chunks.
    surfaces_drained: u32,
    /// Atlas-pinned node→cell (res-3) rollup lookup, built once from the node IPC
    /// at construction. The surface writer uses it to emit the cell-indexed
    /// columns with no runtime H3 (Sacred Contract I-D-005 / D-6).
    node_cell_lookup: io::NodeCellLookup,
    /// Whether the surface frame carries cumulative columns (default true).
    emit_cumulative: bool,
    /// Running cumulative totals, carried across streaming drains so the
    /// cumulative columns are correct over the whole run.
    cumulative: crate::surface_writer::CumulativeState,
    /// Run manifest JSON, set by `emit_run_manifest`. When present it is
    /// embedded in every surface stream's schema metadata for provenance.
    run_manifest_json: Option<String>,
}

#[pymethods]
impl PyEngine {
    /// Construct DES engine from Arrow IPC buffers.
    ///
    /// # Arguments
    /// * `node_ipc` — Arrow IPC bytes for node table.
    /// * `lane_ipc` — Arrow IPC bytes for lane table.
    /// * `schedule_ipc` — Arrow IPC bytes for schedule table.
    /// * `demand_ipc` — Arrow IPC bytes for demand event table.
    /// * `bucket_atu` — Surface emission interval in ATU (default 3_600_000 = 1h).
    #[new]
    #[pyo3(signature = (node_ipc, lane_ipc, schedule_ipc, demand_ipc, bucket_atu = 3_600_000, router = "time_aware"))]
    fn new(
        node_ipc: &[u8],
        lane_ipc: &[u8],
        schedule_ipc: &[u8],
        demand_ipc: &[u8],
        bucket_atu: i64,
        router: &str,
    ) -> PyResult<Self> {
        let num_nodes = io::count_nodes(node_ipc)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyIOError, _>(e.to_string()))?;

        let leaf_ids = io::extract_leaf_node_ids(node_ipc)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyIOError, _>(e.to_string()))?;

        // Atlas-pinned node→cell rollup lookup (res-3 R_UI). Hard requirement:
        // the node IPC must carry the v2 columns; no runtime H3 fallback.
        let node_cell_lookup = io::read_node_cell_lookup(node_ipc)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyIOError, _>(e.to_string()))?;

        let graph = io::read_lanes_to_graph(lane_ipc, num_nodes)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyIOError, _>(e.to_string()))?;

        let schedules = io::read_schedules(schedule_ipc, graph.num_lanes, &graph.lane_id_to_csr)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyIOError, _>(e.to_string()))?;

        let demand = io::read_demand(demand_ipc)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyIOError, _>(e.to_string()))?;

        let total_demand = demand.total() as u64;

        let router_box: Box<dyn crate::router::Router + Send + Sync> = match router {
            "static" => Box::new(StaticRouter::new(&graph, Some(leaf_ids.as_slice()))),
            _ => Box::new(TimeAwareRouter::new(bucket_atu)),
        };

        let num_lanes = graph.num_lanes as usize;
        let engine = DESEngine::new(graph, schedules, demand, router_box, bucket_atu);
        let cumulative =
            crate::surface_writer::CumulativeState::new(node_cell_lookup.num_cells(), num_lanes);

        Ok(PyEngine {
            inner: engine,
            wall_start: None,
            total_demand,
            surfaces_drained: 0,
            node_cell_lookup,
            emit_cumulative: true,
            cumulative,
            run_manifest_json: None,
        })
    }

    /// Run simulation for given horizon (ATU milliseconds). Batch mode.
    fn run(&mut self, py: Python, horizon_atu: i64) -> PyResult<()> {
        self.wall_start = Some(Instant::now());
        py.allow_threads(|| {
            self.inner.run(horizon_atu);
        });
        Ok(())
    }

    /// Advance simulation up to target_atu (exclusive). Streaming mode.
    ///
    /// Processes all events before target_atu, emits surfaces at bucket
    /// boundaries. Call drain_surfaces() after each step_until() to get
    /// the snapshots produced during this chunk.
    fn step_until(&mut self, py: Python, target_atu: i64) -> PyResult<()> {
        if self.wall_start.is_none() {
            self.wall_start = Some(Instant::now());
        }
        py.allow_threads(|| {
            self.inner.step_until(target_atu);
        });
        Ok(())
    }

    /// Emit final surface snapshot and mark simulation complete.
    fn finalize(&mut self) {
        self.inner.finalize_surfaces();
    }

    /// Drain collected surface snapshots as a list of JSON dicts.
    ///
    /// Moves snapshots out of the engine (they are not retained).
    /// Each dict contains: time_atu, node_loads, node_backlogs,
    /// delivered_count, on_time_count, late_count, etc.
    fn drain_surfaces(&mut self) -> Vec<PyObject> {
        let snapshots = self.inner.drain_surfaces();
        self.surfaces_drained = self.surfaces_drained.saturating_add(snapshots.len() as u32);
        Python::with_gil(|py| snapshots.iter().map(|s| snapshot_to_pydict(py, s)).collect())
    }

    /// Drain collected surface snapshots as a single Arrow IPC stream byte
    /// buffer. Schema is `SurfaceBlock` (see `crate::surface_writer`); the
    /// stream contains one record batch per snapshot.
    ///
    /// The `bucket_idx` column is monotonic across streaming drains —
    /// successive calls produce indices that continue where the previous
    /// drain left off.
    ///
    /// Snapshots are moved out of the engine (not retained), matching the
    /// semantics of [`Self::drain_surfaces`]. Session 1 emits this stream
    /// alongside the existing JSON dict path; Session 2 will switch the
    /// UI fully to the Arrow stream and the JSON path retires.
    fn drain_surfaces_arrow(&mut self) -> PyResult<pyo3::Py<pyo3::types::PyBytes>> {
        let num_nodes = self.inner.graph.num_nodes as usize;
        let num_lanes = self.inner.graph.num_lanes as usize;
        let base = self.surfaces_drained;
        // Per-CSR-lane source node for the cell departures rollup. CSR lanes are
        // sorted by source, so find_source is a binary search on node_offsets.
        let lane_src_node: Vec<NodeId> = (0..num_lanes as u32)
            .map(|e| self.inner.graph.find_source(e))
            .collect();
        let snapshots = self.inner.drain_surfaces();
        self.surfaces_drained = self.surfaces_drained.saturating_add(snapshots.len() as u32);
        let bytes = crate::surface_writer::write_surface_block_ipc(
            &snapshots,
            self.inner.surfaces.bucket_atu,
            num_nodes,
            num_lanes,
            base,
            &self.node_cell_lookup,
            &lane_src_node,
            self.emit_cumulative,
            Some(&mut self.cumulative),
            self.run_manifest_json.as_deref(),
        )
        .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
        Python::with_gil(|py| Ok(pyo3::types::PyBytes::new(py, &bytes).unbind()))
    }

    /// Drain once, returning BOTH the wire-ready Arrow IPC bytes (frame `0x06`)
    /// and the legacy per-bucket dicts. Lets the server forward the Rust-built
    /// binary while still emitting the legacy JSON `surface` channel, without
    /// draining the engine twice (the two single-purpose drains each move the
    /// snapshots out, so they cannot both be called for the same buckets).
    fn drain_surfaces_dual(
        &mut self,
    ) -> PyResult<(pyo3::Py<pyo3::types::PyBytes>, Vec<PyObject>)> {
        let num_nodes = self.inner.graph.num_nodes as usize;
        let num_lanes = self.inner.graph.num_lanes as usize;
        let base = self.surfaces_drained;
        let lane_src_node: Vec<NodeId> = (0..num_lanes as u32)
            .map(|e| self.inner.graph.find_source(e))
            .collect();
        let snapshots = self.inner.drain_surfaces();
        self.surfaces_drained = self.surfaces_drained.saturating_add(snapshots.len() as u32);
        let bytes = crate::surface_writer::write_surface_block_ipc(
            &snapshots,
            self.inner.surfaces.bucket_atu,
            num_nodes,
            num_lanes,
            base,
            &self.node_cell_lookup,
            &lane_src_node,
            self.emit_cumulative,
            Some(&mut self.cumulative),
            self.run_manifest_json.as_deref(),
        )
        .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
        Python::with_gil(|py| {
            let pybytes = pyo3::types::PyBytes::new(py, &bytes).unbind();
            let dicts: Vec<PyObject> =
                snapshots.iter().map(|s| snapshot_to_pydict(py, s)).collect();
            Ok((pybytes, dicts))
        })
    }

    /// Step N events (for debugging).
    fn step(&mut self, py: Python, n: usize) -> PyResult<usize> {
        py.allow_threads(|| Ok(self.inner.step(n)))
    }

    /// Get KPI summary as JSON string.
    fn get_kpis(&self) -> String {
        let wall_ms = self
            .wall_start
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(0);
        let kpis = self.inner.kpis.finalize(self.total_demand, wall_ms);
        serde_json::to_string_pretty(&kpis).unwrap_or_else(|_| "{}".to_string())
    }

    /// Realized boardings per scheduled departure, for the UI trip-builder's
    /// Batch dot sizing. Returns a dict of three parallel lists — `lane_id`,
    /// `depart_atu`, `flow units` — one entry per departure slot, joinable to the
    /// schedule on (lane_id, depart_atu). Call after the run completes.
    fn realized_boardings(&self, py: Python) -> PyObject {
        let (lane_ids, depart_atus, flow_units) = self.inner.boardings_by_departure();
        let dict = pyo3::types::PyDict::new(py);
        dict.set_item("lane_id", lane_ids).unwrap();
        dict.set_item("depart_atu", depart_atus).unwrap();
        dict.set_item("flow_units", flow_units).unwrap();
        dict.into_any().unbind()
    }

    /// Per-flow unit strand records for the OFFLINE feasibility oracle. Returns a
    /// dict of parallel lists — `flow_unit_id`, `reason` (StrandReason as u8;
    /// ScheduleInfeasible == 1), `origin_node`, `appear_atu`, `dest_node`,
    /// `due_atu`, `strand_node`, `site`, `held_atu` — one entry per stranded
    /// flow unit. Additive observability; the aggregate strand counts in
    /// `get_kpis()` are unchanged.
    fn strand_records(&self, py: Python) -> PyObject {
        let log = &self.inner.strand_log;
        let dict = pyo3::types::PyDict::new(py);
        let sids: Vec<u64> = log.iter().map(|r| r.flow_unit_id).collect();
        let reasons: Vec<u8> = log.iter().map(|r| r.reason).collect();
        let origins: Vec<u32> = log.iter().map(|r| r.origin_node).collect();
        let appears: Vec<i64> = log.iter().map(|r| r.appear_atu).collect();
        let dests: Vec<u32> = log.iter().map(|r| r.dest_node).collect();
        let dues: Vec<i64> = log.iter().map(|r| r.due_atu).collect();
        let strand_nodes: Vec<u32> = log.iter().map(|r| r.strand_node).collect();
        let sites: Vec<u8> = log.iter().map(|r| r.site).collect();
        let helds: Vec<i64> = log.iter().map(|r| r.held_atu).collect();
        dict.set_item("flow_unit_id", sids).unwrap();
        dict.set_item("reason", reasons).unwrap();
        dict.set_item("origin_node", origins).unwrap();
        dict.set_item("appear_atu", appears).unwrap();
        dict.set_item("dest_node", dests).unwrap();
        dict.set_item("due_atu", dues).unwrap();
        dict.set_item("strand_node", strand_nodes).unwrap();
        dict.set_item("site", sites).unwrap();
        // GATE-0 (schema-additive): held-but-not-in-transit time, i64 ATU.
        dict.set_item("held_atu", helds).unwrap();
        dict.into_any().unbind()
    }

    /// Per-flow unit delivery records (additive observability; the realized-
    /// arrival join for the plan-fidelity instrument). Returns a dict of
    /// parallel lists — `flow_unit_id`, `delivered_atu`, `wait_atu`,
    /// `held_atu` — one entry per delivered flow unit, in delivery order.
    fn delivery_records(&self, py: Python) -> PyObject {
        let log = &self.inner.delivery_log;
        let dict = pyo3::types::PyDict::new(py);
        let sids: Vec<u64> = log.iter().map(|r| r.flow_unit_id).collect();
        let delivered: Vec<i64> = log.iter().map(|r| r.delivered_atu).collect();
        let waits: Vec<i64> = log.iter().map(|r| r.wait_atu).collect();
        let helds: Vec<i64> = log.iter().map(|r| r.held_atu).collect();
        dict.set_item("flow_unit_id", sids).unwrap();
        dict.set_item("delivered_atu", delivered).unwrap();
        dict.set_item("wait_atu", waits).unwrap();
        // GATE-0 (schema-additive): held-but-not-in-transit time, i64 ATU —
        // a strict superset of `wait_atu` (adds schedule dwell to queue
        // time). The engine measures; pricing lives above the engine.
        dict.set_item("held_atu", helds).unwrap();
        dict.into_any().unbind()
    }

    /// Enable/disable the per-boarding hop trace (off by default). Call before
    /// `run`/`step_until`. Observability only — routing and counts unchanged.
    fn set_record_hop_trace(&mut self, enabled: bool) {
        self.inner.set_record_hop_trace(enabled);
    }

    /// Realized hop trace (empty unless `set_record_hop_trace(True)` was set).
    /// Returns a dict of three parallel lists — `flow_unit_id`, `csr_lane`,
    /// `depart_atu` — one entry per realized boarding, in boarding order.
    fn hop_records(&self, py: Python) -> PyObject {
        let log = &self.inner.hop_log;
        let dict = pyo3::types::PyDict::new(py);
        let sids: Vec<u64> = log.iter().map(|r| r.flow_unit_id).collect();
        let lanes: Vec<u32> = log.iter().map(|r| r.csr_lane).collect();
        let departs: Vec<i64> = log.iter().map(|r| r.depart_atu).collect();
        dict.set_item("flow_unit_id", sids).unwrap();
        dict.set_item("csr_lane", lanes).unwrap();
        dict.set_item("depart_atu", departs).unwrap();
        dict.into_any().unbind()
    }

    /// OFFLINE oracle: exact-time earliest arrival from (origin_node, release_atu)
    /// to every node over this run's timetable, capacity ignored. Element `dest`
    /// is the arrival ATU, or i64::MAX if unreachable. Read-only.
    fn earliest_arrival(&self, origin_node: u32, release_atu: i64) -> Vec<i64> {
        self.inner.earliest_arrival_from(origin_node, release_atu)
    }

    /// BULK offline oracle via destination-rooted backward profiles (one
    /// profile-CSA scan per distinct destination, one binary search per
    /// flow unit). Capacity ignored; identical relaxation math to
    /// `earliest_arrival`. Read-only and deterministic (BTreeMap grouping,
    /// input-order output).
    ///
    /// Returns a dict of two parallel lists, one entry per input flow unit:
    ///   `arrival_atu` — earliest arrival at dest (i64::MAX if unreachable)
    ///   `verdict` — 0=ON_TIME, 1=LATE, 2=INFEASIBLE_CLIFF (reachable for an
    ///     earlier release), 3=INFEASIBLE_STRUCTURAL (never reachable)
    fn oracle_verdicts(
        &self,
        py: Python,
        origins: Vec<u32>,
        dests: Vec<u32>,
        releases: Vec<i64>,
        dues: Vec<i64>,
    ) -> PyResult<PyObject> {
        let m = origins.len();
        if dests.len() != m || releases.len() != m || dues.len() != m {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "oracle_verdicts: origins/dests/releases/dues must have equal length",
            ));
        }
        let (arrivals, verdicts) = py.allow_threads(|| {
            crate::feasibility_oracle::bulk_verdicts(
                &self.inner.graph,
                &self.inner.schedules,
                &origins,
                &dests,
                &releases,
                &dues,
            )
        });
        let dict = pyo3::types::PyDict::new(py);
        dict.set_item("arrival_atu", arrivals)?;
        dict.set_item("verdict", verdicts)?;
        Ok(dict.into_any().unbind())
    }

    /// Get current simulation time in ATU.
    fn get_current_time_atu(&self) -> i64 {
        self.inner.current_time_atu()
    }

    /// Get current node loads as list.
    fn get_node_loads(&self) -> Vec<u32> {
        self.inner.node_loads().to_vec()
    }

    /// Get current per-node overflow-queue depth as list.
    fn get_node_queue_depth(&self) -> Vec<u32> {
        self.inner.node_queue_depth().to_vec()
    }

    /// Total flow units currently waiting in any overflow queue.
    fn get_total_queued(&self) -> u64 {
        self.inner.total_queued()
    }

    /// Install lane outage windows. `outages` is a list of
    /// `(csr_lane, t_start_atu, t_end_atu)` half-open intervals. Replaces
    /// any previously set windows.
    fn set_lane_outages(&mut self, outages: Vec<(u32, i64, i64)>) {
        let typed: Vec<(LaneIdx, Atu, Atu)> =
            outages.into_iter().map(|(e, s, t)| (e as LaneIdx, s, t)).collect();
        self.inner.set_lane_outages(typed);
    }

    /// Install node outage windows.
    fn set_node_outages(&mut self, outages: Vec<(u32, i64, i64)>) {
        let typed: Vec<(NodeId, Atu, Atu)> =
            outages.into_iter().map(|(h, s, t)| (h as NodeId, s, t)).collect();
        self.inner.set_node_outages(typed);
    }

    /// Inject additional demand events from Arrow IPC bytes (§E shock).
    /// New events with `appear_atu < current_time` will fire on the next
    /// engine sweep — never silently dropped.
    fn inject_demand(&mut self, demand_ipc: &[u8]) -> PyResult<()> {
        let dq = io::read_demand(demand_ipc)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyIOError, _>(e.to_string()))?;
        // io::read_demand returns a DemandQueue; pull its underlying events.
        let events = dq.into_events();
        self.inner.inject_demand_events(events);
        Ok(())
    }

    // ── Closure — event-queue mutation API ───────────────────────────
    //
    // These methods MUST be called between bucket boundaries (after
    // step_until returns, before the next step_until). They do not
    // alter the DES hot path (`step_until` → `process_event` is
    // unchanged). The DES dispatch loop continues to consult no
    // outside state during event processing.

    /// Apply a batch of schedule-mutation operations atomically.
    ///
    /// Each `op` is a Python dict. Schema:
    ///   {"type": "cancel_departures", "sid": <u64>, "at_or_after_atu": <i64>}
    ///   {"type": "cancel_arrivals",   "sid": <u64>, "at_or_after_atu": <i64>}
    ///   {"type": "cancel_all",        "sid": <u64>, "at_or_after_atu": <i64>}
    ///   {"type": "insert_arrival",    "sid": <u64>, "node": <u32>, "at_atu": <i64>}
    ///
    /// Returns the total number of events affected (cancellations +
    /// inserts). The mutations are applied in order. Since each
    /// individual op is non-failing (modulo OOM), the batch is
    /// effectively atomic at the Python boundary.
    fn apply_schedule_ops(
        &mut self,
        ops: Vec<pyo3::Bound<'_, pyo3::types::PyDict>>,
    ) -> PyResult<usize> {
        use crate::events::EventKind;
        use pyo3::types::PyAnyMethods;
        let mut affected = 0usize;
        for op in ops {
            let ty: String = op
                .get_item("type")?
                .ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err("schedule op missing 'type'")
                })?
                .extract()?;
            let sid: u64 = op
                .get_item("sid")?
                .ok_or_else(|| {
                    pyo3::exceptions::PyValueError::new_err("schedule op missing 'sid'")
                })?
                .extract()?;
            match ty.as_str() {
                "cancel_departures" => {
                    let after: i64 = op.get_item("at_or_after_atu")?.unwrap().extract()?;
                    affected += self
                        .inner
                        .cancel_events_for_flow_unit(sid, after, &[EventKind::Depart]);
                }
                "cancel_arrivals" => {
                    let after: i64 = op.get_item("at_or_after_atu")?.unwrap().extract()?;
                    affected += self
                        .inner
                        .cancel_events_for_flow_unit(sid, after, &[EventKind::Arrive]);
                }
                "cancel_all" => {
                    let after: i64 = op.get_item("at_or_after_atu")?.unwrap().extract()?;
                    affected += self.inner.cancel_all_events_for_flow_unit(sid, after);
                }
                "insert_arrival" => {
                    let node: u32 = op.get_item("node")?.unwrap().extract()?;
                    let at_atu: i64 = op.get_item("at_atu")?.unwrap().extract()?;
                    self.inner
                        .insert_event_raw(sid, EventKind::Arrive, at_atu, node);
                    affected += 1;
                }
                _ => {
                    return Err(pyo3::exceptions::PyValueError::new_err(format!(
                        "unknown schedule op type {:?}",
                        ty
                    )));
                }
            }
        }
        Ok(affected)
    }

    /// Get total delivered count.
    fn get_total_delivered(&self) -> u64 {
        self.inner.total_delivered()
    }

    /// Get number of events remaining in queue.
    fn get_queue_size(&self) -> usize {
        self.inner.events_in_queue()
    }

    /// Toggle capacity enforcement.
    fn set_enforce_capacity(&mut self, enforce: bool) {
        self.inner.enforce_capacity = enforce;
    }

    /// Get number of surface snapshots currently held.
    fn get_surface_count(&self) -> usize {
        self.inner.surfaces.snapshots.len()
    }

    /// Inject forced routes for Tier 3 planner.
    ///
    /// Each entry is (flow_unit_id, lane_path) where lane_path is a list
    /// of CSR lane indices in FORWARD order (origin → destination).
    /// The engine stores them reversed internally (pop from back).
    fn set_forced_routes(&mut self, routes: Vec<(u64, Vec<u32>)>) {
        for (sid, mut path) in routes {
            path.reverse(); // Engine pops from the back
            self.inner.insert_forced_route(sid, path);
        }
    }

    /// Install slot-PINNED forced routes and charge their reservations
    /// (Option A commitment semantics). Each entry is
    /// `(flow_unit_id, [(csr_lane, depart_atu), ...])` in FORWARD order; the
    /// engine reverses internally. ATOMIC: validates every hop against the
    /// timetable and `usage + reserved ≤ capacity` per slot before charging
    /// anything; raises ValueError (installing nothing) on any violation.
    /// Unplanned boarding then sees `capacity − reserved`; pinned flow units
    /// board exactly their reserved slot. Returns the number of routes
    /// installed.
    fn set_forced_routes_pinned(
        &mut self,
        routes: Vec<(u64, Vec<(u32, i64)>)>,
    ) -> PyResult<usize> {
        let typed: Vec<(u64, Vec<(LaneIdx, i64)>)> = routes
            .into_iter()
            .map(|(sid, path)| {
                (sid, path.into_iter().map(|(e, t)| (e as LaneIdx, t)).collect())
            })
            .collect();
        self.inner
            .set_pinned_routes(typed)
            .map_err(pyo3::exceptions::PyValueError::new_err)
    }

    /// Cancel pinned routes (C2: disruption-invalidation release). Releases
    /// the outstanding reservations of each flow unit's REMAINING hops;
    /// already-boarded hops untouched; unknown/unpinned sids skipped.
    /// Returns the number of routes cancelled. The freed capacity becomes
    /// visible to unplanned boarding and to future set_forced_routes_pinned.
    fn cancel_pinned_routes(&mut self, sids: Vec<u64>) -> usize {
        self.inner.cancel_pinned_routes(&sids)
    }

    /// Per-slot capacity state (C2 observability): dict of parallel lists
    /// over the global slot index — `lane_id`, `depart_atu`, `capacity`,
    /// `usage`, `reserved`. The live-loop harness mirrors residuals
    /// (capacity − usage − reserved) from this. Read-only.
    fn slot_state(&self, py: Python) -> PyObject {
        let (lane_ids, departs, caps, usage, reserved) = self.inner.slot_state();
        let dict = pyo3::types::PyDict::new(py);
        dict.set_item("lane_id", lane_ids).unwrap();
        dict.set_item("depart_atu", departs).unwrap();
        dict.set_item("capacity", caps).unwrap();
        dict.set_item("usage", usage).unwrap();
        dict.set_item("reserved", reserved).unwrap();
        dict.into_any().unbind()
    }

    /// Get the lane_id_to_csr mapping for lane index translation.
    ///
    /// Returns a list where index = original_lane_id, value = CSR position.
    /// The planner uses this to convert its lane IDs to CSR indices.
    fn get_lane_id_to_csr(&self) -> Vec<u32> {
        self.inner.graph.lane_id_to_csr.clone()
    }

    /// Get number of nodes in the graph.
    fn get_num_nodes(&self) -> u32 {
        self.inner.graph.num_nodes
    }

    /// Get number of lanes in the graph.
    fn get_num_lanes(&self) -> u32 {
        self.inner.graph.num_lanes
    }

    /// Number of distinct view cells (res-3) the nodes roll up into. Surfaces
    /// emit this many cell-indexed rows per bucket. Observability / test hook.
    fn get_num_cells(&self) -> usize {
        self.node_cell_lookup.num_cells()
    }

    /// Toggle the cumulative-view columns on the surface frame (default on).
    /// Affects subsequent `drain_surfaces_arrow` calls.
    fn set_emit_cumulative(&mut self, emit: bool) {
        self.emit_cumulative = emit;
    }

    /// The 16-hex schema fingerprint of the surface frame this engine emits
    /// (depends on the current cumulative setting). The TS reader pins the same
    /// value; surface this so the two stay in sync.
    fn get_schema_fingerprint(&self) -> String {
        crate::surface_writer::surface_schema_fingerprint(self.emit_cumulative)
    }

    /// Build and emit the run manifest at simulation start.
    ///
    /// Writes `{runs_dir}/{run_id}/run_manifest.json` and stores the JSON so it
    /// is embedded in every subsequent surface stream's schema metadata. The
    /// orchestrator owns the clock, so `created_at` (RFC 3339) is supplied by
    /// the caller. `controller_config` is hashed (SHA-256) into
    /// `controller_config_hash`; pass empty bytes for "no controller".
    /// Returns the manifest JSON.
    #[pyo3(signature = (runs_dir, run_id, scenario_id, seed, created_at, controller_config = None))]
    fn emit_run_manifest(
        &mut self,
        runs_dir: &str,
        run_id: &str,
        scenario_id: &str,
        seed: u64,
        created_at: &str,
        controller_config: Option<Vec<u8>>,
    ) -> PyResult<String> {
        let fingerprint =
            crate::surface_writer::surface_schema_fingerprint(self.emit_cumulative);
        let manifest = crate::run_manifest::RunManifest::v1(
            run_id,
            scenario_id,
            seed,
            crate::surface_writer::SCHEMA_VERSION,
            fingerprint,
            crate::run_manifest::controller_config_hash(controller_config.as_deref().unwrap_or(&[])),
            created_at,
        );
        manifest
            .write_to(runs_dir)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyIOError, _>(e.to_string()))?;
        let json = manifest.to_json();
        self.run_manifest_json = Some(json.clone());
        Ok(json)
    }
}
