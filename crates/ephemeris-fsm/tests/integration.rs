//! Integration tests covering the full canonical transition table,
//! envelope semantics, and concurrent / determinism contracts.

use ephemeris_fsm_py::{
    DeliveryQuality, EntityState, FailureMode, LedgerCommitmentLevel, LedgerOp, LedgerOpHints,
    LedgerResourceKind, RouteSegment, StateDiscriminant, StateMachine, StateMachineError,
    TransitionReason,
};

fn pf_hints() -> LedgerOpHints {
    LedgerOpHints {
        planned_route: vec![RouteSegment {
            kind: LedgerResourceKind::Lane,
            resource_id: 1,
            bucket: 0,
            units: 1,
        }],
        ..Default::default()
    }
}

fn plan_into_dwelling(sm: &StateMachine, id: u64) {
    sm.register(id).unwrap();
    sm.propose_and_commit(id, TransitionReason::PlanFound, 0, &pf_hints())
        .unwrap();
    sm.propose_and_commit(id, TransitionReason::EnterHub(1), 1, &Default::default())
        .unwrap();
}

// ─── Basic invariants ──────────────────────────────────────────────────

#[test]
fn empty_machine_has_no_state() {
    let sm = StateMachine::new();
    assert_eq!(sm.state(1), None);
    assert_eq!(sm.version(), 0);
    assert_eq!(sm.n_entities(), 0);
    assert_eq!(sm.log_len(), 0);
}

#[test]
fn register_creates_in_created() {
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Created));
    assert!(sm.version() > 0);
}

#[test]
fn register_idempotency_errors() {
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    let err = sm.register(1).unwrap_err();
    matches!(err, StateMachineError::EntityAlreadyRegistered(1));
}

#[test]
fn history_empty_for_just_registered() {
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    assert_eq!(sm.history(1).len(), 0);
}

#[test]
fn entities_in_state_groups_correctly() {
    let sm = StateMachine::new();
    for i in 0..5u64 {
        sm.register(i).unwrap();
    }
    let created = sm.entities_in_state(StateDiscriminant::Created);
    assert_eq!(created.len(), 5);
}

// ─── Canonical transition table coverage ──────────────────────────────

#[test]
fn created_to_planned() {
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    sm.propose_and_commit(1, TransitionReason::PlanFound, 0, &pf_hints())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Planned));
}

#[test]
fn created_to_failed_unreachable() {
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    sm.propose_and_commit(1, TransitionReason::Unreachable, 0, &Default::default())
        .unwrap();
    assert_eq!(
        sm.state(1),
        Some(EntityState::Failed(FailureMode::UnreachableTopology))
    );
}

#[test]
fn planned_to_dwelling() {
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    sm.propose_and_commit(1, TransitionReason::PlanFound, 0, &pf_hints())
        .unwrap();
    sm.propose_and_commit(1, TransitionReason::EnterHub(7), 1, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Dwelling));
}

#[test]
fn planned_to_failed_stranded() {
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    sm.propose_and_commit(1, TransitionReason::PlanFound, 0, &pf_hints())
        .unwrap();
    sm.propose_and_commit(1, TransitionReason::Strand, 1, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Failed(FailureMode::Stranded)));
}

#[test]
fn dwelling_to_moving() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::DepartHub(1), 2, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Moving));
}

#[test]
fn dwelling_to_held() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::HoldByControl, 2, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Held));
}

#[test]
fn dwelling_to_deferred() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::Defer(3), 2, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Deferred));
}

#[test]
fn dwelling_to_escalated() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::Escalate, 2, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Escalated));
}

#[test]
fn dwelling_to_delivered_on_time_clean() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(
        1,
        TransitionReason::ArriveDestination {
            quality: DeliveryQuality::OnTimeClean,
        },
        2,
        &Default::default(),
    )
    .unwrap();
    assert_eq!(
        sm.state(1),
        Some(EntityState::Delivered(DeliveryQuality::OnTimeClean))
    );
}

#[test]
fn dwelling_to_failed_expired() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::Expire, 99, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Failed(FailureMode::Expired)));
}

#[test]
fn moving_to_dwelling_intermediate() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::DepartHub(1), 2, &Default::default())
        .unwrap();
    sm.propose_and_commit(1, TransitionReason::ArriveIntermediate(2), 3, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Dwelling));
}

#[test]
fn moving_to_delivered_all_qualities() {
    for q in [
        DeliveryQuality::OnTimeClean,
        DeliveryQuality::LateClean,
        DeliveryQuality::OnTimeViolating,
        DeliveryQuality::LateViolating,
    ] {
        let sm = StateMachine::new();
        plan_into_dwelling(&sm, 1);
        sm.propose_and_commit(1, TransitionReason::DepartHub(1), 2, &Default::default())
            .unwrap();
        sm.propose_and_commit(
            1,
            TransitionReason::ArriveDestination { quality: q },
            3,
            &Default::default(),
        )
        .unwrap();
        assert_eq!(sm.state(1), Some(EntityState::Delivered(q)));
    }
}

#[test]
fn moving_to_failed_expired() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::DepartHub(1), 2, &Default::default())
        .unwrap();
    sm.propose_and_commit(1, TransitionReason::Expire, 99, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Failed(FailureMode::Expired)));
}

#[test]
fn held_to_dwelling_via_release() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::HoldByControl, 2, &Default::default())
        .unwrap();
    sm.propose_and_commit(1, TransitionReason::ReleaseHold, 3, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Dwelling));
}

#[test]
fn held_to_deferred() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::HoldByControl, 2, &Default::default())
        .unwrap();
    sm.propose_and_commit(1, TransitionReason::Defer(5), 3, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Deferred));
}

#[test]
fn held_to_escalated() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::HoldByControl, 2, &Default::default())
        .unwrap();
    sm.propose_and_commit(1, TransitionReason::Escalate, 3, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Escalated));
}

#[test]
fn held_to_failed_stranded() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::HoldByControl, 2, &Default::default())
        .unwrap();
    sm.propose_and_commit(1, TransitionReason::Strand, 3, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Failed(FailureMode::Stranded)));
}

#[test]
fn held_to_failed_policy_forbid() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::HoldByControl, 2, &Default::default())
        .unwrap();
    sm.propose_and_commit(1, TransitionReason::PolicyTerminate, 3, &Default::default())
        .unwrap();
    assert_eq!(
        sm.state(1),
        Some(EntityState::Failed(FailureMode::PolicyForbid))
    );
}

#[test]
fn deferred_to_planned() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::Defer(5), 2, &Default::default())
        .unwrap();
    sm.propose_and_commit(1, TransitionReason::Replan, 3, &pf_hints())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Planned));
}

#[test]
fn escalated_to_moving() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::Escalate, 2, &Default::default())
        .unwrap();
    sm.propose_and_commit(1, TransitionReason::DepartHub(1), 3, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Moving));
}

#[test]
fn escalated_to_recovered() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::Escalate, 2, &Default::default())
        .unwrap();
    sm.propose_and_commit(1, TransitionReason::Recover, 3, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Recovered));
}

#[test]
fn recovered_to_delivered_late_clean() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::Escalate, 2, &Default::default())
        .unwrap();
    sm.propose_and_commit(1, TransitionReason::Recover, 3, &Default::default())
        .unwrap();
    sm.propose_and_commit(
        1,
        TransitionReason::ArriveDestination {
            quality: DeliveryQuality::LateClean,
        },
        4,
        &Default::default(),
    )
    .unwrap();
    assert_eq!(
        sm.state(1),
        Some(EntityState::Delivered(DeliveryQuality::LateClean))
    );
}

// ─── All FailureMode variants reachable ──────────────────────────────

#[test]
fn all_failure_modes_reachable() {
    // Stranded — from Planned
    {
        let sm = StateMachine::new();
        sm.register(1).unwrap();
        sm.propose_and_commit(1, TransitionReason::PlanFound, 0, &pf_hints()).unwrap();
        sm.propose_and_commit(1, TransitionReason::Strand, 1, &Default::default()).unwrap();
        assert_eq!(sm.state(1), Some(EntityState::Failed(FailureMode::Stranded)));
    }
    // Expired — from Dwelling
    {
        let sm = StateMachine::new();
        plan_into_dwelling(&sm, 1);
        sm.propose_and_commit(1, TransitionReason::Expire, 99, &Default::default()).unwrap();
        assert_eq!(sm.state(1), Some(EntityState::Failed(FailureMode::Expired)));
    }
    // UnreachableTopology — from Created
    {
        let sm = StateMachine::new();
        sm.register(1).unwrap();
        sm.propose_and_commit(1, TransitionReason::Unreachable, 0, &Default::default()).unwrap();
        assert_eq!(sm.state(1), Some(EntityState::Failed(FailureMode::UnreachableTopology)));
    }
    // PolicyForbid — from Held
    {
        let sm = StateMachine::new();
        plan_into_dwelling(&sm, 1);
        sm.propose_and_commit(1, TransitionReason::HoldByControl, 2, &Default::default()).unwrap();
        sm.propose_and_commit(1, TransitionReason::PolicyTerminate, 3, &Default::default()).unwrap();
        assert_eq!(sm.state(1), Some(EntityState::Failed(FailureMode::PolicyForbid)));
    }
}

// ─── Illegal transitions rejected ─────────────────────────────────────

#[test]
fn created_to_dwelling_illegal() {
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    let err = sm
        .propose(1, TransitionReason::EnterHub(1), 0, &Default::default())
        .unwrap_err();
    matches!(err, StateMachineError::IllegalTransition { .. });
}

#[test]
fn terminal_state_rejects_outgoing() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(
        1,
        TransitionReason::ArriveDestination {
            quality: DeliveryQuality::OnTimeClean,
        },
        2,
        &Default::default(),
    )
    .unwrap();
    let err = sm
        .propose(1, TransitionReason::DepartHub(1), 3, &Default::default())
        .unwrap_err();
    matches!(err, StateMachineError::TerminalState(_));
}

// ─── Envelope semantics ───────────────────────────────────────────────

#[test]
fn propose_does_not_mutate() {
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    let v0 = sm.version();
    let _env = sm.propose(1, TransitionReason::PlanFound, 0, &pf_hints()).unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Created));
    assert_eq!(sm.version(), v0);
}

#[test]
fn commit_mutates_state() {
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    let env = sm.propose(1, TransitionReason::PlanFound, 0, &pf_hints()).unwrap();
    sm.commit_envelope(&env).unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Planned));
}

#[test]
fn stale_envelope_detected() {
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    let env_a = sm.propose(1, TransitionReason::PlanFound, 0, &pf_hints()).unwrap();
    let env_b = sm.propose(1, TransitionReason::Unreachable, 0, &Default::default()).unwrap();
    // Commit env_b first.
    sm.commit_envelope(&env_b).unwrap();
    // Now env_a is stale.
    let err = sm.commit_envelope(&env_a).unwrap_err();
    matches!(err, StateMachineError::StaleEnvelope { .. });
}

#[test]
fn envelope_ledger_ops_plan_found_reserves() {
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    let hints = LedgerOpHints {
        planned_route: vec![
            RouteSegment {
                kind: LedgerResourceKind::Lane,
                resource_id: 1,
                bucket: 0,
                units: 1,
            },
            RouteSegment {
                kind: LedgerResourceKind::Lane,
                resource_id: 2,
                bucket: 1,
                units: 1,
            },
        ],
        ..Default::default()
    };
    let env = sm.propose(1, TransitionReason::PlanFound, 0, &hints).unwrap();
    assert_eq!(env.ledger_ops.len(), 2);
    for op in &env.ledger_ops {
        match op {
            LedgerOp::Reserve { level, .. } => {
                assert_eq!(*level, LedgerCommitmentLevel::Hard);
            }
            _ => panic!("unexpected op"),
        }
    }
}

#[test]
fn envelope_ledger_ops_strand_releases() {
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    sm.propose_and_commit(1, TransitionReason::PlanFound, 0, &pf_hints()).unwrap();
    let hints = LedgerOpHints {
        current_reservations: vec![(10, LedgerCommitmentLevel::Hard), (11, LedgerCommitmentLevel::Hard)],
        ..Default::default()
    };
    let env = sm.propose(1, TransitionReason::Strand, 1, &hints).unwrap();
    assert_eq!(env.ledger_ops.len(), 2);
    for op in &env.ledger_ops {
        matches!(op, LedgerOp::Release { .. });
    }
}

#[test]
fn envelope_ledger_ops_hold_by_control_downgrades_hard_reservations() {
    // B-D1: previously this branch emitted Promote { new_level: Soft },
    // which the ledger rejects because Promote only moves upward. The
    // fix replaces Promote with Downgrade. This test asserts the
    // envelope now carries Downgrade ops for every Hard reservation,
    // and Soft/Forecast reservations are not touched.
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    let hints = LedgerOpHints {
        current_reservations: vec![
            (10, LedgerCommitmentLevel::Hard),
            (11, LedgerCommitmentLevel::Hard),
            (12, LedgerCommitmentLevel::Soft),       // not touched
            (13, LedgerCommitmentLevel::Forecast),   // not touched
        ],
        ..Default::default()
    };
    let env = sm
        .propose(1, TransitionReason::HoldByControl, 2, &hints)
        .unwrap();
    // Exactly the two Hard reservations should be downgraded.
    assert_eq!(env.ledger_ops.len(), 2, "expected 2 downgrades, got {:?}", env.ledger_ops);
    let mut downgraded_ids: Vec<u64> = env
        .ledger_ops
        .iter()
        .map(|op| match op {
            LedgerOp::Downgrade { reservation_id, new_level } => {
                assert_eq!(
                    *new_level,
                    LedgerCommitmentLevel::Soft,
                    "HoldByControl must downgrade to Soft"
                );
                *reservation_id
            }
            other => panic!("expected LedgerOp::Downgrade, got {:?}", other),
        })
        .collect();
    downgraded_ids.sort();
    assert_eq!(downgraded_ids, vec![10, 11]);
}

#[test]
fn envelope_ledger_ops_arrive_destination_releases_all() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    let hints = LedgerOpHints {
        current_reservations: vec![(10, LedgerCommitmentLevel::Hard)],
        ..Default::default()
    };
    let env = sm
        .propose(
            1,
            TransitionReason::ArriveDestination {
                quality: DeliveryQuality::OnTimeClean,
            },
            2,
            &hints,
        )
        .unwrap();
    assert_eq!(env.ledger_ops.len(), 1);
    matches!(env.ledger_ops[0], LedgerOp::Release { .. });
}

// ─── Concurrent correctness ──────────────────────────────────────────

#[test]
fn concurrent_propose_only_one_commits() {
    use rayon::prelude::*;
    use std::sync::Arc;

    let sm = Arc::new(StateMachine::new());
    sm.register(1).unwrap();

    // 10 threads each propose a competing transition.
    let envelopes: Vec<_> = (0..10)
        .map(|_| {
            sm.propose(1, TransitionReason::PlanFound, 0, &pf_hints())
                .unwrap()
        })
        .collect();
    // All 10 envelopes are concurrent commits.
    let results: Vec<_> = envelopes
        .par_iter()
        .map(|env| sm.commit_envelope(env))
        .collect();
    let successes = results.iter().filter(|r| r.is_ok()).count();
    let stales = results
        .iter()
        .filter(|r| matches!(r, Err(StateMachineError::StaleEnvelope { .. })))
        .count();
    // Exactly one should succeed; the rest should be stale.
    assert_eq!(successes, 1);
    assert_eq!(stales, 9);
}

#[test]
fn concurrent_different_entities_no_interference() {
    use rayon::prelude::*;
    use std::sync::Arc;

    let sm = Arc::new(StateMachine::new());
    for i in 0..100u64 {
        sm.register(i).unwrap();
    }
    (0..100u64).into_par_iter().for_each(|i| {
        sm.propose_and_commit(i, TransitionReason::PlanFound, 0, &pf_hints()).unwrap();
    });
    for i in 0..100u64 {
        assert_eq!(sm.state(i), Some(EntityState::Planned));
    }
}

// ─── Determinism ─────────────────────────────────────────────────────

#[test]
fn determinism_same_sequence_produces_same_state() {
    let make = || {
        let sm = StateMachine::new();
        sm.register(1).unwrap();
        sm.propose_and_commit(1, TransitionReason::PlanFound, 0, &pf_hints()).unwrap();
        sm.propose_and_commit(1, TransitionReason::EnterHub(1), 1, &Default::default()).unwrap();
        sm.propose_and_commit(1, TransitionReason::DepartHub(1), 2, &Default::default()).unwrap();
        sm.propose_and_commit(
            1,
            TransitionReason::ArriveDestination {
                quality: DeliveryQuality::OnTimeClean,
            },
            3,
            &Default::default(),
        )
        .unwrap();
        sm
    };
    let a = make();
    let b = make();
    assert_eq!(a.state(1), b.state(1));
    assert_eq!(a.version(), b.version());
    assert_eq!(a.all_transitions(), b.all_transitions());
}

// ─── History invariants ──────────────────────────────────────────────

#[test]
fn history_chain_from_equals_previous_to() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::DepartHub(1), 2, &Default::default()).unwrap();
    let hist = sm.history(1);
    for w in hist.windows(2) {
        assert_eq!(w[0].to, w[1].from);
    }
}

#[test]
fn terminal_history_ends_with_terminal() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(
        1,
        TransitionReason::ArriveDestination {
            quality: DeliveryQuality::OnTimeClean,
        },
        2,
        &Default::default(),
    )
    .unwrap();
    let hist = sm.history(1);
    assert!(hist.last().unwrap().to.is_terminal());
}

// ─── PreserveDeliverable self-loop ────────────────────────────────────

#[test]
fn preserve_deliverable_self_loop_in_dwelling() {
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    assert_eq!(sm.state(1), Some(EntityState::Dwelling));
    sm.propose_and_commit(
        1,
        TransitionReason::PreserveDeliverable,
        2,
        &Default::default(),
    )
    .unwrap();
    // State unchanged; log appended.
    assert_eq!(sm.state(1), Some(EntityState::Dwelling));
    let hist = sm.history(1);
    let last = hist.last().unwrap();
    assert_eq!(last.from, EntityState::Dwelling);
    assert_eq!(last.to, EntityState::Dwelling);
    assert_eq!(last.reason, TransitionReason::PreserveDeliverable);
}

#[test]
fn preserve_deliverable_self_loop_in_created() {
    // Admission-time path: the controller's R2 decides to admit a flow unit
    // before any PlanFound transition. The state machine accepts the
    // self-loop and logs it.
    let sm = StateMachine::new();
    sm.register(1).unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Created));
    sm.propose_and_commit(
        1,
        TransitionReason::PreserveDeliverable,
        0,
        &Default::default(),
    )
    .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Created));
    let hist = sm.history(1);
    assert_eq!(hist.last().unwrap().reason, TransitionReason::PreserveDeliverable);
    assert_eq!(hist.last().unwrap().from, EntityState::Created);
    assert_eq!(hist.last().unwrap().to, EntityState::Created);
}

#[test]
fn preserve_deliverable_appendable_multiple_times() {
    // Multiple R2 firings on the same flow unit mid-journey produce
    // multiple log entries.
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    for b in 2..7 {
        sm.propose_and_commit(
            1,
            TransitionReason::PreserveDeliverable,
            b,
            &Default::default(),
        )
        .unwrap();
    }
    let preserve_count = sm
        .history(1)
        .iter()
        .filter(|t| t.reason == TransitionReason::PreserveDeliverable)
        .count();
    assert_eq!(preserve_count, 5);
    assert_eq!(sm.state(1), Some(EntityState::Dwelling));
}

#[test]
fn preserve_deliverable_illegal_from_moving() {
    // Moving has no PreserveDeliverable rule — controller only fires
    // R2 when the flow unit is Dwelling at a node (or Created at admission).
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    sm.propose_and_commit(1, TransitionReason::DepartHub(1), 2, &Default::default())
        .unwrap();
    assert_eq!(sm.state(1), Some(EntityState::Moving));
    let err = sm
        .propose_and_commit(
            1,
            TransitionReason::PreserveDeliverable,
            3,
            &Default::default(),
        )
        .unwrap_err();
    matches!(
        err,
        StateMachineError::IllegalTransition {
            reason: TransitionReason::PreserveDeliverable,
            ..
        }
    );
}

#[test]
fn preserve_deliverable_emits_no_ledger_ops() {
    // PreserveDeliverable does not touch
    // the ledger. The envelope's ledger_ops must be empty.
    let sm = StateMachine::new();
    plan_into_dwelling(&sm, 1);
    let env = sm
        .propose(
            1,
            TransitionReason::PreserveDeliverable,
            2,
            &Default::default(),
        )
        .unwrap();
    assert!(
        env.ledger_ops.is_empty(),
        "PreserveDeliverable must not emit ledger ops, got {:?}",
        env.ledger_ops,
    );
    let _: &[LedgerOp] = &env.ledger_ops;
}
