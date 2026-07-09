//! CSR graph built from Arrow IPC lane table.
//!
//! Sacred Contract B-1: CSR topology (node_offsets[N+1], arc_heads[E]).
//! Lanes are sorted by source node. O(1) adjacency lookup.

use crate::types::{Atu, Co2Grams, CostMicros, LaneIdx, NodeId};

/// Per-lane metadata stored in CSR order.
#[derive(Debug, Clone)]
pub struct LaneMeta {
    pub target: NodeId,
    pub lane_mode: u8,
    pub duration_atu: Atu,
    pub cost_micros: CostMicros,
    pub co2_grams: Co2Grams,
}

/// Compressed Sparse Row graph.
///
/// `node_offsets[u]..node_offsets[u+1]` gives the range of outgoing lanes
/// from node `u` in the `lanes` array.
pub struct CSRGraph {
    pub num_nodes: u32,
    pub num_lanes: u32,
    pub node_offsets: Vec<u32>,
    pub lanes: Vec<LaneMeta>,
    /// Maps original lane_id (from Python) → CSR position.
    /// `lane_id_to_csr[original_lane_id] = csr_index`.
    pub lane_id_to_csr: Vec<u32>,
}

impl CSRGraph {
    /// Build CSR from flat lane arrays (as read from Arrow IPC).
    ///
    /// # Arguments
    /// * `num_nodes` - Number of nodes (N).
    /// * `src_node_ids` - Source node ID per lane (original order).
    /// * `dst_node_ids` - Destination node ID per lane (original order).
    /// * `lane_ids` - Original lane_id per lane (for schedule mapping).
    /// * `lane_modes` - Transport mode per lane.
    /// * `duration_atus` - Transit time per lane in ATU.
    /// * `cost_micros` - Cost per lane in microdollars.
    /// * `co2_grams` - CO2 per lane in grams.
    pub fn from_lanes(
        num_nodes: u32,
        src_node_ids: &[u32],
        dst_node_ids: &[u32],
        lane_ids: &[u32],
        lane_modes: &[u8],
        duration_atus: &[i64],
        cost_micros_arr: &[i64],
        co2_grams_arr: &[i64],
    ) -> Self {
        let n = num_nodes as usize;
        let e = src_node_ids.len();

        // Count outgoing lanes per node
        let mut out_count = vec![0u32; n];
        for &src in src_node_ids {
            if (src as usize) < n {
                out_count[src as usize] += 1;
            }
        }

        // Build node_offsets via prefix sum
        let mut node_offsets = vec![0u32; n + 1];
        for i in 0..n {
            node_offsets[i + 1] = node_offsets[i] + out_count[i];
        }

        // Fill lanes in CSR order, build lane_id_to_csr mapping
        let total_lanes = node_offsets[n] as usize;
        let mut lanes = vec![
            LaneMeta {
                target: 0,
                lane_mode: 0,
                duration_atu: 0,
                cost_micros: 0,
                co2_grams: 0,
            };
            total_lanes
        ];

        let max_lane_id = lane_ids.iter().copied().max().unwrap_or(0) as usize;
        let mut lane_id_to_csr = vec![0u32; max_lane_id + 1];
        let mut cursor = node_offsets[..n].to_vec();

        for i in 0..e {
            let src = src_node_ids[i] as usize;
            if src >= n {
                continue;
            }
            let pos = cursor[src] as usize;
            lanes[pos] = LaneMeta {
                target: dst_node_ids[i],
                lane_mode: lane_modes[i],
                duration_atu: duration_atus[i],
                cost_micros: cost_micros_arr[i],
                co2_grams: co2_grams_arr[i],
            };
            lane_id_to_csr[lane_ids[i] as usize] = pos as u32;
            cursor[src] += 1;
        }

        // Sort lanes within each node's range by target (for binary search in router)
        // We need to update lane_id_to_csr after sorting.
        // Build a temporary reverse map: csr_pos → original_lane_id
        let mut csr_to_lane_id = vec![0u32; total_lanes];
        for (eid, &csr_pos) in lane_id_to_csr.iter().enumerate() {
            if (eid as u32) < e as u32 && (csr_pos as usize) < total_lanes {
                // Only set if this lane_id was actually used
                if lane_ids.contains(&(eid as u32)) {
                    csr_to_lane_id[csr_pos as usize] = eid as u32;
                }
            }
        }

        for u in 0..n {
            let start = node_offsets[u] as usize;
            let end = node_offsets[u + 1] as usize;
            if start >= end {
                continue;
            }
            // Create index array for sorting
            let mut indices: Vec<usize> = (start..end).collect();
            indices.sort_by_key(|&idx| lanes[idx].target);

            // Reorder lanes and csr_to_lane_id
            let sorted_lanes: Vec<LaneMeta> = indices.iter().map(|&i| lanes[i].clone()).collect();
            let sorted_eids: Vec<u32> = indices.iter().map(|&i| csr_to_lane_id[i]).collect();

            for (offset, idx) in (start..end).enumerate() {
                lanes[idx] = sorted_lanes[offset].clone();
                csr_to_lane_id[idx] = sorted_eids[offset];
            }
        }

        // Rebuild lane_id_to_csr from the sorted csr_to_lane_id
        for (csr_pos, &eid) in csr_to_lane_id.iter().enumerate() {
            if (eid as usize) < lane_id_to_csr.len() {
                lane_id_to_csr[eid as usize] = csr_pos as u32;
            }
        }

        CSRGraph {
            num_nodes,
            num_lanes: total_lanes as u32,
            node_offsets,
            lanes,
            lane_id_to_csr,
        }
    }

    /// Get outgoing lanes from node `u`.
    #[inline]
    pub fn out_lanes(&self, u: NodeId) -> &[LaneMeta] {
        let start = self.node_offsets[u as usize] as usize;
        let end = self.node_offsets[u as usize + 1] as usize;
        &self.lanes[start..end]
    }

    /// Get the CSR range for node `u`.
    #[inline]
    pub fn lane_range(&self, u: NodeId) -> (usize, usize) {
        let start = self.node_offsets[u as usize] as usize;
        let end = self.node_offsets[u as usize + 1] as usize;
        (start, end)
    }

    /// Find the source node for a CSR lane index via binary search on node_offsets.
    pub fn find_source(&self, csr_idx: LaneIdx) -> NodeId {
        let idx = csr_idx as usize;
        let pos = self
            .node_offsets
            .partition_point(|&offset| (offset as usize) <= idx);
        if pos == 0 { 0 } else { (pos - 1) as NodeId }
    }
}
