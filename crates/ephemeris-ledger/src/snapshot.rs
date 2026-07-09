//! Arrow-backed snapshot of the ledger state.
//!
//! `Ledger::snapshot()` returns a frozen
//! view at the current version. The Arrow schema is fixed for v1:
//! reservation_id, resource_kind, resource_id, bucket, units, level,
//! owner, created_at_version. Rows ordered by reservation_id for
//! determinism.

#![cfg(feature = "arrow")]

use crate::ledger::Ledger;
use crate::reservation::LedgerVersion;
use arrow::array::{ArrayRef, RecordBatch, UInt32Array, UInt64Array, UInt8Array};
use arrow::datatypes::{DataType, Field, Schema};
use std::sync::Arc;

pub struct LedgerSnapshot {
    pub version: LedgerVersion,
    pub batch: RecordBatch,
}

/// Schema for ledger snapshots. Stable as part of the public API.
pub fn snapshot_schema() -> Schema {
    Schema::new(vec![
        Field::new("reservation_id", DataType::UInt64, false),
        Field::new("resource_kind", DataType::UInt8, false),
        Field::new("resource_id", DataType::UInt64, false),
        Field::new("bucket", DataType::UInt32, false),
        Field::new("units", DataType::UInt64, false),
        Field::new("level", DataType::UInt8, false),
        Field::new("owner", DataType::UInt64, false),
        Field::new("created_at_version", DataType::UInt64, false),
    ])
}

impl Ledger {
    /// Snapshot the full ledger state at the current version.
    /// Rows are ordered by `reservation_id` for byte-determinism.
    pub fn snapshot(&self) -> LedgerSnapshot {
        let version = self.version();
        let all = self.all_reservations();
        let n = all.len();

        let mut reservation_ids = Vec::with_capacity(n);
        let mut kinds = Vec::with_capacity(n);
        let mut resource_ids = Vec::with_capacity(n);
        let mut buckets = Vec::with_capacity(n);
        let mut units_vec = Vec::with_capacity(n);
        let mut levels = Vec::with_capacity(n);
        let mut owners = Vec::with_capacity(n);
        let mut created_at = Vec::with_capacity(n);

        for r in &all {
            reservation_ids.push(r.id);
            kinds.push(r.resource_kind.as_u8());
            resource_ids.push(r.resource_id);
            buckets.push(r.bucket);
            units_vec.push(r.units);
            levels.push(r.level.as_u8());
            owners.push(r.owner);
            created_at.push(r.created_at_version);
        }

        let schema = Arc::new(snapshot_schema());
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from(reservation_ids)),
            Arc::new(UInt8Array::from(kinds)),
            Arc::new(UInt64Array::from(resource_ids)),
            Arc::new(UInt32Array::from(buckets)),
            Arc::new(UInt64Array::from(units_vec)),
            Arc::new(UInt8Array::from(levels)),
            Arc::new(UInt64Array::from(owners)),
            Arc::new(UInt64Array::from(created_at)),
        ];
        let batch = RecordBatch::try_new(schema, arrays).expect(
            "snapshot RecordBatch construction: all columns same length",
        );
        LedgerSnapshot { version, batch }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reservation::CommitmentLevel;
    use crate::resource::ResourceKind;

    #[test]
    fn empty_snapshot_has_zero_rows() {
        let l = Ledger::new();
        let s = l.snapshot();
        assert_eq!(s.batch.num_rows(), 0);
        assert_eq!(s.batch.schema().fields().len(), 8);
        assert_eq!(s.version, 0);
    }

    #[test]
    fn populated_snapshot_has_correct_rows() {
        let l = Ledger::new();
        l.register_lane(1, 100, 24);
        l.register_node(2, 50, 24);
        l.reserve(ResourceKind::Lane, 1, 5, 10, CommitmentLevel::Hard, 99)
            .unwrap();
        l.reserve(ResourceKind::Node, 2, 5, 7, CommitmentLevel::Soft, 99)
            .unwrap();
        l.reserve(ResourceKind::Lane, 1, 6, 3, CommitmentLevel::Forecast, 100)
            .unwrap();

        let s = l.snapshot();
        assert_eq!(s.batch.num_rows(), 3);

        let ids = s
            .batch
            .column_by_name("reservation_id")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        // Rows ordered by reservation_id (0, 1, 2).
        assert_eq!(ids.value(0), 0);
        assert_eq!(ids.value(1), 1);
        assert_eq!(ids.value(2), 2);

        let units = s
            .batch
            .column_by_name("units")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap();
        assert_eq!(units.value(0), 10);
        assert_eq!(units.value(1), 7);
        assert_eq!(units.value(2), 3);
    }

    #[test]
    fn snapshot_is_deterministic() {
        // Two ledgers with identical operations → byte-identical snapshots.
        let make = || {
            let l = Ledger::new();
            l.register_lane(1, 100, 24);
            l.register_lane(2, 50, 24);
            l.reserve(ResourceKind::Lane, 1, 0, 10, CommitmentLevel::Hard, 1)
                .unwrap();
            l.reserve(ResourceKind::Lane, 2, 0, 5, CommitmentLevel::Soft, 2)
                .unwrap();
            l.reserve(ResourceKind::Lane, 1, 1, 7, CommitmentLevel::Forecast, 3)
                .unwrap();
            l
        };
        let s1 = make().snapshot();
        let s2 = make().snapshot();
        assert_eq!(s1.version, s2.version);
        assert_eq!(s1.batch.num_rows(), s2.batch.num_rows());
        // Compare each column row by row.
        for col in [
            "reservation_id",
            "resource_kind",
            "resource_id",
            "bucket",
            "units",
            "level",
            "owner",
            "created_at_version",
        ] {
            let a = s1.batch.column_by_name(col).unwrap();
            let b = s2.batch.column_by_name(col).unwrap();
            assert_eq!(a.to_data(), b.to_data(), "column {} differs", col);
        }
    }

    #[test]
    fn snapshot_version_matches_ledger_version() {
        let l = Ledger::new();
        let s0 = l.snapshot();
        assert_eq!(s0.version, l.version());
        l.register_lane(1, 100, 24);
        let s1 = l.snapshot();
        assert!(s1.version > s0.version);
        l.reserve(ResourceKind::Lane, 1, 0, 10, CommitmentLevel::Hard, 99)
            .unwrap();
        let s2 = l.snapshot();
        assert!(s2.version > s1.version);
    }
}
