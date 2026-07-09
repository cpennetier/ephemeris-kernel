//! Integration tests for the capacity ledger. Exercises end-to-end
//! invariants and the spec's Invariants section.

use ephemeris_ledger_py::{CommitmentLevel, Ledger, LedgerError, ResourceKind};

// ─── Basic invariants ──────────────────────────────────────────────────

#[test]
fn empty_ledger_returns_zero_capacity() {
    let l = Ledger::new();
    assert_eq!(l.total_capacity(ResourceKind::Lane, 1, 0), 0);
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard),
        0
    );
}

#[test]
fn register_then_query_returns_capacity() {
    let l = Ledger::new();
    l.register_lane(1, 100, 24);
    assert_eq!(l.total_capacity(ResourceKind::Lane, 1, 0), 100);
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard),
        100
    );
}

#[test]
fn single_reservation_reduces_free_at_hard() {
    let l = Ledger::new();
    l.register_lane(1, 100, 24);
    let id = l
        .reserve(ResourceKind::Lane, 1, 0, 10, CommitmentLevel::Hard, 99)
        .unwrap();
    assert_eq!(id, 0);
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard),
        90
    );
}

#[test]
fn reservation_ids_monotonic() {
    let l = Ledger::new();
    l.register_lane(1, 100, 24);
    let a = l
        .reserve(ResourceKind::Lane, 1, 0, 1, CommitmentLevel::Hard, 1)
        .unwrap();
    let b = l
        .reserve(ResourceKind::Lane, 1, 0, 1, CommitmentLevel::Hard, 1)
        .unwrap();
    let c = l
        .reserve(ResourceKind::Lane, 1, 1, 1, CommitmentLevel::Hard, 1)
        .unwrap();
    assert!(a < b);
    assert!(b < c);
}

#[test]
fn version_increments_on_writes() {
    let l = Ledger::new();
    let v0 = l.version();
    l.register_lane(1, 100, 24);
    let v1 = l.version();
    assert!(v1 > v0);
    l.reserve(ResourceKind::Lane, 1, 0, 5, CommitmentLevel::Hard, 1)
        .unwrap();
    let v2 = l.version();
    assert!(v2 > v1);
}

// ─── Commitment-level semantics ────────────────────────────────────────

#[test]
fn lattice_property_holds() {
    // free(Hard) >= free(Soft) >= free(Forecast) always.
    let l = Ledger::new();
    l.register_lane(1, 100, 24);
    l.reserve(ResourceKind::Lane, 1, 0, 10, CommitmentLevel::Hard, 1)
        .unwrap();
    l.reserve(ResourceKind::Lane, 1, 0, 20, CommitmentLevel::Soft, 2)
        .unwrap();
    l.reserve(ResourceKind::Lane, 1, 0, 30, CommitmentLevel::Forecast, 3)
        .unwrap();
    let f_hard = l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard);
    let f_soft = l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Soft);
    let f_fcst = l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Forecast);
    assert_eq!(f_hard, 90); // 100 - 10 (hard only)
    assert_eq!(f_soft, 70); // 100 - 10 - 20 (hard + soft)
    assert_eq!(f_fcst, 40); // 100 - 10 - 20 - 30 (all)
    assert!(f_hard >= f_soft);
    assert!(f_soft >= f_fcst);
}

#[test]
fn forecast_reservation_does_not_reduce_hard_or_soft_free() {
    let l = Ledger::new();
    l.register_lane(1, 100, 24);
    l.reserve(ResourceKind::Lane, 1, 0, 50, CommitmentLevel::Forecast, 1)
        .unwrap();
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard),
        100
    );
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Soft),
        100
    );
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Forecast),
        50
    );
}

#[test]
fn forecast_is_always_admissible_over_capacity() {
    // Forecast reservations can exceed capacity — they're predictions.
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    l.reserve(ResourceKind::Lane, 1, 0, 100, CommitmentLevel::Forecast, 1)
        .unwrap();
    // Free at forecast view is saturated at 0 (saturating_sub).
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Forecast),
        0
    );
    // Hard/Soft views still show the full 10.
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard),
        10
    );
}

#[test]
fn hard_reservation_fails_at_capacity_boundary() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    l.reserve(ResourceKind::Lane, 1, 0, 7, CommitmentLevel::Hard, 1)
        .unwrap();
    // 7 booked; 3 free at Soft view; try to book 5 → fail.
    let err = l
        .reserve(ResourceKind::Lane, 1, 0, 5, CommitmentLevel::Hard, 2)
        .unwrap_err();
    matches!(err, LedgerError::InsufficientCapacity { .. });
    // Booking exactly 3 succeeds.
    l.reserve(ResourceKind::Lane, 1, 0, 3, CommitmentLevel::Hard, 2)
        .unwrap();
}

#[test]
fn soft_reservation_books_against_hard_pool() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    l.reserve(ResourceKind::Lane, 1, 0, 6, CommitmentLevel::Hard, 1)
        .unwrap();
    // 4 free at Soft. Book 5 soft → fail.
    let err = l
        .reserve(ResourceKind::Lane, 1, 0, 5, CommitmentLevel::Soft, 2)
        .unwrap_err();
    matches!(err, LedgerError::InsufficientCapacity { .. });
    // Book 4 → OK.
    l.reserve(ResourceKind::Lane, 1, 0, 4, CommitmentLevel::Soft, 2)
        .unwrap();
}

// ─── Promotion ──────────────────────────────────────────────────────────

#[test]
fn promote_forecast_to_soft_consumes_capacity() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let id = l
        .reserve(ResourceKind::Lane, 1, 0, 5, CommitmentLevel::Forecast, 1)
        .unwrap();
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Soft),
        10
    );
    l.promote(id, CommitmentLevel::Soft).unwrap();
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Soft),
        5
    );
}

#[test]
fn promote_soft_to_hard_does_not_change_free_at_soft() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let id = l
        .reserve(ResourceKind::Lane, 1, 0, 5, CommitmentLevel::Soft, 1)
        .unwrap();
    let f_soft_before = l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Soft);
    l.promote(id, CommitmentLevel::Hard).unwrap();
    let f_soft_after = l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Soft);
    assert_eq!(f_soft_before, f_soft_after);
    // But free at Hard reduces.
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard),
        5
    );
}

#[test]
fn promote_rejects_downgrade() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let id = l
        .reserve(ResourceKind::Lane, 1, 0, 5, CommitmentLevel::Hard, 1)
        .unwrap();
    let err = l.promote(id, CommitmentLevel::Soft).unwrap_err();
    matches!(err, LedgerError::InvalidPromotion { .. });
}

#[test]
fn promote_rejects_equal_level() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let id = l
        .reserve(ResourceKind::Lane, 1, 0, 5, CommitmentLevel::Soft, 1)
        .unwrap();
    let err = l.promote(id, CommitmentLevel::Soft).unwrap_err();
    matches!(err, LedgerError::InvalidPromotion { .. });
}

#[test]
fn promote_forecast_fails_if_no_capacity() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    // Hard at 8/10
    l.reserve(ResourceKind::Lane, 1, 0, 8, CommitmentLevel::Hard, 1)
        .unwrap();
    // Forecast for 5 → fine, doesn't consume capacity.
    let fid = l
        .reserve(ResourceKind::Lane, 1, 0, 5, CommitmentLevel::Forecast, 2)
        .unwrap();
    // Promote forecast to soft → would need 5 free at Soft, only 2 are free.
    let err = l.promote(fid, CommitmentLevel::Soft).unwrap_err();
    matches!(err, LedgerError::InsufficientCapacity { .. });
}

// ─── Downgrade (B-D1) ─────────────────────────────────────────────────

#[test]
fn downgrade_hard_to_soft_frees_capacity_at_hard_view() {
    // After downgrade, the units no longer count at the Hard view, so
    // free_on(..., Hard) increases. The Soft view is unchanged (units
    // still count there).
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let id = l
        .reserve(ResourceKind::Lane, 1, 0, 7, CommitmentLevel::Hard, 1)
        .unwrap();
    assert_eq!(l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard), 3);
    assert_eq!(l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Soft), 3);
    l.downgrade(id, CommitmentLevel::Soft).unwrap();
    // Hard view: the 7 Hard units are gone → 10 free.
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard),
        10
    );
    // Soft view: the 7 units still count → 3 free.
    assert_eq!(l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Soft), 3);
}

#[test]
fn downgrade_soft_to_forecast_frees_capacity_at_both_strict_views() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let id = l
        .reserve(ResourceKind::Lane, 1, 0, 4, CommitmentLevel::Soft, 1)
        .unwrap();
    assert_eq!(l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Soft), 6);
    l.downgrade(id, CommitmentLevel::Forecast).unwrap();
    // Forecast doesn't count at Soft, so Soft view is fully free.
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Soft),
        10
    );
}

#[test]
fn downgrade_rejects_upgrade() {
    // Soft → Hard is an upgrade; downgrade must reject (use promote).
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let id = l
        .reserve(ResourceKind::Lane, 1, 0, 5, CommitmentLevel::Soft, 1)
        .unwrap();
    let err = l.downgrade(id, CommitmentLevel::Hard).unwrap_err();
    assert!(
        matches!(err, LedgerError::InvalidDowngrade { .. }),
        "expected InvalidDowngrade, got {:?}",
        err
    );
}

#[test]
fn downgrade_rejects_equal_level() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let id = l
        .reserve(ResourceKind::Lane, 1, 0, 5, CommitmentLevel::Soft, 1)
        .unwrap();
    let err = l.downgrade(id, CommitmentLevel::Soft).unwrap_err();
    assert!(
        matches!(err, LedgerError::InvalidDowngrade { .. }),
        "expected InvalidDowngrade, got {:?}",
        err
    );
}

#[test]
fn downgrade_unknown_reservation_returns_not_found() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let err = l
        .downgrade(99_999, CommitmentLevel::Forecast)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::ReservationNotFound(99_999)),
        "expected ReservationNotFound, got {:?}",
        err
    );
}

#[test]
fn downgrade_increments_version() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let id = l
        .reserve(ResourceKind::Lane, 1, 0, 4, CommitmentLevel::Hard, 1)
        .unwrap();
    let v_before = l.version();
    l.downgrade(id, CommitmentLevel::Soft).unwrap();
    assert_eq!(l.version(), v_before + 1);
}

// ─── Release ───────────────────────────────────────────────────────────

#[test]
fn release_restores_free() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let id = l
        .reserve(ResourceKind::Lane, 1, 0, 5, CommitmentLevel::Hard, 1)
        .unwrap();
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard),
        5
    );
    l.release(id).unwrap();
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard),
        10
    );
}

#[test]
fn release_is_idempotent() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let id = l
        .reserve(ResourceKind::Lane, 1, 0, 5, CommitmentLevel::Hard, 1)
        .unwrap();
    l.release(id).unwrap();
    l.release(id).unwrap(); // second call: no-op, no error
}

#[test]
fn released_reservation_not_in_reservations_for() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let id = l
        .reserve(ResourceKind::Lane, 1, 0, 5, CommitmentLevel::Hard, 1)
        .unwrap();
    assert_eq!(
        l.reservations_for(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard)
            .len(),
        1
    );
    l.release(id).unwrap();
    assert_eq!(
        l.reservations_for(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard)
            .len(),
        0
    );
}

// ─── Zero-unit and not-registered errors ──────────────────────────────

#[test]
fn zero_units_is_rejected() {
    let l = Ledger::new();
    l.register_lane(1, 10, 24);
    let err = l
        .reserve(ResourceKind::Lane, 1, 0, 0, CommitmentLevel::Hard, 1)
        .unwrap_err();
    matches!(err, LedgerError::ZeroUnits);
}

#[test]
fn unregistered_resource_is_rejected() {
    let l = Ledger::new();
    let err = l
        .reserve(ResourceKind::Lane, 999, 0, 1, CommitmentLevel::Hard, 1)
        .unwrap_err();
    matches!(err, LedgerError::ResourceNotRegistered { .. });
}

// ─── Node resources ─────────────────────────────────────────────────────

#[test]
fn node_resources_work_alongside_lanes() {
    let l = Ledger::new();
    l.register_lane(1, 100, 24);
    l.register_node(1, 50, 24);
    // Same numeric id, different kinds — must be tracked separately.
    l.reserve(ResourceKind::Lane, 1, 0, 30, CommitmentLevel::Hard, 1)
        .unwrap();
    l.reserve(ResourceKind::Node, 1, 0, 20, CommitmentLevel::Hard, 1)
        .unwrap();
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard),
        70
    );
    assert_eq!(
        l.free_on(ResourceKind::Node, 1, 0, CommitmentLevel::Hard),
        30
    );
}

// ─── Reservations-for iteration order is stable ───────────────────────

#[test]
fn reservations_for_returns_in_id_order() {
    let l = Ledger::new();
    l.register_lane(1, 100, 24);
    let mut ids = Vec::new();
    for owner in 0..5 {
        ids.push(
            l.reserve(ResourceKind::Lane, 1, 0, 1, CommitmentLevel::Hard, owner)
                .unwrap(),
        );
    }
    let res = l.reservations_for(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard);
    let res_ids: Vec<u64> = res.iter().map(|r| r.id).collect();
    assert_eq!(res_ids, ids);
}

// ─── Determinism ───────────────────────────────────────────────────────

#[test]
fn two_identical_sequences_produce_identical_state() {
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
    let a = make();
    let b = make();
    assert_eq!(a.version(), b.version());
    assert_eq!(a.n_reservations(), b.n_reservations());
    assert_eq!(
        a.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard),
        b.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard)
    );
    assert_eq!(a.all_reservations(), b.all_reservations());
}

// ─── Concurrent correctness ───────────────────────────────────────────

#[test]
fn concurrent_reservations_no_double_booking() {
    use rayon::prelude::*;
    use std::sync::Arc;
    let l = Arc::new(Ledger::new());
    l.register_lane(1, 1000, 24);

    let n_threads = 10;
    let per_thread = 50;
    let ids: Vec<Vec<Result<u64, ephemeris_ledger_py::LedgerError>>> = (0..n_threads)
        .into_par_iter()
        .map(|t| {
            (0..per_thread)
                .map(|i| {
                    l.reserve(
                        ResourceKind::Lane,
                        1,
                        0,
                        1,
                        CommitmentLevel::Hard,
                        (t * per_thread + i) as u64,
                    )
                })
                .collect()
        })
        .collect();
    let successes: usize = ids
        .iter()
        .map(|v| v.iter().filter(|r| r.is_ok()).count())
        .sum();
    // We made 500 total attempts and capacity is 1000 — all should succeed.
    assert_eq!(successes, n_threads * per_thread);
    assert_eq!(
        l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard),
        1000 - (n_threads * per_thread) as u64
    );
    // Reservation IDs must be unique.
    let all_ids: std::collections::HashSet<u64> = ids
        .iter()
        .flatten()
        .filter_map(|r| r.as_ref().ok().copied())
        .collect();
    assert_eq!(all_ids.len(), n_threads * per_thread);
}

#[test]
fn concurrent_mixed_readers_and_writers() {
    use rayon::prelude::*;
    use std::sync::Arc;
    let l = Arc::new(Ledger::new());
    l.register_lane(1, 10_000, 24);

    let n_threads = 8;
    (0..n_threads).into_par_iter().for_each(|t| {
        if t < 4 {
            // Writer threads
            for i in 0..100 {
                let _ = l.reserve(
                    ResourceKind::Lane,
                    1,
                    0,
                    1,
                    CommitmentLevel::Hard,
                    (t * 1000 + i) as u64,
                );
            }
        } else {
            // Reader threads
            for _ in 0..1000 {
                let _ = l.free_on(ResourceKind::Lane, 1, 0, CommitmentLevel::Hard);
            }
        }
    });
    // 4 writers × 100 each = 400 reservations.
    assert_eq!(l.n_reservations(), 400);
}
