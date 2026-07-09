//! Ephemeris Kernel DES — criterion benchmarks for §D of the test plan.
//!
//! Decomposed timings: IPC parsing, CSR build, reverse Dijkstra,
//! TimeAwareRouter first-hop query, EventQueue micro, full
//! engine.run() at three scales. Run with `cargo bench`.
//!
//! Designed to disable the `pyo3` feature path (criterion would otherwise
//! fail to link the cdylib in test mode). Run as:
//!
//!     cargo bench --no-default-features

use std::sync::Arc;

use arrow::array::{
    BooleanArray, Float64Array, Int32Array, Int64Array, StringArray, UInt8Array, UInt32Array,
    UInt64Array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use ephemeris_des::demand::DemandQueue;
use ephemeris_des::engine::DESEngine;
use ephemeris_des::events::{Event, EventKind, EventQueue};
use ephemeris_des::graph::CSRGraph;
use ephemeris_des::io;
use ephemeris_des::router::{Router, StaticRouter};
use ephemeris_des::schedule::ScheduleStore;
use ephemeris_des::time_router::TimeAwareRouter;

const ATU_PER_HOUR: i64 = 3_600_000;

// ── Synthetic scenario builders ────────────────────────────────────────

/// node_spoke with `k = round(sqrt(n))` gateways, the rest as leaves.
/// Returns the four flat arrays the production CSR/Schedule constructors
/// consume — close cousin of the Python testkit, but Rust-side and
/// allocation-free at run time.
struct Synthetic {
    n_nodes: u32,
    lane_ids: Vec<u32>,
    lane_src: Vec<u32>,
    lane_dst: Vec<u32>,
    lane_mode: Vec<u8>,
    lane_dur: Vec<i64>,
    lane_cost: Vec<i64>,
    lane_co2: Vec<i64>,
    sched_lane: Vec<u32>,
    sched_dep: Vec<i64>,
    sched_cap: Vec<u32>,
    dem_sids: Vec<u64>,
    dem_appear: Vec<i64>,
    dem_due: Vec<i64>,
    dem_origin: Vec<u32>,
    dem_dest: Vec<u32>,
    dem_size: Vec<u32>,
    dem_prio: Vec<u8>,
    dem_flags: Vec<u8>,
}

fn synth(n_nodes: u32, n_demand: u32, slots_per_lane: u32, cap_per_slot: u32) -> Synthetic {
    let k = (n_nodes as f64).sqrt().round() as u32;
    let k = k.max(2);

    // Lanes: gateway↔gateway (full mesh), then spokes from each non-gateway.
    let mut lane_ids = Vec::new();
    let mut lane_src = Vec::new();
    let mut lane_dst = Vec::new();
    let mut next_eid: u32 = 0;
    let mut push_lane = |ids: &mut Vec<u32>, src: &mut Vec<u32>, dst: &mut Vec<u32>, s, d| {
        ids.push(next_eid);
        src.push(s);
        dst.push(d);
        next_eid += 1;
    };
    // Bind locally to avoid closure capture of next_eid.
    drop(push_lane);

    let mut next_eid: u32 = 0;
    for i in 0..k {
        for j in 0..k {
            if i != j {
                lane_ids.push(next_eid);
                lane_src.push(i);
                lane_dst.push(j);
                next_eid += 1;
            }
        }
    }
    for h in k..n_nodes {
        let gw = h % k;
        lane_ids.push(next_eid); lane_src.push(gw); lane_dst.push(h); next_eid += 1;
        lane_ids.push(next_eid); lane_src.push(h); lane_dst.push(gw); next_eid += 1;
    }

    let n_lanes = lane_ids.len();
    let lane_mode = vec![0u8; n_lanes];
    let lane_dur = vec![60_000i64; n_lanes]; // 1-minute lanes — keeps multi-hop tractable
    let lane_cost = vec![100_000i64; n_lanes];
    let lane_co2 = vec![10i64; n_lanes];

    // Schedules: `slots_per_lane` evenly spaced across 24h.
    let mut sched_lane = Vec::with_capacity(n_lanes * slots_per_lane as usize);
    let mut sched_dep = Vec::with_capacity(n_lanes * slots_per_lane as usize);
    let mut sched_cap = Vec::with_capacity(n_lanes * slots_per_lane as usize);
    let step = 24 * ATU_PER_HOUR / slots_per_lane.max(1) as i64;
    for &eid in &lane_ids {
        for s in 0..slots_per_lane {
            sched_lane.push(eid);
            sched_dep.push(s as i64 * step);
            sched_cap.push(cap_per_slot);
        }
    }

    // Demand: leaf → leaf, deterministic LCG-style draws.
    let leaves: Vec<u32> = (k..n_nodes).collect();
    let n_leaves = leaves.len() as u32;
    let mut dem_sids = Vec::with_capacity(n_demand as usize);
    let mut dem_appear = Vec::with_capacity(n_demand as usize);
    let mut dem_due = Vec::with_capacity(n_demand as usize);
    let mut dem_origin = Vec::with_capacity(n_demand as usize);
    let mut dem_dest = Vec::with_capacity(n_demand as usize);
    let mut dem_size = Vec::with_capacity(n_demand as usize);
    let mut dem_prio = Vec::with_capacity(n_demand as usize);
    let mut dem_flags = Vec::with_capacity(n_demand as usize);
    let horizon_atu = 24 * ATU_PER_HOUR;
    let mut state: u64 = 0xDEADBEEF;
    for sid in 0..n_demand as u64 {
        // xorshift64
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let r = state;
        let appear = ((r as i64).rem_euclid(horizon_atu)) as i64;
        let origin = leaves[(r as u32 % n_leaves) as usize];
        let dest_idx = ((r >> 32) as u32) % n_leaves;
        let dest = leaves[dest_idx as usize];
        if dest == origin {
            // Skip degenerate; LCG rarely hits self for non-trivial n.
            continue;
        }
        dem_sids.push(sid);
        dem_appear.push(appear);
        dem_due.push(appear + 8 * ATU_PER_HOUR);
        dem_origin.push(origin);
        dem_dest.push(dest);
        dem_size.push(1 + (sid as u32 % 5));
        dem_prio.push((sid as u8) % 3);
        dem_flags.push(0);
    }
    // Sort by appear_atu (DemandQueue::from_arrays debug-asserts this).
    let mut order: Vec<usize> = (0..dem_sids.len()).collect();
    order.sort_by_key(|&i| (dem_appear[i], dem_sids[i]));
    let dem_sids = order.iter().map(|&i| dem_sids[i]).collect();
    let dem_appear: Vec<i64> = order.iter().map(|&i| dem_appear[i]).collect();
    let dem_due = order.iter().map(|&i| dem_due[i]).collect();
    let dem_origin = order.iter().map(|&i| dem_origin[i]).collect();
    let dem_dest = order.iter().map(|&i| dem_dest[i]).collect();
    let dem_size = order.iter().map(|&i| dem_size[i]).collect();
    let dem_prio = order.iter().map(|&i| dem_prio[i]).collect();
    let dem_flags = order.iter().map(|&i| dem_flags[i]).collect();

    Synthetic {
        n_nodes,
        lane_ids, lane_src, lane_dst, lane_mode, lane_dur, lane_cost, lane_co2,
        sched_lane, sched_dep, sched_cap,
        dem_sids, dem_appear, dem_due, dem_origin, dem_dest, dem_size, dem_prio, dem_flags,
    }
}

fn build_graph(s: &Synthetic) -> CSRGraph {
    CSRGraph::from_lanes(
        s.n_nodes,
        &s.lane_src, &s.lane_dst, &s.lane_ids, &s.lane_mode,
        &s.lane_dur, &s.lane_cost, &s.lane_co2,
    )
}

fn build_schedules(s: &Synthetic, lane_id_to_csr: &[u32]) -> ScheduleStore {
    ScheduleStore::from_flat(
        s.lane_ids.len() as u32,
        &s.sched_lane, &s.sched_dep, &s.sched_cap, lane_id_to_csr,
    )
}

fn build_demand(s: &Synthetic) -> DemandQueue {
    DemandQueue::from_arrays(
        &s.dem_sids, &s.dem_appear, &s.dem_due,
        &s.dem_origin, &s.dem_dest, &s.dem_size, &s.dem_prio, &s.dem_flags,
    )
}

// ── Arrow IPC writers (for `io::read_*` benches) ──────────────────────

fn ipc_lanes(s: &Synthetic) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("lane_id", DataType::UInt32, false),
        Field::new("src_node_id", DataType::UInt32, false),
        Field::new("dst_node_id", DataType::UInt32, false),
        Field::new("lane_mode", DataType::UInt8, false),
        Field::new("duration_atu", DataType::Int64, false),
        Field::new("cost_usd_micros", DataType::Int64, false),
        Field::new("co2_g", DataType::Int64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt32Array::from(s.lane_ids.clone())),
            Arc::new(UInt32Array::from(s.lane_src.clone())),
            Arc::new(UInt32Array::from(s.lane_dst.clone())),
            Arc::new(UInt8Array::from(s.lane_mode.clone())),
            Arc::new(Int64Array::from(s.lane_dur.clone())),
            Arc::new(Int64Array::from(s.lane_cost.clone())),
            Arc::new(Int64Array::from(s.lane_co2.clone())),
        ],
    ).unwrap();
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut buf, &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
    }
    buf
}

fn ipc_schedules(s: &Synthetic) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("lane_id", DataType::UInt32, false),
        Field::new("depart_atu", DataType::Int64, false),
        Field::new("capacity", DataType::UInt32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt32Array::from(s.sched_lane.clone())),
            Arc::new(Int64Array::from(s.sched_dep.clone())),
            Arc::new(UInt32Array::from(s.sched_cap.clone())),
        ],
    ).unwrap();
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut buf, &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
    }
    buf
}

fn ipc_nodes(n_nodes: u32) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("node_id", DataType::UInt32, false),
        Field::new("h3_index", DataType::UInt64, false),
        Field::new("lat", DataType::Float64, false),
        Field::new("lon", DataType::Float64, false),
        Field::new("tier", DataType::Utf8, false),
        Field::new("territory_id", DataType::UInt32, false),
        Field::new("demand_gravity", DataType::Float64, false),
        Field::new("has_airport", DataType::Boolean, false),
        Field::new("has_port", DataType::Boolean, false),
        Field::new("has_rail", DataType::Boolean, false),
        Field::new("landmass_id", DataType::Int32, false),
    ]));
    let ids: Vec<u32> = (0..n_nodes).collect();
    let h3: Vec<u64> = (0..n_nodes as u64).map(|i| 0x881F0D2400000000u64 | i).collect();
    let lats = vec![40.0f64; n_nodes as usize];
    let lons = vec![-100.0f64; n_nodes as usize];
    let tier: Vec<&str> = (0..n_nodes as usize).map(|_| "leaf").collect();
    let territory = vec![0u32; n_nodes as usize];
    let gravity = vec![1.0f64; n_nodes as usize];
    let airport = vec![false; n_nodes as usize];
    let port = vec![false; n_nodes as usize];
    let rail = vec![false; n_nodes as usize];
    let landmass = vec![0i32; n_nodes as usize];
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt32Array::from(ids)),
            Arc::new(UInt64Array::from(h3)),
            Arc::new(Float64Array::from(lats)),
            Arc::new(Float64Array::from(lons)),
            Arc::new(StringArray::from(tier)),
            Arc::new(UInt32Array::from(territory)),
            Arc::new(Float64Array::from(gravity)),
            Arc::new(BooleanArray::from(airport)),
            Arc::new(BooleanArray::from(port)),
            Arc::new(BooleanArray::from(rail)),
            Arc::new(Int32Array::from(landmass)),
        ],
    ).unwrap();
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut buf, &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
    }
    buf
}

// ── Benchmarks ─────────────────────────────────────────────────────────

fn bench_ipc_parsing(c: &mut Criterion) {
    let mut g = c.benchmark_group("ipc_parsing");
    for n_nodes in [50u32, 200, 500] {
        let s = synth(n_nodes, 1_000, 24, 16);
        let lane_buf = ipc_lanes(&s);
        let sched_buf = ipc_schedules(&s);
        let node_buf = ipc_nodes(n_nodes);

        g.throughput(Throughput::Bytes(lane_buf.len() as u64));
        g.bench_with_input(BenchmarkId::new("read_lanes_to_graph", n_nodes), &lane_buf, |b, buf| {
            b.iter(|| black_box(io::read_lanes_to_graph(black_box(buf), n_nodes).unwrap()))
        });
        g.bench_with_input(BenchmarkId::new("count_nodes", n_nodes), &node_buf, |b, buf| {
            b.iter(|| black_box(io::count_nodes(black_box(buf)).unwrap()))
        });
        g.bench_with_input(BenchmarkId::new("read_schedules", n_nodes), &(sched_buf, &s), |b, (buf, s)| {
            // Build lane_id_to_csr once per setup.
            let g = build_graph(s);
            b.iter(|| black_box(io::read_schedules(black_box(buf), s.lane_ids.len() as u32, &g.lane_id_to_csr).unwrap()))
        });
    }
    g.finish();
}

fn bench_csr_build(c: &mut Criterion) {
    let mut g = c.benchmark_group("csr_build");
    for n_nodes in [50u32, 200, 500] {
        let s = synth(n_nodes, 1_000, 24, 16);
        g.throughput(Throughput::Elements(s.lane_ids.len() as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n_nodes), &s, |b, s| {
            b.iter(|| black_box(build_graph(s)))
        });
    }
    g.finish();
}

fn bench_static_router_build(c: &mut Criterion) {
    let mut g = c.benchmark_group("static_router_build");
    for n_nodes in [50u32, 200, 500] {
        let s = synth(n_nodes, 1_000, 24, 16);
        let graph = build_graph(&s);
        // Use all nodes as leaves (worst case for reverse Dijkstra).
        let leaves: Vec<u32> = (0..n_nodes).collect();
        g.bench_with_input(BenchmarkId::from_parameter(n_nodes), &(graph, leaves), |b, (g, d)| {
            b.iter(|| black_box(StaticRouter::new(g, Some(d.as_slice()))))
        });
    }
    g.finish();
}

fn bench_time_aware_first_hop(c: &mut Criterion) {
    let mut g = c.benchmark_group("time_aware_first_hop");
    for n_nodes in [50u32, 200, 500] {
        let s = synth(n_nodes, 1_000, 24, 16);
        let graph = build_graph(&s);
        let schedules = build_schedules(&s, &graph.lane_id_to_csr);
        let router = TimeAwareRouter::new(ATU_PER_HOUR);

        g.bench_with_input(BenchmarkId::from_parameter(n_nodes), &(graph, schedules, router), |b, (gph, sched, r)| {
            b.iter(|| {
                // Vary the (origin, dest, time) so the cache is exercised
                // realistically, not just the first-call cold path.
                let origin = 0u32;
                let dest = (n_nodes - 1) as u32;
                black_box(r.next_hop(origin, dest, ATU_PER_HOUR, gph, sched))
            })
        });
    }
    g.finish();
}

fn bench_event_queue(c: &mut Criterion) {
    let mut g = c.benchmark_group("event_queue");
    for n in [1_000usize, 10_000, 100_000] {
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::new("push_then_drain", n), &n, |b, &n| {
            b.iter(|| {
                let mut q = EventQueue::new();
                for i in 0..n {
                    q.push(Event {
                        time_atu: (i as i64) * 7 % 1_000_000,
                        kind: EventKind::Inject,
                        flow_unit_id: i as u64,
                        location: (i as u32) % 1024,
                        event_id: i as u64,
                    });
                }
                while let Some(e) = q.pop() {
                    black_box(e);
                }
            })
        });
    }
    g.finish();
}

fn bench_engine_run(c: &mut Criterion) {
    let mut g = c.benchmark_group("engine_run");
    g.sample_size(20);
    for &(n_nodes, n_demand) in &[(50u32, 1_000u32), (200, 10_000), (500, 10_000)] {
        let s = synth(n_nodes, n_demand, 24, 32);
        g.throughput(Throughput::Elements(n_demand as u64));
        g.bench_with_input(
            BenchmarkId::from_parameter(format!("{n_nodes}h_{n_demand}d")),
            &s,
            |b, s| {
                b.iter(|| {
                    let graph = build_graph(s);
                    let schedules = build_schedules(s, &graph.lane_id_to_csr);
                    let demand = build_demand(s);
                    let router: Box<dyn Router + Send + Sync> =
                        Box::new(TimeAwareRouter::new(ATU_PER_HOUR));
                    let mut engine = DESEngine::new(graph, schedules, demand, router, ATU_PER_HOUR);
                    engine.run(24 * ATU_PER_HOUR);
                    black_box(engine.total_delivered())
                })
            },
        );
    }
    g.finish();
}

fn bench_surface_emit(c: &mut Criterion) {
    let mut g = c.benchmark_group("surface_emit");
    for n_nodes in [50u32, 200, 500] {
        let s = synth(n_nodes, 1_000, 24, 16);
        g.throughput(Throughput::Elements(n_nodes as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n_nodes), &s, |b, s| {
            b.iter(|| {
                let graph = build_graph(s);
                let schedules = build_schedules(s, &graph.lane_id_to_csr);
                let demand = build_demand(s);
                let router: Box<dyn Router + Send + Sync> =
                    Box::new(TimeAwareRouter::new(ATU_PER_HOUR));
                let mut engine = DESEngine::new(graph, schedules, demand, router, ATU_PER_HOUR);
                // Run a short slice so emit has work to do without dominating.
                engine.run(2 * ATU_PER_HOUR);
                black_box(engine.surfaces.snapshots.len())
            })
        });
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_ipc_parsing,
    bench_csr_build,
    bench_static_router_build,
    bench_time_aware_first_hop,
    bench_event_queue,
    bench_engine_run,
    bench_surface_emit,
);
criterion_main!(benches);
