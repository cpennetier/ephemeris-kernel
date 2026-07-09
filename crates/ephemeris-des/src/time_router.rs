//! TimeAwareRouter: schedule-aware earliest-arrival routing.
//!
//! Unlike StaticRouter which uses precomputed topology-only paths,
//! TimeAwareRouter computes the earliest-arrival path considering
//! actual departure schedules. A flow unit arriving at a node at 3pm
//! routes via the lane with the next available departure, not the
//! lane with the shortest static duration.
//!
//! Cache: routes are memoized by (current, dest, hour_bucket).
//! Flow units in the same hour on the same (current, dest) share a route.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use crate::graph::CSRGraph;
use crate::router::Router;
use crate::schedule::ScheduleStore;
use crate::types::{Atu, LaneIdx, NodeId};

/// Priority queue element for time-dependent Dijkstra.
#[derive(Copy, Clone, Eq, PartialEq)]
struct TDState {
    arrival: Atu,
    node: u32,
}

impl Ord for TDState {
    fn cmp(&self, other: &Self) -> Ordering {
        other.arrival.cmp(&self.arrival) // min-heap
    }
}

impl PartialOrd for TDState {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

type CacheKey = (NodeId, NodeId, u32); // (current, dest, time_bucket)

/// TimeAwareRouter with per-(current, dest, hour) caching.
pub struct TimeAwareRouter {
    bucket_width_atu: Atu,
    cache: RefCell<HashMap<CacheKey, Option<LaneIdx>>>,
}

// Safety: RefCell is only accessed from the simulation thread (single-threaded DES loop).
// The Router trait requires Send + Sync for the Box<dyn Router>, but the engine
// runs single-threaded. We enforce this with unsafe impl.
unsafe impl Send for TimeAwareRouter {}
unsafe impl Sync for TimeAwareRouter {}

impl TimeAwareRouter {
    /// Construct with cache bucket width (default 3_600_000 = 1 hour ATU).
    pub fn new(bucket_width_atu: Atu) -> Self {
        TimeAwareRouter {
            bucket_width_atu,
            cache: RefCell::new(HashMap::new()),
        }
    }
}

impl Router for TimeAwareRouter {
    fn next_hop(
        &self,
        current: NodeId,
        target: NodeId,
        time_atu: Atu,
        graph: &CSRGraph,
        schedules: &ScheduleStore,
    ) -> Option<LaneIdx> {
        let bucket = (time_atu / self.bucket_width_atu) as u32;
        let key = (current, target, bucket);

        if let Some(&result) = self.cache.borrow().get(&key) {
            return result;
        }

        let result =
            time_dependent_dijkstra_first_hop(current, target, time_atu, graph, schedules);

        self.cache.borrow_mut().insert(key, result);
        result
    }
}

/// Time-dependent Dijkstra returning only the first CSR lane on the
/// earliest-arrival path from `origin` to `dest` starting at `start_time`.
///
/// We store `(predecessor_node, lane_used)` per node so we can trace
/// back to origin and return the first hop.
fn time_dependent_dijkstra_first_hop(
    origin: NodeId,
    dest: NodeId,
    start_time: Atu,
    graph: &CSRGraph,
    schedules: &ScheduleStore,
) -> Option<LaneIdx> {
    if origin == dest {
        return None;
    }

    let n = graph.num_nodes as usize;
    let mut dist = vec![Atu::MAX; n];
    // (predecessor_node, csr_lane_used_to_reach_this_node)
    let mut pred: Vec<Option<(NodeId, LaneIdx)>> = vec![None; n];

    dist[origin as usize] = start_time;
    let mut heap = BinaryHeap::new();
    heap.push(TDState {
        arrival: start_time,
        node: origin,
    });

    while let Some(TDState { arrival, node: u }) = heap.pop() {
        if u == dest {
            // Trace back to find the first hop from origin
            let mut cur = dest;
            loop {
                let (prev, lane) = pred[cur as usize]?;
                if prev == origin {
                    return Some(lane);
                }
                cur = prev;
            }
        }

        if arrival > dist[u as usize] {
            continue;
        }

        let (start, end) = graph.lane_range(u);
        for csr_idx in start..end {
            let lane = &graph.lanes[csr_idx];
            let v = lane.target;

            let slot = match schedules.next_departure(csr_idx as LaneIdx, arrival) {
                Some(s) => s,
                None => continue,
            };

            let t_arrive_v = slot.depart_atu + lane.duration_atu;

            if t_arrive_v < dist[v as usize] {
                dist[v as usize] = t_arrive_v;
                pred[v as usize] = Some((u, csr_idx as LaneIdx));
                heap.push(TDState {
                    arrival: t_arrive_v,
                    node: v,
                });
            }
        }
    }

    None
}
