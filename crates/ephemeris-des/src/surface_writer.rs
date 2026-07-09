//! `SurfaceBlock` Arrow IPC writer.
//!
//! Encodes per-bucket [`SurfaceSnapshot`] data as an Arrow IPC stream so
//! the UI can consume it zero-copy via Apache Arrow JS (Session 2 wires the
//! browser-side decoder; this module ships the producer side).
//!
//! # Schema
//!
//! One Arrow record batch per bucket. Each batch has one row. Per-node and
//! per-lane arrays are encoded as `List<T>` columns; KPIs are scalar columns.
//! The node-indexed columns all share the same length (`num_nodes`); the
//! lane-indexed columns all share `num_lanes`. The schema is fixed (column
//! order is load-bearing for the TypeScript reader).
//!
//! # Cell rollups (columns 18..25)
//!
//! Per-view-cell (res-3 R_UI) rollups are emitted here from the atlas-pinned
//! [`NodeCellLookup`] (built by `io::read_node_cell_lookup` from the v2 node IPC).
//! The writer performs NO H3 math — the node→cell mapping and cell centroids are
//! data. Cell-indexed columns share length `num_cells` and are ordered by the
//! lookup's deterministic dense cell order (ascending `cell_id_r3`). Reductions
//! are SUM (load/queue/arrivals/departures), which are commutative, so child
//! iteration order does not affect the result. `cell_status` is WORST-OF
//! (max) of child node statuses; v1 has no per-node status, so it is all zeros.
//!
//! # Out of scope for this writer
//!
//! - `failed_by_reason` per-bucket counts. The DES engine does not yet track
//!   strand reasons at bucket granularity (only at run-end via KPIs). The
//!   schema reserves no column for this until the engine tracks it.
//!
//! # Sacred Contract
//!
//! G-1: time in ATU i64 ms. G-2: capacity units u32. G-3: dense IDs.

use std::sync::Arc;

use arrow::array::{
    ArrayRef, Float64Builder, Int64Array, ListBuilder, RecordBatch, UInt32Array, UInt32Builder,
    UInt64Array, UInt64Builder, UInt8Builder,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::error::ArrowError;
use arrow::ipc::writer::StreamWriter;

use sha2::{Digest, Sha256};

use crate::io::NodeCellLookup;
use crate::surfaces::SurfaceSnapshot;
use crate::types::{Atu, NodeId};

/// Surface-frame schema version. Bumped manually on any schema change (column
/// add/remove/reorder/type). Embedded in the Arrow schema metadata under
/// `schema_version`; mirrored by the TypeScript reader.
pub const SCHEMA_VERSION: &str = "2026.05.surface.v1";

/// Deterministic schema fingerprint: SHA-256 of the column names sorted
/// ascending and joined with newlines, truncated to 16 hex chars. Column order
/// does not affect it (names are sorted), so a reordering that preserves the
/// set is fingerprint-stable while an add/remove/rename is not.
pub fn schema_fingerprint(schema: &Schema) -> String {
    let mut names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    names.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(names.join("\n").as_bytes());
    let digest = hasher.finalize();
    let hex = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
    hex[..16].to_string()
}

/// The fingerprint of the surface schema for a given cumulative setting. Public
/// so Python (and the TS-sync test) can read the canonical value.
pub fn surface_schema_fingerprint(emit_cumulative: bool) -> String {
    schema_fingerprint(&surface_block_schema(emit_cumulative))
}

/// Build the `SurfaceBlock` schema. Column order is load-bearing —
/// consumers index columns by position.
///
/// When `emit_cumulative` is true, three additive cross-bucket cumulative
/// columns are appended (cols 26..28), each tagged with Arrow field metadata
/// `cumulative="true"` so consumers can identify them programmatically. When
/// false they are absent from the schema entirely (not zero-filled). Only
/// extensive (additive) metrics get a cumulative; instantaneous states
/// (cell_load, queue_depth, status) do not — a running sum of an instantaneous
/// value is meaningless.
pub fn surface_block_schema(emit_cumulative: bool) -> Schema {
    let mut fields = vec![
        // Bucket metadata
        Field::new("bucket_idx", DataType::UInt32, false),
        Field::new("time_atu", DataType::Int64, false),
        Field::new("bucket_atu", DataType::Int64, false),
        // Node-indexed lists (length = num_nodes)
        Field::new(
            "node_load",
            DataType::List(Arc::new(Field::new("item", DataType::UInt32, true))),
            false,
        ),
        Field::new(
            "node_backlog",
            DataType::List(Arc::new(Field::new("item", DataType::UInt32, true))),
            false,
        ),
        Field::new(
            "node_queue_depth",
            DataType::List(Arc::new(Field::new("item", DataType::UInt32, true))),
            false,
        ),
        Field::new(
            "node_arrivals_in_bucket",
            DataType::List(Arc::new(Field::new("item", DataType::UInt32, true))),
            false,
        ),
        // Lane-indexed lists (length = num_lanes)
        Field::new(
            "lane_departures",
            DataType::List(Arc::new(Field::new("item", DataType::UInt32, true))),
            false,
        ),
        Field::new(
            "lane_flow_units",
            DataType::List(Arc::new(Field::new("item", DataType::UInt64, true))),
            false,
        ),
        Field::new(
            "lane_in_use",
            DataType::List(Arc::new(Field::new("item", DataType::UInt8, true))),
            false,
        ),
        // KPI scalars (cumulative-within-bucket; cross-bucket cumulatives are
        // a UI concern computed at consumption time per the session decisions).
        Field::new("delivered_count", DataType::UInt64, false),
        Field::new("on_time_count", DataType::UInt64, false),
        Field::new("late_count", DataType::UInt64, false),
        Field::new("total_transit_atu", DataType::Int64, false),
        Field::new("total_cost_micros", DataType::Int64, false),
        Field::new("total_co2_grams", DataType::Int64, false),
        Field::new("waits_recorded", DataType::UInt64, false),
        Field::new("total_wait_atu", DataType::Int64, false),
        // Cell-indexed lists (length = num_cells), atlas-pinned res-3 rollups.
        Field::new(
            "cell_h3",
            DataType::List(Arc::new(Field::new("item", DataType::UInt64, true))),
            false,
        ),
        Field::new(
            "cell_centroid_lat",
            DataType::List(Arc::new(Field::new("item", DataType::Float64, true))),
            false,
        ),
        Field::new(
            "cell_centroid_lon",
            DataType::List(Arc::new(Field::new("item", DataType::Float64, true))),
            false,
        ),
        Field::new(
            "cell_load",
            DataType::List(Arc::new(Field::new("item", DataType::UInt32, true))),
            false,
        ),
        Field::new(
            "cell_arrivals_in_bucket",
            DataType::List(Arc::new(Field::new("item", DataType::UInt32, true))),
            false,
        ),
        Field::new(
            "cell_departures_in_bucket",
            DataType::List(Arc::new(Field::new("item", DataType::UInt32, true))),
            false,
        ),
        Field::new(
            "cell_queue_depth",
            DataType::List(Arc::new(Field::new("item", DataType::UInt32, true))),
            false,
        ),
        Field::new(
            "cell_status",
            DataType::List(Arc::new(Field::new("item", DataType::UInt8, true))),
            false,
        ),
    ];

    if emit_cumulative {
        // Cross-bucket cumulative columns for extensive metrics only. Each is a
        // u64 list (widened from the u32 per-bucket base to avoid overflow over
        // long runs). failed_by_reason_cumulative is reserved — omitted in v1.
        fields.push(cumulative_field(
            "cell_arrivals_cumulative",
            DataType::UInt64,
        ));
        fields.push(cumulative_field(
            "cell_departures_cumulative",
            DataType::UInt64,
        ));
        fields.push(cumulative_field(
            "lane_flow_units_cumulative",
            DataType::UInt64,
        ));
    }

    let base = Schema::new(fields);
    let fingerprint = schema_fingerprint(&base);
    let meta = std::collections::HashMap::from([
        ("schema_version".to_string(), SCHEMA_VERSION.to_string()),
        ("schema_fingerprint".to_string(), fingerprint),
    ]);
    base.with_metadata(meta)
}

/// A cumulative `List<item_ty>` column tagged with field metadata
/// `cumulative="true"` so downstream consumers can identify additive columns.
fn cumulative_field(name: &str, item_ty: DataType) -> Field {
    let meta = std::collections::HashMap::from([("cumulative".to_string(), "true".to_string())]);
    Field::new(
        name,
        DataType::List(Arc::new(Field::new("item", item_ty, true))),
        false,
    )
    .with_metadata(meta)
}

/// Encode a slice of [`SurfaceSnapshot`] into an Arrow IPC stream. Returns
/// the full byte buffer ready for WebSocket dispatch under tag `0x06`.
///
/// One record batch per snapshot; one row per batch. `bucket_idx_base` is
/// the index of the first snapshot in the slice (`0` for a fresh drain;
/// non-zero when concatenating drains across streaming-mode chunks).
///
/// # Errors
///
/// Returns [`ArrowError`] on schema construction or IPC serialization
/// failure. Empty `snapshots` is not an error — the stream contains only
/// the schema preamble.
/// Running per-cell / per-lane cumulative totals, carried ACROSS streaming
/// drains so the cumulative columns are correct over the whole run, not just
/// within one `drain_surfaces_arrow` chunk. Construct once per run
/// (`PyEngine` owns it); pass `None` for a self-contained batch encode.
#[derive(Debug, Clone)]
pub struct CumulativeState {
    cell_arrivals: Vec<u64>,
    cell_departures: Vec<u64>,
    lane_flow_units: Vec<u64>,
}

impl CumulativeState {
    pub fn new(num_cells: usize, num_lanes: usize) -> Self {
        CumulativeState {
            cell_arrivals: vec![0; num_cells],
            cell_departures: vec![0; num_cells],
            lane_flow_units: vec![0; num_lanes],
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn write_surface_block_ipc(
    snapshots: &[SurfaceSnapshot],
    bucket_atu: Atu,
    num_nodes: usize,
    num_lanes: usize,
    bucket_idx_base: u32,
    cells: &NodeCellLookup,
    lane_src_node: &[NodeId],
    emit_cumulative: bool,
    cum: Option<&mut CumulativeState>,
    run_manifest_json: Option<&str>,
) -> Result<Vec<u8>, ArrowError> {
    if cells.num_nodes() != num_nodes {
        return Err(ArrowError::InvalidArgumentError(format!(
            "SurfaceBlock: NodeCellLookup has {} nodes, expected num_nodes={num_nodes}",
            cells.num_nodes()
        )));
    }
    if lane_src_node.len() != num_lanes {
        return Err(ArrowError::InvalidArgumentError(format!(
            "SurfaceBlock: lane_src_node has length {}, expected num_lanes={num_lanes}",
            lane_src_node.len()
        )));
    }

    // Cumulative accumulator: use the caller's (streaming continuation) or a
    // fresh local one (batch encode). Only touched when emit_cumulative.
    let mut local_cum;
    let state: &mut CumulativeState = match cum {
        Some(s) => s,
        None => {
            local_cum = CumulativeState::new(cells.num_cells(), num_lanes);
            &mut local_cum
        }
    };

    let mut schema = surface_block_schema(emit_cumulative);
    if let Some(manifest) = run_manifest_json {
        // Embed the run manifest in the stream's schema metadata for browser-side
        // provenance validation, alongside schema_version / schema_fingerprint.
        let mut meta = schema.metadata().clone();
        meta.insert("run_manifest".to_string(), manifest.to_string());
        schema = schema.with_metadata(meta);
    }
    let schema = Arc::new(schema);
    let mut buf: Vec<u8> = Vec::with_capacity(estimate_buf_size(snapshots, num_nodes, num_lanes));
    {
        let mut writer = StreamWriter::try_new(&mut buf, &schema)?;
        for (offset, snap) in snapshots.iter().enumerate() {
            let batch = snapshot_to_record_batch(
                snap,
                bucket_atu,
                num_nodes,
                num_lanes,
                bucket_idx_base + offset as u32,
                cells,
                lane_src_node,
                emit_cumulative,
                state,
                Arc::clone(&schema),
            )?;
            writer.write(&batch)?;
        }
        writer.finish()?;
    }
    Ok(buf)
}

#[allow(clippy::too_many_arguments)]
fn snapshot_to_record_batch(
    snap: &SurfaceSnapshot,
    bucket_atu: Atu,
    num_nodes: usize,
    num_lanes: usize,
    bucket_idx: u32,
    cells: &NodeCellLookup,
    lane_src_node: &[NodeId],
    emit_cumulative: bool,
    cum: &mut CumulativeState,
    schema: Arc<Schema>,
) -> Result<RecordBatch, ArrowError> {
    // Node-indexed columns: length must equal num_nodes. We rely on the engine
    // contract that node_loads / node_backlogs / node_queue_depth are all
    // num_nodes-length vectors. Mismatches are an engine-invariant violation
    // and we surface them as ArrowError rather than silently padding.
    check_node_len(snap.node_loads.len(), num_nodes, "node_loads")?;
    check_node_len(snap.node_backlogs.len(), num_nodes, "node_backlogs")?;
    check_node_len(snap.node_queue_depth.len(), num_nodes, "node_queue_depth")?;
    check_lane_len(snap.lane_departures.len(), num_lanes, "lane_departures")?;
    check_lane_len(snap.lane_flow_units.len(), num_lanes, "lane_flow_units")?;

    let node_arrivals = node_arrivals_from_intermediate(&snap.intermediate_arrivals, num_nodes);
    let lane_in_use: Vec<u8> = snap
        .lane_departures
        .iter()
        .map(|d| if *d > 0 { 1u8 } else { 0u8 })
        .collect();

    let cell = rollup_cells(snap, cells, lane_src_node);

    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(UInt32Array::from(vec![bucket_idx])),
        Arc::new(Int64Array::from(vec![snap.time_atu])),
        Arc::new(Int64Array::from(vec![bucket_atu])),
        Arc::new(list_u32(&snap.node_loads)),
        Arc::new(list_u32(&snap.node_backlogs)),
        Arc::new(list_u32(&snap.node_queue_depth)),
        Arc::new(list_u32(&node_arrivals)),
        Arc::new(list_u32(&snap.lane_departures)),
        Arc::new(list_u64(&snap.lane_flow_units)),
        Arc::new(list_u8(&lane_in_use)),
        Arc::new(UInt64Array::from(vec![snap.delivered_count])),
        Arc::new(UInt64Array::from(vec![snap.on_time_count])),
        Arc::new(UInt64Array::from(vec![snap.late_count])),
        Arc::new(Int64Array::from(vec![snap.total_transit_atu])),
        Arc::new(Int64Array::from(vec![snap.total_cost_micros])),
        Arc::new(Int64Array::from(vec![snap.total_co2_grams])),
        Arc::new(UInt64Array::from(vec![snap.waits_recorded])),
        Arc::new(Int64Array::from(vec![snap.total_wait_atu])),
        Arc::new(list_u64(&cell.h3)),
        Arc::new(list_f64(&cell.lat)),
        Arc::new(list_f64(&cell.lon)),
        Arc::new(list_u32(&cell.load)),
        Arc::new(list_u32(&cell.arrivals)),
        Arc::new(list_u32(&cell.departures)),
        Arc::new(list_u32(&cell.queue)),
        Arc::new(list_u8(&cell.status)),
    ];

    if emit_cumulative {
        // Advance running totals by this bucket's additive values, then emit
        // the running totals (cumulative-through-this-bucket). Saturating to
        // guard against overflow on pathologically long runs.
        for t in 0..cell.arrivals.len() {
            cum.cell_arrivals[t] = cum.cell_arrivals[t].saturating_add(cell.arrivals[t] as u64);
            cum.cell_departures[t] =
                cum.cell_departures[t].saturating_add(cell.departures[t] as u64);
        }
        for (e, &flow) in snap.lane_flow_units.iter().enumerate() {
            cum.lane_flow_units[e] = cum.lane_flow_units[e].saturating_add(flow);
        }
        columns.push(Arc::new(list_u64(&cum.cell_arrivals)));
        columns.push(Arc::new(list_u64(&cum.cell_departures)));
        columns.push(Arc::new(list_u64(&cum.lane_flow_units)));
    }

    RecordBatch::try_new(schema, columns)
}

/// Per-bucket cell-indexed rollup arrays. Every field has length `num_cells`.
struct CellColumns {
    h3: Vec<u64>,
    lat: Vec<f64>,
    lon: Vec<f64>,
    load: Vec<u32>,
    arrivals: Vec<u32>,
    departures: Vec<u32>,
    queue: Vec<u32>,
    status: Vec<u8>,
}

/// Roll per-node / per-lane surface values up to view cells (res-3) using the
/// atlas-pinned [`NodeCellLookup`]. No H3 math — the mapping is data.
///
/// * `cell_load`, `cell_queue_depth` — SUM of child-node values.
/// * `cell_arrivals_in_bucket` — count of intermediate arrivals whose node maps
///   to the cell.
/// * `cell_departures_in_bucket` — SUM of `lane_departures` rolled to the
///   lane's source node's cell.
/// * `cell_status` — WORST-OF (max) child-node status. v1 has no per-node status
///   so this is all zeros; the reduction shape is ready for when it lands.
///
/// All SUM reductions are commutative, so child iteration order is irrelevant;
/// the dense cell order is fixed by the lookup (ascending `cell_id_r3`).
fn rollup_cells(
    snap: &SurfaceSnapshot,
    cells: &NodeCellLookup,
    lane_src_node: &[NodeId],
) -> CellColumns {
    let nt = cells.num_cells();
    let nh = cells.num_nodes();
    let mut load = vec![0u32; nt];
    let mut queue = vec![0u32; nt];
    let mut arrivals = vec![0u32; nt];
    let mut departures = vec![0u32; nt];
    let status = vec![0u8; nt];

    for node in 0..nh {
        let t = cells.node_to_cell[node] as usize;
        load[t] = load[t].saturating_add(snap.node_loads[node]);
        queue[t] = queue[t].saturating_add(snap.node_queue_depth[node]);
    }

    for (_sid, node, _atu) in &snap.intermediate_arrivals {
        let h = *node as usize;
        if h < nh {
            let t = cells.node_to_cell[h] as usize;
            arrivals[t] = arrivals[t].saturating_add(1);
        }
    }

    for (e, &count) in snap.lane_departures.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let src = lane_src_node[e] as usize;
        if src < nh {
            let t = cells.node_to_cell[src] as usize;
            departures[t] = departures[t].saturating_add(count);
        }
    }

    CellColumns {
        h3: cells.cell_h3.clone(),
        lat: cells.cell_centroid_lat.clone(),
        lon: cells.cell_centroid_lon.clone(),
        load,
        arrivals,
        departures,
        queue,
        status,
    }
}

/// Aggregate per-flow unit arrival records into per-node counts.
///
/// `intermediate_arrivals` records (flow_unit_id, node_id, atu) for
/// `Moving → Dwelling` transitions in this bucket. We count per node_id.
/// Out-of-bounds node_ids are dropped silently — they indicate engine bugs
/// that surface louder elsewhere and we do not want one bad arrival to
/// break the whole frame.
fn node_arrivals_from_intermediate(
    intermediate_arrivals: &[(u64, u32, Atu)],
    num_nodes: usize,
) -> Vec<u32> {
    let mut counts = vec![0u32; num_nodes];
    for (_flow_unit, node_id, _atu) in intermediate_arrivals {
        let idx = *node_id as usize;
        if idx < num_nodes {
            counts[idx] = counts[idx].saturating_add(1);
        }
    }
    counts
}

fn list_u32(values: &[u32]) -> arrow::array::ListArray {
    let mut builder = ListBuilder::new(UInt32Builder::with_capacity(values.len()));
    for v in values {
        builder.values().append_value(*v);
    }
    builder.append(true);
    builder.finish()
}

fn list_u64(values: &[u64]) -> arrow::array::ListArray {
    let mut builder = ListBuilder::new(UInt64Builder::with_capacity(values.len()));
    for v in values {
        builder.values().append_value(*v);
    }
    builder.append(true);
    builder.finish()
}

fn list_f64(values: &[f64]) -> arrow::array::ListArray {
    let mut builder = ListBuilder::new(Float64Builder::with_capacity(values.len()));
    for v in values {
        builder.values().append_value(*v);
    }
    builder.append(true);
    builder.finish()
}

fn list_u8(values: &[u8]) -> arrow::array::ListArray {
    let mut builder = ListBuilder::new(UInt8Builder::with_capacity(values.len()));
    for v in values {
        builder.values().append_value(*v);
    }
    builder.append(true);
    builder.finish()
}

fn check_node_len(actual: usize, expected: usize, field: &str) -> Result<(), ArrowError> {
    if actual != expected {
        Err(ArrowError::InvalidArgumentError(format!(
            "SurfaceBlock: {field} has length {actual}, expected num_nodes={expected}"
        )))
    } else {
        Ok(())
    }
}

fn check_lane_len(actual: usize, expected: usize, field: &str) -> Result<(), ArrowError> {
    if actual != expected {
        Err(ArrowError::InvalidArgumentError(format!(
            "SurfaceBlock: {field} has length {actual}, expected num_lanes={expected}"
        )))
    } else {
        Ok(())
    }
}

fn estimate_buf_size(snapshots: &[SurfaceSnapshot], num_nodes: usize, num_lanes: usize) -> usize {
    // 4-byte node uints + 8-byte lane uints + ~160-byte overhead per batch.
    let per_batch = num_nodes * 4 * 4 + num_lanes * (4 + 8 + 1) + 160;
    snapshots.len() * per_batch + 1024
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::ipc::reader::StreamReader;

    /// 4 nodes → 2 cells: nodes 0,1 → cell 0 (h3 100); nodes 2,3 → cell 1 (h3 222).
    fn tiles_4h_2t() -> NodeCellLookup {
        NodeCellLookup {
            node_to_cell: vec![0, 0, 1, 1],
            cell_h3: vec![100, 222],
            cell_centroid_lat: vec![10.0, 20.0],
            cell_centroid_lon: vec![-10.0, -20.0],
        }
    }

    /// Per-CSR-lane source node for the 6-lane fixture.
    fn lane_src_6() -> Vec<NodeId> {
        vec![0, 1, 2, 3, 0, 2]
    }

    fn list_u32_at(batch: &RecordBatch, col: usize) -> Vec<u32> {
        batch
            .column(col)
            .as_any()
            .downcast_ref::<arrow::array::ListArray>()
            .unwrap()
            .value(0)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap()
            .iter()
            .map(|v| v.unwrap())
            .collect()
    }

    fn list_u64_at(batch: &RecordBatch, col: usize) -> Vec<u64> {
        batch
            .column(col)
            .as_any()
            .downcast_ref::<arrow::array::ListArray>()
            .unwrap()
            .value(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .iter()
            .map(|v| v.unwrap())
            .collect()
    }

    fn make_snapshot(time_atu: Atu, num_nodes: usize, num_lanes: usize) -> SurfaceSnapshot {
        SurfaceSnapshot {
            time_atu,
            node_loads: (0..num_nodes as u32).collect(),
            node_backlogs: vec![1; num_nodes],
            node_queue_depth: vec![0; num_nodes],
            lane_departures: (0..num_lanes as u32).map(|i| i % 3).collect(),
            lane_flow_units: (0..num_lanes as u64).collect(),
            delivered_count: 10,
            on_time_count: 7,
            late_count: 3,
            total_transit_atu: 12_345,
            total_cost_micros: 1_000_000,
            total_co2_grams: 200,
            waits_recorded: 4,
            total_wait_atu: 500,
            intermediate_arrivals: vec![(0, 1, time_atu), (1, 1, time_atu), (2, 2, time_atu)],
        }
    }

    #[test]
    fn roundtrip_single_bucket() {
        let snap = make_snapshot(3_600_000, 4, 6);
        let bytes =
            write_surface_block_ipc(&[snap.clone()], 3_600_000, 4, 6, 0, &tiles_4h_2t(), &lane_src_6(), false, None, None)
                .expect("write should succeed");

        let cursor = std::io::Cursor::new(bytes);
        let mut reader = StreamReader::try_new(cursor, None).expect("schema should parse");
        let batch = reader.next().expect("one batch expected").expect("batch ok");

        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 26);

        let bucket_idx = batch
            .column(0)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        assert_eq!(bucket_idx.value(0), 0);
        let time_atu = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(time_atu.value(0), 3_600_000);

        let delivered = batch
            .column(10)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        assert_eq!(delivered.value(0), 10);

        // No second batch.
        assert!(reader.next().is_none());
    }

    #[test]
    fn roundtrip_multi_bucket_preserves_order() {
        let snaps: Vec<SurfaceSnapshot> = (1..=3)
            .map(|i| make_snapshot(i as Atu * 3_600_000, 4, 6))
            .collect();
        let bytes =
            write_surface_block_ipc(&snaps, 3_600_000, 4, 6, 0, &tiles_4h_2t(), &lane_src_6(), false, None, None)
                .unwrap();

        let cursor = std::io::Cursor::new(bytes);
        let reader = StreamReader::try_new(cursor, None).unwrap();
        let batches: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(batches.len(), 3);
        for (i, batch) in batches.iter().enumerate() {
            let idx_col = batch
                .column(0)
                .as_any()
                .downcast_ref::<UInt32Array>()
                .unwrap();
            assert_eq!(idx_col.value(0), i as u32, "bucket_idx must monotonically increase");
            let t_col = batch
                .column(1)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            assert_eq!(t_col.value(0), (i as i64 + 1) * 3_600_000);
        }
    }

    #[test]
    fn node_arrivals_count_per_node() {
        // intermediate_arrivals has 2 arrivals at node 1 and 1 at node 2.
        let snap = make_snapshot(3_600_000, 4, 6);
        let bytes =
            write_surface_block_ipc(&[snap], 3_600_000, 4, 6, 0, &tiles_4h_2t(), &lane_src_6(), false, None, None)
                .unwrap();
        let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        let arrivals_col = batch
            .column(6)
            .as_any()
            .downcast_ref::<arrow::array::ListArray>()
            .unwrap();
        let values = arrivals_col
            .value(0)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap()
            .iter()
            .map(|v| v.unwrap())
            .collect::<Vec<u32>>();
        assert_eq!(values, vec![0, 2, 1, 0]);
    }

    #[test]
    fn lane_in_use_is_one_when_departures_nonzero() {
        let snap = make_snapshot(3_600_000, 4, 6);
        // make_snapshot uses i % 3 for lane_departures, so lanes [0, 3] are
        // zero and [1, 2, 4, 5] are non-zero.
        let bytes =
            write_surface_block_ipc(&[snap], 3_600_000, 4, 6, 0, &tiles_4h_2t(), &lane_src_6(), false, None, None)
                .unwrap();
        let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        let in_use = batch
            .column(9)
            .as_any()
            .downcast_ref::<arrow::array::ListArray>()
            .unwrap();
        let values = in_use
            .value(0)
            .as_any()
            .downcast_ref::<arrow::array::UInt8Array>()
            .unwrap()
            .iter()
            .map(|v| v.unwrap())
            .collect::<Vec<u8>>();
        assert_eq!(values, vec![0, 1, 1, 0, 1, 1]);
    }

    #[test]
    fn rejects_mismatched_node_length() {
        let mut snap = make_snapshot(3_600_000, 4, 6);
        snap.node_loads.pop(); // length 3, expected 4
        let result =
            write_surface_block_ipc(&[snap], 3_600_000, 4, 6, 0, &tiles_4h_2t(), &lane_src_6(), false, None, None);
        assert!(matches!(result, Err(ArrowError::InvalidArgumentError(_))));
    }

    #[test]
    fn empty_snapshots_emits_schema_only_stream() {
        let bytes =
            write_surface_block_ipc(&[], 3_600_000, 4, 6, 0, &tiles_4h_2t(), &lane_src_6(), false, None, None).unwrap();
        let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
        // Schema parses, zero batches.
        assert_eq!(reader.count(), 0);
    }

    #[test]
    fn bucket_idx_base_offsets_indices() {
        let snaps: Vec<SurfaceSnapshot> = (0..2).map(|_| make_snapshot(3_600_000, 4, 6)).collect();
        let bytes =
            write_surface_block_ipc(&snaps, 3_600_000, 4, 6, 100, &tiles_4h_2t(), &lane_src_6(), false, None, None)
                .unwrap();
        let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
        let batches: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();
        let idx0 = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap()
            .value(0);
        let idx1 = batches[1]
            .column(0)
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap()
            .value(0);
        assert_eq!(idx0, 100);
        assert_eq!(idx1, 101);
    }

    #[test]
    fn cell_rollups_sum_and_passthrough() {
        // make_snapshot(_,4,6): node_loads=[0,1,2,3], queue=[0;4],
        // arrivals at hub1 x2 + hub2 x1, lane_departures=[0,1,2,0,1,2].
        // cells: nodes 0,1->t0; nodes 2,3->t1. lane_src=[0,1,2,3,0,2].
        let snap = make_snapshot(3_600_000, 4, 6);
        let bytes =
            write_surface_block_ipc(&[snap], 3_600_000, 4, 6, 0, &tiles_4h_2t(), &lane_src_6(), false, None, None)
                .unwrap();
        let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();

        // col 18 cell_h3 — passthrough from the lookup (dense ascending order).
        let h3: Vec<u64> = batch
            .column(18)
            .as_any()
            .downcast_ref::<arrow::array::ListArray>()
            .unwrap()
            .value(0)
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .iter()
            .map(|v| v.unwrap())
            .collect();
        assert_eq!(h3, vec![100, 222]);

        // col 21 cell_load = SUM child node_loads: t0=0+1, t1=2+3.
        assert_eq!(list_u32_at(&batch, 21), vec![1, 5]);
        // col 22 cell_arrivals: hub1->t0 (2), hub2->t1 (1).
        assert_eq!(list_u32_at(&batch, 22), vec![2, 1]);
        // col 23 cell_departures: e1(1)+e4(1)->t0; e2(2)+e5(2)->t1.
        assert_eq!(list_u32_at(&batch, 23), vec![2, 4]);
        // col 24 cell_queue_depth: all zero.
        assert_eq!(list_u32_at(&batch, 24), vec![0, 0]);

        // col 25 cell_status — v1 WORST-OF over absent per-node status = zeros.
        let status: Vec<u8> = batch
            .column(25)
            .as_any()
            .downcast_ref::<arrow::array::ListArray>()
            .unwrap()
            .value(0)
            .as_any()
            .downcast_ref::<arrow::array::UInt8Array>()
            .unwrap()
            .iter()
            .map(|v| v.unwrap())
            .collect();
        assert_eq!(status, vec![0, 0]);
    }

    #[test]
    fn rejects_lookup_node_count_mismatch() {
        // Lookup has 4 nodes but the writer is told num_nodes=3.
        let snap = make_snapshot(3_600_000, 3, 6);
        let result =
            write_surface_block_ipc(&[snap], 3_600_000, 3, 6, 0, &tiles_4h_2t(), &lane_src_6(), false, None, None);
        assert!(matches!(result, Err(ArrowError::InvalidArgumentError(_))));
    }

    #[test]
    fn cumulative_columns_absent_when_disabled() {
        let snap = make_snapshot(3_600_000, 4, 6);
        let bytes =
            write_surface_block_ipc(&[snap], 3_600_000, 4, 6, 0, &tiles_4h_2t(), &lane_src_6(), false, None, None)
                .unwrap();
        let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        assert_eq!(batch.num_columns(), 26);
        for name in [
            "cell_arrivals_cumulative",
            "cell_departures_cumulative",
            "lane_flow_units_cumulative",
        ] {
            assert!(batch.schema().column_with_name(name).is_none());
        }
    }

    #[test]
    fn cumulative_columns_present_and_tagged_when_enabled() {
        let snap = make_snapshot(3_600_000, 4, 6);
        let bytes =
            write_surface_block_ipc(&[snap], 3_600_000, 4, 6, 0, &tiles_4h_2t(), &lane_src_6(), true, None, None)
                .unwrap();
        let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        assert_eq!(batch.num_columns(), 29);
        // Each cumulative field carries metadata cumulative=true.
        let schema = batch.schema();
        for name in [
            "cell_arrivals_cumulative",
            "cell_departures_cumulative",
            "lane_flow_units_cumulative",
        ] {
            let (_, field) = schema.column_with_name(name).unwrap();
            assert_eq!(field.metadata().get("cumulative").map(String::as_str), Some("true"));
        }
    }

    #[test]
    fn cumulative_is_cumsum_across_buckets() {
        // Two identical buckets. arrivals/cell=[2,1], departures/cell=[2,4],
        // lane_flow_units=[0,1,2,3,4,5] each bucket.
        let snaps: Vec<SurfaceSnapshot> =
            (0..2).map(|i| make_snapshot(i as Atu * 3_600_000, 4, 6)).collect();
        let bytes =
            write_surface_block_ipc(&snaps, 3_600_000, 4, 6, 0, &tiles_4h_2t(), &lane_src_6(), true, None, None)
                .unwrap();
        let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
        let batches: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(batches.len(), 2);

        // Bucket 0 = first bucket's totals.
        assert_eq!(list_u64_at(&batches[0], 26), vec![2, 1]); // cell_arrivals_cumulative
        assert_eq!(list_u64_at(&batches[0], 27), vec![2, 4]); // cell_departures_cumulative
        assert_eq!(list_u64_at(&batches[0], 28), vec![0, 1, 2, 3, 4, 5]); // lane_flow_units_cumulative
        // Bucket 1 = running sum of both buckets.
        assert_eq!(list_u64_at(&batches[1], 26), vec![4, 2]);
        assert_eq!(list_u64_at(&batches[1], 27), vec![4, 8]);
        assert_eq!(list_u64_at(&batches[1], 28), vec![0, 2, 4, 6, 8, 10]);
    }

    #[test]
    fn cumulative_continues_across_streaming_drains() {
        // Carry one CumulativeState across two separate write calls (drains).
        let mut cum = CumulativeState::new(2, 6);
        let snap = make_snapshot(3_600_000, 4, 6);
        let b1 = write_surface_block_ipc(
            &[snap.clone()], 3_600_000, 4, 6, 0, &tiles_4h_2t(), &lane_src_6(), true, Some(&mut cum), None,
        )
        .unwrap();
        let b2 = write_surface_block_ipc(
            &[snap], 3_600_000, 4, 6, 1, &tiles_4h_2t(), &lane_src_6(), true, Some(&mut cum), None,
        )
        .unwrap();
        let r1 = StreamReader::try_new(std::io::Cursor::new(b1), None).unwrap();
        let r2 = StreamReader::try_new(std::io::Cursor::new(b2), None).unwrap();
        let batch1 = r1.into_iter().next().unwrap().unwrap();
        let batch2 = r2.into_iter().next().unwrap().unwrap();
        assert_eq!(list_u64_at(&batch1, 26), vec![2, 1]);
        // Second drain continues the running total, not reset.
        assert_eq!(list_u64_at(&batch2, 26), vec![4, 2]);
        assert_eq!(list_u64_at(&batch2, 28), vec![0, 2, 4, 6, 8, 10]);
    }

    #[test]
    fn fingerprint_is_deterministic_and_order_independent() {
        // Same column set in different field order → same fingerprint.
        let a = Schema::new(vec![
            Field::new("b", DataType::UInt32, false),
            Field::new("a", DataType::Int64, false),
        ]);
        let b = Schema::new(vec![
            Field::new("a", DataType::Int64, false),
            Field::new("b", DataType::UInt32, false),
        ]);
        assert_eq!(schema_fingerprint(&a), schema_fingerprint(&b));
        assert_eq!(schema_fingerprint(&a).len(), 16);

        // Adding a column changes the fingerprint.
        let c = Schema::new(vec![
            Field::new("a", DataType::Int64, false),
            Field::new("b", DataType::UInt32, false),
            Field::new("c", DataType::UInt8, false),
        ]);
        assert_ne!(schema_fingerprint(&a), schema_fingerprint(&c));
    }

    #[test]
    fn schema_carries_version_and_fingerprint_metadata() {
        let schema = surface_block_schema(true);
        assert_eq!(
            schema.metadata().get("schema_version").map(String::as_str),
            Some(SCHEMA_VERSION)
        );
        assert_eq!(
            schema.metadata().get("schema_fingerprint"),
            Some(&surface_schema_fingerprint(true))
        );
        // Cumulative on/off are distinct schemas → distinct fingerprints.
        assert_ne!(
            surface_schema_fingerprint(true),
            surface_schema_fingerprint(false)
        );
    }

    #[test]
    fn run_manifest_embeds_in_stream_schema() {
        let snap = make_snapshot(3_600_000, 4, 6);
        let manifest = r#"{"run_id":"r1"}"#;
        let bytes = write_surface_block_ipc(
            &[snap], 3_600_000, 4, 6, 0, &tiles_4h_2t(), &lane_src_6(), false, None, Some(manifest),
        )
        .unwrap();
        let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
        let schema = reader.schema();
        assert_eq!(schema.metadata().get("run_manifest").map(String::as_str), Some(manifest));
    }

    #[test]
    fn rejects_lane_src_length_mismatch() {
        let snap = make_snapshot(3_600_000, 4, 6);
        let short_src = vec![0u32, 1, 2]; // len 3, expected 6
        let result = write_surface_block_ipc(
            &[snap],
            3_600_000,
            4,
            6,
            0,
            &tiles_4h_2t(),
            &short_src,
            false,
            None,
            None,
        );
        assert!(matches!(result, Err(ArrowError::InvalidArgumentError(_))));
    }
}
