//! Property tests for the capacity ledger.
//!
//! Generates random reserve / release / promote sequences and verifies
//! that the ledger's invariants hold at every step.

use ephemeris_ledger_py::{CommitmentLevel, Ledger, ResourceKind};
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Op {
    Reserve {
        bucket: u32,
        units: u32,
        level: u8, // 0=Forecast, 1=Soft, 2=Hard
    },
    Release {
        target: u8, // index into the running ID list, modulo size
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0u32..4, 1u32..20, 0u8..3).prop_map(|(bucket, units, level)| Op::Reserve {
            bucket,
            units,
            level,
        }),
        (0u8..255).prop_map(|target| Op::Release { target }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 200,
        .. ProptestConfig::default()
    })]

    /// For any sequence of reserve / release ops, the lattice property
    /// holds at every step.
    #[test]
    fn lattice_property_holds_under_arbitrary_ops(ops in prop::collection::vec(op_strategy(), 0..200)) {
        let l = Ledger::new();
        // Single lane with finite capacity; ops live in [0, 4) buckets.
        l.register_lane(1, 100, 4);

        let mut live_ids: Vec<u64> = Vec::new();
        for op in &ops {
            match op {
                Op::Reserve { bucket, units, level } => {
                    let lvl = match level {
                        0 => CommitmentLevel::Forecast,
                        1 => CommitmentLevel::Soft,
                        _ => CommitmentLevel::Hard,
                    };
                    if let Ok(id) = l.reserve(
                        ResourceKind::Lane, 1, *bucket, *units as u64, lvl, 0,
                    ) {
                        live_ids.push(id);
                    }
                }
                Op::Release { target } => {
                    if !live_ids.is_empty() {
                        let idx = (*target as usize) % live_ids.len();
                        let id = live_ids.swap_remove(idx);
                        l.release(id).unwrap();
                    }
                }
            }
            // Check the lattice property on every (kind, resource, bucket).
            for bucket in 0..4 {
                let fh = l.free_on(ResourceKind::Lane, 1, bucket, CommitmentLevel::Hard);
                let fs = l.free_on(ResourceKind::Lane, 1, bucket, CommitmentLevel::Soft);
                let ff = l.free_on(ResourceKind::Lane, 1, bucket, CommitmentLevel::Forecast);
                prop_assert!(fh >= fs, "free(Hard) >= free(Soft) violated: {} < {}", fh, fs);
                prop_assert!(fs >= ff, "free(Soft) >= free(Forecast) violated: {} < {}", fs, ff);
                // free bounded by total capacity.
                let total = l.total_capacity(ResourceKind::Lane, 1, bucket);
                prop_assert!(fh <= total, "free(Hard) {} > total {}", fh, total);
            }
        }
    }

    /// After all releases, free returns to capacity.
    #[test]
    fn release_all_returns_to_full_capacity(
        reserves in prop::collection::vec(
            (0u32..4, 1u32..20, 0u8..2),  // Hard/Soft only (Forecast can over-book)
            0..50
        ),
    ) {
        let l = Ledger::new();
        l.register_lane(1, 100, 4);
        let mut ids = Vec::new();
        for (bucket, units, level) in reserves {
            let lvl = match level {
                0 => CommitmentLevel::Soft,
                _ => CommitmentLevel::Hard,
            };
            if let Ok(id) = l.reserve(ResourceKind::Lane, 1, bucket, units as u64, lvl, 0) {
                ids.push(id);
            }
        }
        for id in &ids {
            l.release(*id).unwrap();
        }
        for bucket in 0..4 {
            prop_assert_eq!(
                l.free_on(ResourceKind::Lane, 1, bucket, CommitmentLevel::Soft),
                100
            );
        }
    }
}
