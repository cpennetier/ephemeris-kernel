//! Arrow IPC reader for node, lane, schedule, and demand tables.
//!
//! Replaces JSON loading from the snippet. Reads Arrow IPC stream format
//! produced by the Python backend (pyarrow.ipc.new_stream).

use arrow::array::*;
use arrow::datatypes::DataType;
use arrow::ipc::reader::StreamReader;
use std::io::Cursor;

use crate::demand::DemandQueue;
use crate::graph::CSRGraph;
use crate::schedule::ScheduleStore;

/// Error type for IO operations.
#[derive(Debug, thiserror::Error)]
pub enum IOError {
    #[error("Arrow error: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    #[error("Missing column: {0}")]
    MissingColumn(String),
    #[error("Type mismatch for column {0}: expected {1}")]
    TypeMismatch(String, String),
    #[error("Invalid node data: {0}")]
    InvalidData(String),
}

/// Helper: extract a typed column from a RecordBatch.
macro_rules! extract_col {
    ($batch:expr, $name:expr, $array_type:ty) => {{
        let col = $batch
            .column_by_name($name)
            .ok_or_else(|| IOError::MissingColumn($name.to_string()))?;
        col.as_any()
            .downcast_ref::<$array_type>()
            .ok_or_else(|| {
                IOError::TypeMismatch(
                    $name.to_string(),
                    format!("{:?}", col.data_type()),
                )
            })?
    }};
}

/// Read lane Arrow IPC and build CSR graph.
///
/// Expected schema: lane_id u32, src_node_id u32, dst_node_id u32,
/// lane_mode u8, duration_atu i64, cost_usd_micros i64, co2_g i64.
pub fn read_lanes_to_graph(ipc_bytes: &[u8], num_nodes: u32) -> Result<CSRGraph, IOError> {
    let cursor = Cursor::new(ipc_bytes);
    let reader = StreamReader::try_new(cursor, None)?;

    let mut all_lane_ids = Vec::new();
    let mut all_src = Vec::new();
    let mut all_dst = Vec::new();
    let mut all_mode = Vec::new();
    let mut all_dur = Vec::new();
    let mut all_cost = Vec::new();
    let mut all_co2 = Vec::new();

    for batch_result in reader {
        let batch = batch_result?;
        let lane_id = extract_col!(batch, "lane_id", UInt32Array);
        let src = extract_col!(batch, "src_node_id", UInt32Array);
        let dst = extract_col!(batch, "dst_node_id", UInt32Array);
        let mode = extract_col!(batch, "lane_mode", UInt8Array);
        let dur = extract_col!(batch, "duration_atu", Int64Array);
        let cost = extract_col!(batch, "cost_usd_micros", Int64Array);
        let co2 = extract_col!(batch, "co2_g", Int64Array);

        for i in 0..batch.num_rows() {
            all_lane_ids.push(lane_id.value(i));
            all_src.push(src.value(i));
            all_dst.push(dst.value(i));
            all_mode.push(mode.value(i));
            all_dur.push(dur.value(i));
            all_cost.push(cost.value(i));
            all_co2.push(co2.value(i));
        }
    }

    Ok(CSRGraph::from_lanes(
        num_nodes,
        &all_src,
        &all_dst,
        &all_lane_ids,
        &all_mode,
        &all_dur,
        &all_cost,
        &all_co2,
    ))
}

/// Read schedule Arrow IPC.
///
/// Expected schema: lane_id u32, depart_atu i64, capacity u32.
pub fn read_schedules(
    ipc_bytes: &[u8],
    num_csr_lanes: u32,
    lane_id_to_csr: &[u32],
) -> Result<ScheduleStore, IOError> {
    let cursor = Cursor::new(ipc_bytes);
    let reader = StreamReader::try_new(cursor, None)?;

    let mut all_lane = Vec::new();
    let mut all_dep = Vec::new();
    let mut all_cap = Vec::new();

    for batch_result in reader {
        let batch = batch_result?;
        let lane = extract_col!(batch, "lane_id", UInt32Array);
        let dep = extract_col!(batch, "depart_atu", Int64Array);
        let cap = extract_col!(batch, "capacity", UInt32Array);

        for i in 0..batch.num_rows() {
            all_lane.push(lane.value(i));
            all_dep.push(dep.value(i));
            all_cap.push(cap.value(i));
        }
    }

    Ok(ScheduleStore::from_flat(
        num_csr_lanes,
        &all_lane,
        &all_dep,
        &all_cap,
        lane_id_to_csr,
    ))
}

/// Read demand Arrow IPC.
///
/// Expected schema: flow_unit_id u64, appear_atu i64, due_atu i64,
/// origin_node_id u32, dest_node_id u32, size_units u32, priority_class u8,
/// handling_flags u8.
pub fn read_demand(ipc_bytes: &[u8]) -> Result<DemandQueue, IOError> {
    let cursor = Cursor::new(ipc_bytes);
    let reader = StreamReader::try_new(cursor, None)?;

    let mut sids = Vec::new();
    let mut appears = Vec::new();
    let mut dues = Vec::new();
    let mut origins = Vec::new();
    let mut dests = Vec::new();
    let mut sizes = Vec::new();
    let mut priorities = Vec::new();
    let mut handlings = Vec::new();

    for batch_result in reader {
        let batch = batch_result?;
        let sid = extract_col!(batch, "flow_unit_id", UInt64Array);
        let appear = extract_col!(batch, "appear_atu", Int64Array);
        let due = extract_col!(batch, "due_atu", Int64Array);
        let origin = extract_col!(batch, "origin_node_id", UInt32Array);
        let dest = extract_col!(batch, "dest_node_id", UInt32Array);
        let size = extract_col!(batch, "size_units", UInt32Array);
        let prio = extract_col!(batch, "priority_class", UInt8Array);
        let hand = extract_col!(batch, "handling_flags", UInt8Array);

        for i in 0..batch.num_rows() {
            sids.push(sid.value(i));
            appears.push(appear.value(i));
            dues.push(due.value(i));
            origins.push(origin.value(i));
            dests.push(dest.value(i));
            sizes.push(size.value(i));
            priorities.push(prio.value(i));
            handlings.push(hand.value(i));
        }
    }

    Ok(DemandQueue::from_arrays(
        &sids, &appears, &dues, &origins, &dests, &sizes, &priorities, &handlings,
    ))
}

/// Per-node → view-cell (res-3 R_UI) rollup lookup, built once at scenario load
/// from the atlas-pinned columns the node IPC carries (`cell_id_r3`,
/// `cell_centroid_lat`, `cell_centroid_lon`).
///
/// The DES engine performs NO H3 math: the node→cell mapping is computed at
/// artifact-build time (by the consumer's scenario serializer) and read
/// here as ordinary data. Sacred Contract I-D-005 (atlas sealed; runtime must
/// not call H3) and D-6 (node→cell mapping is artifact-pinned, not recomputed).
#[derive(Debug, Clone, Default)]
pub struct NodeCellLookup {
    /// `node_id` → dense cell index in `[0, num_cells)`.
    pub node_to_cell: Vec<u32>,
    /// dense cell index → res-3 H3 cell id (u64).
    pub cell_h3: Vec<u64>,
    /// dense cell index → res-3 parent centroid latitude.
    pub cell_centroid_lat: Vec<f64>,
    /// dense cell index → res-3 parent centroid longitude.
    pub cell_centroid_lon: Vec<f64>,
}

impl NodeCellLookup {
    pub fn num_nodes(&self) -> usize {
        self.node_to_cell.len()
    }
    pub fn num_cells(&self) -> usize {
        self.cell_h3.len()
    }
}

/// Read the atlas-pinned node→cell rollup columns from the node Arrow IPC and
/// build a [`NodeCellLookup`].
///
/// Dense cell order is the ascending sort of the distinct `cell_id_r3` values,
/// so the mapping is fully deterministic given the node set (Sacred Contract
/// G-3 dense-index discipline; the surface writer relies on this for stable
/// cell-indexed columns across buckets).
///
/// # Errors
///
/// Returns [`IOError::MissingColumn`] with a regeneration hint if the node IPC
/// predates schema v2 and lacks `cell_id_r3`. There is deliberately NO fallback
/// to runtime H3 computation. Returns [`IOError::InvalidData`] if `node_id` is
/// not dense `[0, num_nodes)`.
pub fn read_node_cell_lookup(ipc_bytes: &[u8]) -> Result<NodeCellLookup, IOError> {
    let cursor = Cursor::new(ipc_bytes);
    let reader = StreamReader::try_new(cursor, None)?;

    // (node_id, cell_h3, centroid_lat, centroid_lon) per row.
    let mut rows: Vec<(u32, u64, f64, f64)> = Vec::new();
    for batch_result in reader {
        let batch = batch_result?;
        if batch.column_by_name("cell_id_r3").is_none() {
            return Err(IOError::MissingColumn(
                "cell_id_r3 — node IPC predates schema v2; regenerate the node IPC \
                 with the current scenario serializer"
                    .to_string(),
            ));
        }
        let node_id = extract_col!(batch, "node_id", UInt32Array);
        let cell_h3 = extract_col!(batch, "cell_id_r3", UInt64Array);
        let lat = extract_col!(batch, "cell_centroid_lat", Float64Array);
        let lon = extract_col!(batch, "cell_centroid_lon", Float64Array);
        for i in 0..batch.num_rows() {
            rows.push((node_id.value(i), cell_h3.value(i), lat.value(i), lon.value(i)));
        }
    }

    let num_nodes = rows.len();

    // Dense cell order = ascending distinct cell_h3 (deterministic).
    let mut distinct: Vec<u64> = rows.iter().map(|r| r.1).collect();
    distinct.sort_unstable();
    distinct.dedup();

    let mut idx_of: std::collections::HashMap<u64, u32> =
        std::collections::HashMap::with_capacity(distinct.len());
    for (i, h) in distinct.iter().enumerate() {
        idx_of.insert(*h, i as u32);
    }

    let mut cell_centroid_lat = vec![0.0f64; distinct.len()];
    let mut cell_centroid_lon = vec![0.0f64; distinct.len()];
    let mut node_to_cell = vec![0u32; num_nodes];

    for (node_id, h3, lat, lon) in &rows {
        let hid = *node_id as usize;
        if hid >= num_nodes {
            return Err(IOError::InvalidData(format!(
                "node_id {node_id} out of range for {num_nodes} nodes (node_ids must be dense [0,N))"
            )));
        }
        let tidx = idx_of[h3];
        node_to_cell[hid] = tidx;
        // All nodes sharing a res-3 parent carry that parent's centroid, so this
        // write is idempotent across contributing nodes.
        cell_centroid_lat[tidx as usize] = *lat;
        cell_centroid_lon[tidx as usize] = *lon;
    }

    Ok(NodeCellLookup {
        node_to_cell,
        cell_h3: distinct,
        cell_centroid_lat,
        cell_centroid_lon,
    })
}

/// Count nodes from node Arrow IPC (just reads num_rows).
pub fn count_nodes(ipc_bytes: &[u8]) -> Result<u32, IOError> {
    let cursor = Cursor::new(ipc_bytes);
    let reader = StreamReader::try_new(cursor, None)?;
    let mut count = 0u32;
    for batch_result in reader {
        let batch = batch_result?;
        count += batch.num_rows() as u32;
    }
    Ok(count)
}

/// Extract leaf node IDs from node Arrow IPC.
pub fn extract_leaf_node_ids(ipc_bytes: &[u8]) -> Result<Vec<u32>, IOError> {
    let cursor = Cursor::new(ipc_bytes);
    let reader = StreamReader::try_new(cursor, None)?;
    let mut leaf_ids = Vec::new();

    for batch_result in reader {
        let batch = batch_result?;
        let node_id = extract_col!(batch, "node_id", UInt32Array);
        let tier = extract_col!(batch, "tier", StringArray);

        for i in 0..batch.num_rows() {
            if tier.value(i) == "leaf" {
                leaf_ids.push(node_id.value(i));
            }
        }
    }
    Ok(leaf_ids)
}

#[cfg(test)]
mod node_cell_lookup_tests {
    use super::*;
    use arrow::array::{Float64Array, RecordBatch, UInt32Array, UInt64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::writer::StreamWriter;
    use std::sync::Arc;

    fn node_ipc(node_ids: &[u32], cell_h3: &[u64], lat: &[f64], lon: &[f64]) -> Vec<u8> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("node_id", DataType::UInt32, false),
            Field::new("cell_id_r3", DataType::UInt64, false),
            Field::new("cell_centroid_lat", DataType::Float64, false),
            Field::new("cell_centroid_lon", DataType::Float64, false),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(UInt32Array::from(node_ids.to_vec())),
                Arc::new(UInt64Array::from(cell_h3.to_vec())),
                Arc::new(Float64Array::from(lat.to_vec())),
                Arc::new(Float64Array::from(lon.to_vec())),
            ],
        )
        .unwrap();
        let mut buf = Vec::new();
        {
            let mut w = StreamWriter::try_new(&mut buf, &schema).unwrap();
            w.write(&batch).unwrap();
            w.finish().unwrap();
        }
        buf
    }

    #[test]
    fn groups_nodes_by_res3_parent_with_sorted_dense_order() {
        // nodes 0,2 -> cell 100; nodes 1,3 -> cell 50. Distinct ascending = [50,100],
        // so cell_idx 0 == 50, cell_idx 1 == 100.
        let ipc = node_ipc(
            &[0, 1, 2, 3],
            &[100, 50, 100, 50],
            &[10.0, 20.0, 10.0, 20.0],
            &[-10.0, -20.0, -10.0, -20.0],
        );
        let lut = read_node_cell_lookup(&ipc).unwrap();

        assert_eq!(lut.num_nodes(), 4);
        assert_eq!(lut.num_cells(), 2);
        assert_eq!(lut.cell_h3, vec![50, 100]); // ascending, deterministic
        assert_eq!(lut.node_to_cell, vec![1, 0, 1, 0]);
        // Centroids align with the dense cell order.
        assert_eq!(lut.cell_centroid_lat, vec![20.0, 10.0]);
        assert_eq!(lut.cell_centroid_lon, vec![-20.0, -10.0]);
    }

    #[test]
    fn one_cell_per_node_when_all_distinct() {
        let ipc = node_ipc(&[0, 1, 2], &[7, 3, 5], &[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]);
        let lut = read_node_cell_lookup(&ipc).unwrap();
        assert_eq!(lut.cell_h3, vec![3, 5, 7]);
        // hub0->7(idx2), hub1->3(idx0), hub2->5(idx1)
        assert_eq!(lut.node_to_cell, vec![2, 0, 1]);
    }

    #[test]
    fn missing_cell_column_errors_with_hint() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "node_id",
            DataType::UInt32,
            false,
        )]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(UInt32Array::from(vec![0u32, 1]))],
        )
        .unwrap();
        let mut buf = Vec::new();
        {
            let mut w = StreamWriter::try_new(&mut buf, &schema).unwrap();
            w.write(&batch).unwrap();
            w.finish().unwrap();
        }
        let err = read_node_cell_lookup(&buf).unwrap_err();
        assert!(
            matches!(&err, IOError::MissingColumn(m) if m.contains("cell_id_r3") && m.contains("regenerate")),
            "expected a regeneration hint, got: {err}"
        );
    }

    #[test]
    fn empty_node_set_yields_empty_lookup() {
        let ipc = node_ipc(&[], &[], &[], &[]);
        let lut = read_node_cell_lookup(&ipc).unwrap();
        assert_eq!(lut.num_nodes(), 0);
        assert_eq!(lut.num_cells(), 0);
    }
}
