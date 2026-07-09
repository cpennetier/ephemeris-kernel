//! Pooled schedule store with O(log M) binary search.
//!
//! Sacred Contract B-3: sched_offsets[E+1] maps each CSR lane to its
//! range in the pooled departures_atu[] and capacities[] arrays.

use crate::types::{Atu, CapacityUnits, LaneIdx};

/// Pooled schedule store. Each CSR lane maps to a range of departures.
pub struct ScheduleStore {
    /// Per-CSR-lane range pointers: `sched_offsets[e]..sched_offsets[e+1]`.
    pub sched_offsets: Vec<u32>,
    /// Pooled departure times (sorted per lane).
    pub departures_atu: Vec<Atu>,
    /// Pooled capacities (parallel to departures).
    pub capacities: Vec<CapacityUnits>,
}

/// Result of a departure lookup.
#[derive(Debug, Clone, Copy)]
pub struct DepartureSlot {
    pub depart_atu: Atu,
    pub capacity: CapacityUnits,
    /// Global index into the pooled arrays (used for capacity booking).
    pub slot_idx: u32,
}

impl ScheduleStore {
    /// Build from flat schedule arrays, remapping original lane_ids to CSR positions.
    ///
    /// # Arguments
    /// * `num_csr_lanes` - Number of lanes in CSR (E).
    /// * `lane_ids` - Original lane_id per schedule event (from Arrow IPC).
    /// * `depart_atus` - Departure times per event.
    /// * `capacities_arr` - Capacities per event.
    /// * `lane_id_to_csr` - Mapping from original lane_id → CSR index.
    pub fn from_flat(
        num_csr_lanes: u32,
        lane_ids: &[u32],
        depart_atus: &[i64],
        capacities_arr: &[u32],
        lane_id_to_csr: &[u32],
    ) -> Self {
        let e = num_csr_lanes as usize;
        let s = lane_ids.len();

        // Count events per CSR lane
        let mut event_count = vec![0u32; e];
        for &lid in lane_ids {
            if (lid as usize) < lane_id_to_csr.len() {
                let csr_idx = lane_id_to_csr[lid as usize] as usize;
                if csr_idx < e {
                    event_count[csr_idx] += 1;
                }
            }
        }

        // Build sched_offsets via prefix sum
        let mut sched_offsets = vec![0u32; e + 1];
        for i in 0..e {
            sched_offsets[i + 1] = sched_offsets[i] + event_count[i];
        }

        let total_slots = sched_offsets[e] as usize;
        let mut departures_atu = vec![0i64; total_slots];
        let mut capacities = vec![0u32; total_slots];
        let mut cursor = sched_offsets[..e].to_vec();

        for i in 0..s {
            let lid = lane_ids[i];
            if (lid as usize) >= lane_id_to_csr.len() {
                continue;
            }
            let csr_idx = lane_id_to_csr[lid as usize] as usize;
            if csr_idx >= e {
                continue;
            }
            let pos = cursor[csr_idx] as usize;
            departures_atu[pos] = depart_atus[i];
            capacities[pos] = capacities_arr[i];
            cursor[csr_idx] += 1;
        }

        // Sort departures within each lane's range
        for csr_e in 0..e {
            let start = sched_offsets[csr_e] as usize;
            let end = sched_offsets[csr_e + 1] as usize;
            if start >= end {
                continue;
            }
            // Create index array, sort by departure time
            let mut indices: Vec<usize> = (start..end).collect();
            indices.sort_by_key(|&i| departures_atu[i]);
            let sorted_deps: Vec<i64> = indices.iter().map(|&i| departures_atu[i]).collect();
            let sorted_caps: Vec<u32> = indices.iter().map(|&i| capacities[i]).collect();
            for (offset, idx) in (start..end).enumerate() {
                departures_atu[idx] = sorted_deps[offset];
                capacities[idx] = sorted_caps[offset];
            }
        }

        ScheduleStore {
            sched_offsets,
            departures_atu,
            capacities,
        }
    }

    /// Find next departure for CSR lane at or after `after_atu`.
    /// O(log M) via binary search. Sacred Contract B-3.
    #[inline]
    pub fn next_departure(&self, csr_lane: LaneIdx, after_atu: Atu) -> Option<DepartureSlot> {
        let start = self.sched_offsets[csr_lane as usize] as usize;
        let end = self.sched_offsets[csr_lane as usize + 1] as usize;
        if start >= end {
            return None;
        }
        let slice = &self.departures_atu[start..end];
        let idx = slice.partition_point(|&dep| dep < after_atu);
        if idx < slice.len() {
            let global_idx = start + idx;
            Some(DepartureSlot {
                depart_atu: self.departures_atu[global_idx],
                capacity: self.capacities[global_idx],
                slot_idx: global_idx as u32,
            })
        } else {
            None
        }
    }

    /// Total number of departure slots.
    pub fn total_slots(&self) -> usize {
        self.departures_atu.len()
    }
}
