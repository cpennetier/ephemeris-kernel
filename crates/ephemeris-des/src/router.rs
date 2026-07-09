//! Router trait and StaticRouter (leaf-only reverse Dijkstra).
//!
//! Gateways and regionals are pure transit (sort and forward).
//! They never originate or terminate flow units.
//! The StaticRouter precomputes cost-optimal paths ignoring schedules and capacity.
//! This provides the "oracle baseline" — best-case performance.
//! The Router trait accepts time_atu to enable future Tier 3 planners
//! that consider schedule availability and remaining capacity.
//! See PRD-030 "Planner Integration" and ERD-032 "Planner Interface."

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::graph::CSRGraph;
use crate::schedule::ScheduleStore;
use crate::types::{Atu, LaneIdx, NodeId};

/// Router trait — returns the CSR lane index for the next hop.
///
/// `time_atu` and `schedules` enable time-dependent routing (TimeAwareRouter).
/// StaticRouter ignores them.
pub trait Router: Send + Sync {
    fn next_hop(
        &self,
        current: NodeId,
        target: NodeId,
        time_atu: Atu,
        graph: &CSRGraph,
        schedules: &ScheduleStore,
    ) -> Option<LaneIdx>;
}

/// Dijkstra state for priority queue.
#[derive(Copy, Clone, Eq, PartialEq)]
struct DState {
    cost: i64,
    node: u32,
}

impl Ord for DState {
    fn cmp(&self, other: &Self) -> Ordering {
        other.cost.cmp(&self.cost) // min-heap
    }
}

impl PartialOrd for DState {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Static router with leaf-only reverse-Dijkstra next-hop tables.
///
/// Table layout: `next_hop_table[leaf_index][node_id]` = next node on the
/// shortest path FROM node_id TOWARD that leaf.
///
/// Built by running Dijkstra on the reverse graph from each leaf.
/// D reverse Dijkstra runs. At D=500, N=5000: ~330ms, ~10MB.
pub struct StaticRouter {
    /// `next_hop_table[leaf_index][node_id]` = next node toward that leaf.
    next_hop_table: Vec<Vec<Option<NodeId>>>,
    /// Maps leaf node_id → index into next_hop_table.
    leaf_to_row: Vec<Option<usize>>,
    num_nodes: usize,
}

impl StaticRouter {
    /// Build routing table. Always leaf-only.
    ///
    /// # Arguments
    /// * `graph` — The CSR graph (forward lanes).
    /// * `leaf_node_ids` — Leaf-tier node IDs. If None, uses all nodes (test fallback).
    pub fn new(graph: &CSRGraph, leaf_node_ids: Option<&[NodeId]>) -> Self {
        let n = graph.num_nodes as usize;

        let leafs: Vec<NodeId> = match leaf_node_ids {
            Some(ids) => ids.to_vec(),
            None => (0..n as NodeId).collect(), // Test fallback: all nodes
        };

        // Build reverse adjacency list for reverse Dijkstra
        // reverse_adj[v] = list of (u, duration_atu) where lane u→v exists
        let mut reverse_adj: Vec<Vec<(NodeId, i64)>> = vec![Vec::new(); n];
        for u in 0..n {
            let (start, end) = graph.lane_range(u as NodeId);
            for csr_idx in start..end {
                let lane = &graph.lanes[csr_idx];
                reverse_adj[lane.target as usize].push((u as NodeId, lane.duration_atu));
            }
        }

        let num_leafs = leafs.len();
        let mut next_hop_table = Vec::with_capacity(num_leafs);
        let mut leaf_to_row = vec![None; n];

        for (row_idx, &leaf) in leafs.iter().enumerate() {
            leaf_to_row[leaf as usize] = Some(row_idx);
            let row = Self::reverse_dijkstra_next_hops(n, leaf, &reverse_adj, graph);
            next_hop_table.push(row);
        }

        StaticRouter {
            next_hop_table,
            leaf_to_row,
            num_nodes: n,
        }
    }

    /// Run reverse Dijkstra from `leaf` on the reverse graph.
    ///
    /// This computes shortest paths TO the leaf FROM all other nodes.
    /// The `successor` array records: for each node h, which neighbor of h
    /// is the next step on the shortest path from h toward leaf.
    fn reverse_dijkstra_next_hops(
        n: usize,
        leaf: NodeId,
        reverse_adj: &[Vec<(NodeId, i64)>],
        graph: &CSRGraph,
    ) -> Vec<Option<NodeId>> {
        let mut dist = vec![i64::MAX; n];
        let mut heap = BinaryHeap::new();
        // successor[h] = next node from h toward leaf on the shortest path
        let mut successor: Vec<Option<NodeId>> = vec![None; n];

        dist[leaf as usize] = 0;
        heap.push(DState {
            cost: 0,
            node: leaf,
        });

        while let Some(DState { cost, node: v }) = heap.pop() {
            if cost > dist[v as usize] {
                continue;
            }
            // Relax reverse lanes: for each u→v in forward graph (= v→u in reverse)
            for &(u, duration) in &reverse_adj[v as usize] {
                let new_cost = cost + duration;
                if new_cost < dist[u as usize] {
                    dist[u as usize] = new_cost;
                    // On the shortest path from u to leaf, the next hop from u is v
                    // (because we relaxed u via the reverse lane from v)
                    successor[u as usize] = Some(v);
                    heap.push(DState {
                        cost: new_cost,
                        node: u,
                    });
                }
            }
        }

        successor
    }
}

impl Router for StaticRouter {
    fn next_hop(
        &self,
        current: NodeId,
        target: NodeId,
        _time_atu: Atu,
        graph: &CSRGraph,
        _schedules: &ScheduleStore,
    ) -> Option<LaneIdx> {
        // target must be a leaf
        let row_idx = self.leaf_to_row.get(target as usize)?.as_ref()?;
        let next_node = self.next_hop_table[*row_idx]
            .get(current as usize)?
            .as_ref()?;

        // Find the CSR lane from current → next_node
        let (start, end) = graph.lane_range(current);
        let slice = &graph.lanes[start..end];
        // Lanes are sorted by target, use binary search
        if let Ok(idx) = slice.binary_search_by_key(next_node, |e| e.target) {
            Some((start + idx) as LaneIdx)
        } else {
            // Linear scan fallback (in case of duplicate targets)
            for (i, e) in slice.iter().enumerate() {
                if e.target == *next_node {
                    return Some((start + i) as LaneIdx);
                }
            }
            None
        }
    }
}
