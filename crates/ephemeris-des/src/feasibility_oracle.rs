//! Offline feasibility oracle — exact-time earliest-arrival over the timetable.
//!
//! This is a READ-ONLY analysis helper. It does NOT touch routing or strand
//! behavior; it answers the schedule-SUPPLY question for a completed run:
//! "ignoring capacity, does the timetable admit a path from (origin, release)
//! to dest, and does it arrive on time?"
//!
//! It REUSES the production time-dependent Dijkstra relaxation verbatim
//! (`time_router.rs:138-152`):
//!
//! ```text
//! slot       = schedules.next_departure(lane, arrival)   // wait for departure
//! t_arrive_v = slot.depart_atu + lane.duration_atu        // + transit
//! ```
//!
//! Differences from `time_dependent_dijkstra_first_hop` (by design, per spec):
//!   - EXACT time — there is no hour-bucket here (the bucket only ever lived in
//!     the router's CACHE KEY, not in the relaxation, so this is identical math).
//!   - RUN TO COMPLETION — settles every node, returning arrival to ALL dests
//!     from one source (no early-termination at a single dest).
//!   - CAPACITY IGNORED — same as the router's Dijkstra, which never reads
//!     capacity; the oracle asks the pure schedule-supply question.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::graph::CSRGraph;
use crate::schedule::ScheduleStore;
use crate::types::{Atu, LaneIdx, NodeId};

/// Min-heap element (mirror of `time_router::TDState`; that type is private and
/// `time_router.rs` is intentionally untouched).
#[derive(Copy, Clone, Eq, PartialEq)]
struct OracleState {
    arrival: Atu,
    node: u32,
}

impl Ord for OracleState {
    fn cmp(&self, other: &Self) -> Ordering {
        other.arrival.cmp(&self.arrival) // min-heap on arrival
    }
}

impl PartialOrd for OracleState {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Exact-time earliest-arrival from `(origin, release_atu)` to EVERY node over the
/// full timetable, capacity ignored, run to completion.
///
/// Returns a `Vec<Atu>` of length `graph.num_nodes` where `out[dest]` is the
/// earliest arrival time at `dest`, or `Atu::MAX` if `dest` is unreachable from
/// `origin` at or after `release_atu`. `out[origin] == release_atu`.
pub fn earliest_arrival_all_dests(
    origin: NodeId,
    release_atu: Atu,
    graph: &CSRGraph,
    schedules: &ScheduleStore,
) -> Vec<Atu> {
    let n = graph.num_nodes as usize;
    let mut dist = vec![Atu::MAX; n];
    dist[origin as usize] = release_atu;

    let mut heap = BinaryHeap::new();
    heap.push(OracleState { arrival: release_atu, node: origin });

    while let Some(OracleState { arrival, node: u }) = heap.pop() {
        if arrival > dist[u as usize] {
            continue; // stale heap entry
        }
        let (start, end) = graph.lane_range(u);
        for csr_idx in start..end {
            let lane = &graph.lanes[csr_idx];
            let v = lane.target;

            // SAME relaxation as the production router: wait for the next
            // scheduled departure at/after `arrival`, then add transit.
            let slot = match schedules.next_departure(csr_idx as LaneIdx, arrival) {
                Some(s) => s,
                None => continue,
            };
            let t_arrive_v = slot.depart_atu + lane.duration_atu;

            if t_arrive_v < dist[v as usize] {
                dist[v as usize] = t_arrive_v;
                heap.push(OracleState { arrival: t_arrive_v, node: v });
            }
        }
    }

    dist
}

// ────────────────────────────────────────────────────────────────────────
// Step 2: destination-rooted backward profiles (profile Connection Scan).
//
// One backward scan over the time-sorted connection array per DESTINATION
// yields, for every node, the Pareto profile of (depart_atu → earliest arrival
// at dest). Every flow unit verdict to that destination is then one binary
// search — the per-(origin, release) source-Dijkstra loop (measured 26.5 s at
// 1M, 493,824 distinct keys, ~0% cache hits) becomes O(K·|C| log P) one-time
// + O(log P) per flow unit.
//
// Exactness vs the production relaxation: identical math
// (`next_departure(lane, arrival).depart_atu + duration`), with one stated
// assumption — `duration_atu >= 1` on every lane, so two connections with the
// SAME depart time can never chain (the single descending pass would miss
// zero-duration same-instant chains). The testkit and Sacred Contract use
// strictly positive durations; the cross-check test pins agreement with
// `earliest_arrival_all_dests`.
// ────────────────────────────────────────────────────────────────────────

/// Global connection array sorted by depart_atu DESC (ties: csr_lane ASC,
/// slot ASC) with the per-connection source node precomputed. Build once,
/// reuse across all destinations.
pub struct ConnectionIndex {
    /// (depart_atu, csr_lane, source_node) sorted by depart DESC.
    conns: Vec<(Atu, LaneIdx, NodeId)>,
}

impl ConnectionIndex {
    pub fn build(graph: &CSRGraph, schedules: &ScheduleStore) -> Self {
        let mut conns: Vec<(Atu, LaneIdx, NodeId)> = Vec::with_capacity(
            schedules.total_slots(),
        );
        for e in 0..graph.num_lanes as usize {
            let src = graph.find_source(e as LaneIdx);
            let start = schedules.sched_offsets[e] as usize;
            let end = schedules.sched_offsets[e + 1] as usize;
            for s in start..end {
                conns.push((schedules.departures_atu[s], e as LaneIdx, src));
            }
        }
        // DESC by depart; deterministic tiebreak on csr_lane.
        conns.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        ConnectionIndex { conns }
    }
}

/// Per-node Pareto profile to one destination: pairs `(depart_atu, arrival_at
/// dest)` stored in depart-DESC push order (so arrival is DESC too). The
/// earliest arrival leaving node `u` at/after `t` is `eval(&prof[u], t)`.
pub type DestProfile = Vec<Vec<(Atu, Atu)>>;

/// Earliest arrival at the profile's destination when present at this node at
/// time `t`; `Atu::MAX` if no remaining departure chain reaches it.
#[inline]
pub fn eval_profile(pairs: &[(Atu, Atu)], t: Atu) -> Atu {
    // pairs sorted by depart DESC: the prefix has depart >= t; its LAST
    // element is the smallest depart >= t, which (Pareto) has the smallest
    // arrival.
    let idx = pairs.partition_point(|&(dep, _)| dep >= t);
    if idx == 0 { Atu::MAX } else { pairs[idx - 1].1 }
}

/// Build the backward profile for `dest` over the full timetable.
pub fn build_dest_profile(
    dest: NodeId,
    graph: &CSRGraph,
    index: &ConnectionIndex,
) -> DestProfile {
    let n = graph.num_nodes as usize;
    let mut prof: DestProfile = vec![Vec::new(); n];
    for &(dep, e, src) in &index.conns {
        let lane = &graph.lanes[e as usize];
        let arr_v = dep + lane.duration_atu;
        let tau = if lane.target == dest {
            arr_v
        } else {
            eval_profile(&prof[lane.target as usize], arr_v)
        };
        if tau == Atu::MAX {
            continue;
        }
        let pairs = &mut prof[src as usize];
        match pairs.last_mut() {
            None => pairs.push((dep, tau)),
            Some(last) => {
                if last.0 == dep {
                    if tau < last.1 {
                        last.1 = tau;
                    }
                } else if tau < last.1 {
                    pairs.push((dep, tau));
                }
            }
        }
    }
    prof
}

/// Bulk verdict codes (mirrored by consumers' Python-side oracles).
pub const VERDICT_ON_TIME: u8 = 0;
pub const VERDICT_LATE: u8 = 1;
pub const VERDICT_INFEASIBLE_CLIFF: u8 = 2;
pub const VERDICT_INFEASIBLE_STRUCTURAL: u8 = 3;

/// Bulk oracle: for parallel flow unit arrays, return (earliest_arrival, verdict)
/// per flow unit. Groups flow units by destination; one profile build per distinct
/// destination, one `eval_profile` per flow unit. Deterministic: grouping is by
/// sorted destination id, output order is input order.
pub fn bulk_verdicts(
    graph: &CSRGraph,
    schedules: &ScheduleStore,
    origins: &[NodeId],
    dests: &[NodeId],
    releases: &[Atu],
    dues: &[Atu],
) -> (Vec<Atu>, Vec<u8>) {
    let m = origins.len();
    let mut arrivals = vec![Atu::MAX; m];
    let mut verdicts = vec![VERDICT_INFEASIBLE_STRUCTURAL; m];

    // Group flow unit indices by destination (sorted for determinism).
    let mut by_dest: std::collections::BTreeMap<NodeId, Vec<usize>> =
        std::collections::BTreeMap::new();
    for i in 0..m {
        by_dest.entry(dests[i]).or_default().push(i);
    }

    let index = ConnectionIndex::build(graph, schedules);
    for (dest, idxs) in by_dest {
        let prof = build_dest_profile(dest, graph, &index);
        for i in idxs {
            let o = origins[i] as usize;
            if origins[i] == dest {
                arrivals[i] = releases[i];
                verdicts[i] = if releases[i] <= dues[i] {
                    VERDICT_ON_TIME
                } else {
                    VERDICT_LATE
                };
                continue;
            }
            let pairs = match prof.get(o) {
                Some(p) => p.as_slice(),
                None => &[],
            };
            let arr = eval_profile(pairs, releases[i]);
            arrivals[i] = arr;
            verdicts[i] = if arr == Atu::MAX {
                if pairs.is_empty() {
                    VERDICT_INFEASIBLE_STRUCTURAL // never reachable
                } else {
                    VERDICT_INFEASIBLE_CLIFF // reachable for earlier releases
                }
            } else if arr <= dues[i] {
                VERDICT_ON_TIME
            } else {
                VERDICT_LATE
            };
        }
    }
    (arrivals, verdicts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{CSRGraph, LaneMeta};
    use crate::schedule::ScheduleStore;

    fn line_graph() -> (CSRGraph, ScheduleStore) {
        // 3 nodes: 0 -> 1 (dur 1000), 1 -> 2 (dur 2000). Node 2 also reachable.
        let graph = CSRGraph {
            num_nodes: 3,
            num_lanes: 2,
            node_offsets: vec![0, 1, 2, 2],
            lanes: vec![
                LaneMeta { target: 1, lane_mode: 0, duration_atu: 1000, cost_micros: 0, co2_grams: 0 },
                LaneMeta { target: 2, lane_mode: 0, duration_atu: 2000, cost_micros: 0, co2_grams: 0 },
            ],
            lane_id_to_csr: vec![0, 1],
        };
        // CSR lane 0 departs at 5000; CSR lane 1 departs at 7000.
        let schedules = ScheduleStore {
            sched_offsets: vec![0, 1, 2, 2],
            departures_atu: vec![5000, 7000],
            capacities: vec![100, 100],
        };
        (graph, schedules)
    }

    #[test]
    fn earliest_arrival_reaches_all_dests_via_schedule() {
        let (g, s) = line_graph();
        let d = earliest_arrival_all_dests(0, 0, &g, &s);
        assert_eq!(d[0], 0); // origin = release
        assert_eq!(d[1], 6000); // depart 5000 + transit 1000
        assert_eq!(d[2], 9000); // arrive hub1 at 6000, depart 7000 + transit 2000
    }

    #[test]
    fn unreachable_when_release_after_last_departure() {
        let (g, s) = line_graph();
        // Release after lane 0's only departure (5000): node 1 & 2 unreachable.
        let d = earliest_arrival_all_dests(0, 6000, &g, &s);
        assert_eq!(d[0], 6000);
        assert_eq!(d[1], Atu::MAX);
        assert_eq!(d[2], Atu::MAX);
    }

    /// Property: profile evaluation must agree with the source-Dijkstra on
    /// every (origin, release, dest) of the line graph at several releases.
    #[test]
    fn profile_agrees_with_dijkstra_on_line_graph() {
        let (g, s) = line_graph();
        let index = ConnectionIndex::build(&g, &s);
        for dest in 0..3u32 {
            let prof = build_dest_profile(dest, &g, &index);
            for origin in 0..3u32 {
                if origin == dest {
                    continue;
                }
                for release in [0, 4_000, 5_000, 5_001, 6_000, 6_001, 7_001] {
                    let dij = earliest_arrival_all_dests(origin, release, &g, &s);
                    let via_profile = eval_profile(&prof[origin as usize], release);
                    assert_eq!(
                        via_profile, dij[dest as usize],
                        "profile vs dijkstra mismatch o={origin} d={dest} t={release}"
                    );
                }
            }
        }
    }

    #[test]
    fn bulk_verdicts_split_cliff_vs_structural() {
        let (g, s) = line_graph();
        // Flow unit 0: 0→2 release 0, due 9000  → ON_TIME (arrives 9000).
        // Flow unit 1: 0→2 release 0, due 8999  → LATE.
        // Flow unit 2: 0→2 release 6000         → CLIFF (path existed earlier).
        // Flow unit 3: 2→0 release 0            → STRUCTURAL (no lane 2→0 ever).
        let (arr, ver) = bulk_verdicts(
            &g, &s,
            &[0, 0, 0, 2],
            &[2, 2, 2, 0],
            &[0, 0, 6_000, 0],
            &[9_000, 8_999, 20_000, 20_000],
        );
        assert_eq!(arr[0], 9_000);
        assert_eq!(ver[0], VERDICT_ON_TIME);
        assert_eq!(ver[1], VERDICT_LATE);
        assert_eq!(arr[2], Atu::MAX);
        assert_eq!(ver[2], VERDICT_INFEASIBLE_CLIFF);
        assert_eq!(ver[3], VERDICT_INFEASIBLE_STRUCTURAL);
    }

    #[test]
    fn bulk_verdicts_origin_equals_dest() {
        let (g, s) = line_graph();
        let (arr, ver) = bulk_verdicts(&g, &s, &[1], &[1], &[42], &[42]);
        assert_eq!(arr[0], 42);
        assert_eq!(ver[0], VERDICT_ON_TIME);
    }

    #[test]
    fn isolated_node_is_unreachable() {
        // Node 2 has no inbound schedule path from 0 if lane 1 never departs.
        let g = CSRGraph {
            num_nodes: 3,
            num_lanes: 1,
            node_offsets: vec![0, 1, 1, 1],
            lanes: vec![LaneMeta { target: 1, lane_mode: 0, duration_atu: 1000, cost_micros: 0, co2_grams: 0 }],
            lane_id_to_csr: vec![0],
        };
        let s = ScheduleStore { sched_offsets: vec![0, 1, 1, 1], departures_atu: vec![5000], capacities: vec![100] };
        let d = earliest_arrival_all_dests(0, 0, &g, &s);
        assert_eq!(d[1], 6000);
        assert_eq!(d[2], Atu::MAX); // genuinely no path
    }
}
