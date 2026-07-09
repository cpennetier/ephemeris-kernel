//! Arrow snapshot of the transition log.

#![cfg(feature = "arrow")]

use crate::machine::StateMachine;
use crate::state::EntityState;
use crate::transition::Transition;
use arrow::array::{
    ArrayRef, RecordBatch, StringArray, UInt32Array, UInt64Array, UInt8Array,
};
use arrow::datatypes::{DataType, Field, Schema};
use std::sync::Arc;

pub struct LogSnapshot {
    pub version: u64,
    pub batch: RecordBatch,
}

pub fn snapshot_schema() -> Schema {
    Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("from_discriminant", DataType::UInt8, false),
        Field::new("from_detail", DataType::Utf8, true),
        Field::new("to_discriminant", DataType::UInt8, false),
        Field::new("to_detail", DataType::Utf8, true),
        Field::new("reason", DataType::Utf8, false),
        Field::new("at_bucket", DataType::UInt32, false),
        Field::new("at_version", DataType::UInt64, false),
    ])
}

fn detail_of(state: EntityState) -> Option<String> {
    match state {
        EntityState::Delivered(q) => Some(q.as_str().to_string()),
        EntityState::Failed(f) => Some(f.as_str().to_string()),
        _ => None,
    }
}

fn discriminant_u8(state: EntityState) -> u8 {
    state.discriminant() as u8
}

impl StateMachine {
    /// Snapshot the transition log as an Arrow RecordBatch.
    /// Rows are ordered by version (== insertion order).
    pub fn snapshot_log(&self) -> LogSnapshot {
        let version = self.version();
        let all: Vec<Transition> = self.all_transitions();
        let n = all.len();

        let mut entity_ids = Vec::with_capacity(n);
        let mut from_disc = Vec::with_capacity(n);
        let mut from_detail = Vec::with_capacity(n);
        let mut to_disc = Vec::with_capacity(n);
        let mut to_detail = Vec::with_capacity(n);
        let mut reasons = Vec::with_capacity(n);
        let mut buckets = Vec::with_capacity(n);
        let mut versions = Vec::with_capacity(n);

        for t in &all {
            entity_ids.push(t.entity_id);
            from_disc.push(discriminant_u8(t.from));
            from_detail.push(detail_of(t.from));
            to_disc.push(discriminant_u8(t.to));
            to_detail.push(detail_of(t.to));
            reasons.push(t.reason.as_string());
            buckets.push(t.at_bucket);
            versions.push(t.at_version);
        }

        let schema = Arc::new(snapshot_schema());
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from(entity_ids)),
            Arc::new(UInt8Array::from(from_disc)),
            Arc::new(StringArray::from(from_detail)),
            Arc::new(UInt8Array::from(to_disc)),
            Arc::new(StringArray::from(to_detail)),
            Arc::new(StringArray::from(reasons)),
            Arc::new(UInt32Array::from(buckets)),
            Arc::new(UInt64Array::from(versions)),
        ];
        let batch = RecordBatch::try_new(schema, arrays).expect(
            "snapshot RecordBatch construction: all columns same length",
        );
        LogSnapshot { version, batch }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::LedgerOpHints;
    use crate::state::{DeliveryQuality, FailureMode};

    #[test]
    fn empty_snapshot() {
        let m = StateMachine::new();
        let s = m.snapshot_log();
        assert_eq!(s.batch.num_rows(), 0);
        assert_eq!(s.version, 0);
    }

    #[test]
    fn snapshot_after_transitions_has_rows() {
        let m = StateMachine::new();
        let hints = LedgerOpHints {
            planned_route: vec![crate::envelope::RouteSegment {
                kind: crate::envelope::LedgerResourceKind::Lane,
                resource_id: 1,
                bucket: 0,
                units: 1,
            }],
            ..Default::default()
        };
        m.register(42).unwrap();
        m.propose_and_commit(42, crate::transition::TransitionReason::PlanFound, 0, &hints)
            .unwrap();
        m.propose_and_commit(
            42,
            crate::transition::TransitionReason::EnterHub(1),
            1,
            &Default::default(),
        )
        .unwrap();

        let s = m.snapshot_log();
        assert_eq!(s.batch.num_rows(), 2);
        assert_eq!(s.batch.schema().fields().len(), 8);
    }

    #[test]
    fn snapshot_includes_terminal_detail() {
        let m = StateMachine::new();
        m.register(7).unwrap();
        // Walk through to a Delivered(OnTimeClean) terminal.
        m.propose_and_commit(7, crate::transition::TransitionReason::PlanFound, 0, &LedgerOpHints {
            planned_route: vec![crate::envelope::RouteSegment {
                kind: crate::envelope::LedgerResourceKind::Lane,
                resource_id: 1,
                bucket: 0,
                units: 1,
            }],
            ..Default::default()
        })
        .unwrap();
        m.propose_and_commit(7, crate::transition::TransitionReason::EnterHub(1), 1, &Default::default()).unwrap();
        m.propose_and_commit(7, crate::transition::TransitionReason::DepartHub(1), 2, &Default::default()).unwrap();
        m.propose_and_commit(7, crate::transition::TransitionReason::ArriveDestination { quality: DeliveryQuality::OnTimeClean }, 3, &Default::default()).unwrap();

        let s = m.snapshot_log();
        assert_eq!(s.batch.num_rows(), 4);
        let to_detail = s.batch.column_by_name("to_detail").unwrap().as_any().downcast_ref::<StringArray>().unwrap();
        // Last row's to_detail = "OnTimeClean"
        assert_eq!(to_detail.value(3), "OnTimeClean");
    }

    #[test]
    fn snapshot_failure_detail() {
        let m = StateMachine::new();
        m.register(9).unwrap();
        m.propose_and_commit(
            9,
            crate::transition::TransitionReason::Unreachable,
            0,
            &Default::default(),
        )
        .unwrap();
        let s = m.snapshot_log();
        let to_detail = s.batch.column_by_name("to_detail").unwrap().as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(to_detail.value(0), "UnreachableTopology");

        let _ = FailureMode::Stranded; // touch FailureMode to silence unused import warning
    }
}
