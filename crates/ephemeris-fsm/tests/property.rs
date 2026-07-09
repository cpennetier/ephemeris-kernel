//! Property tests for the state machine.
//!
//! Generates random sequences of valid (register, propose_and_commit)
//! ops and verifies invariants at every step.

use ephemeris_fsm_py::{
    DeliveryQuality, FailureMode, LedgerOpHints, LedgerResourceKind, RouteSegment,
    StateMachine, TransitionReason,
};
use proptest::prelude::*;

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

/// Generate a reason that's syntactically valid (parser would accept).
fn reason_strategy() -> impl Strategy<Value = TransitionReason> {
    prop_oneof![
        Just(TransitionReason::PlanFound),
        Just(TransitionReason::HoldByControl),
        Just(TransitionReason::ReleaseHold),
        Just(TransitionReason::Escalate),
        Just(TransitionReason::Recover),
        Just(TransitionReason::Strand),
        Just(TransitionReason::Expire),
        Just(TransitionReason::Unreachable),
        Just(TransitionReason::PolicyTerminate),
        Just(TransitionReason::Replan),
        (0u64..10u64).prop_map(TransitionReason::EnterHub),
        (0u64..10u64).prop_map(TransitionReason::DepartHub),
        (0u64..10u64).prop_map(TransitionReason::ArriveIntermediate),
        (0u32..3u32).prop_map(TransitionReason::Defer),
        Just(TransitionReason::ArriveDestination {
            quality: DeliveryQuality::OnTimeClean,
        }),
        Just(TransitionReason::ArriveDestination {
            quality: DeliveryQuality::LateClean,
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 500, .. ProptestConfig::default() })]

    /// For any random sequence of attempted transitions on a single
    /// entity, the state machine never reaches a state that doesn't
    /// satisfy the lifecycle invariants:
    ///   - history[i].from == history[i-1].to for i >= 1
    ///   - if history[i].to is terminal, no later transitions are accepted
    #[test]
    fn lifecycle_invariants_hold_under_random_ops(
        reasons in prop::collection::vec(reason_strategy(), 0..30)
    ) {
        let sm = StateMachine::new();
        sm.register(1).unwrap();
        let mut term = false;
        for (i, r) in reasons.iter().enumerate() {
            let res = sm.propose_and_commit(1, *r, i as u32, &pf_hints());
            if let Ok(env) = res {
                prop_assert!(!term, "transition accepted after terminal");
                if env.transition.to.discriminant() == ephemeris_fsm_py::StateDiscriminant::Delivered
                    || env.transition.to.discriminant() == ephemeris_fsm_py::StateDiscriminant::Failed
                {
                    term = true;
                }
            }
        }

        let hist = sm.history(1);
        for w in hist.windows(2) {
            prop_assert_eq!(w[0].to, w[1].from);
        }
        if let Some(last) = hist.last() {
            if last.to.is_terminal() {
                // Once terminal, count of further transitions = 0 means OK.
                // Already enforced by `term` check above; double-check via
                // re-attempt:
                let again = sm.propose(1, TransitionReason::PlanFound, 0, &pf_hints());
                prop_assert!(again.is_err());
            }
        }
    }

    /// Determinism: two fresh machines running the same legal subsequence
    /// produce identical state, log, version.
    #[test]
    fn determinism_under_random_legal_ops(
        n in 0u32..20u32,
    ) {
        let make = |seed: u32| {
            let sm = StateMachine::new();
            sm.register(1).unwrap();
            // Apply n PlanFound (only first will succeed; rest illegal).
            for i in 0..seed {
                let _ = sm.propose_and_commit(1, TransitionReason::PlanFound, i, &pf_hints());
            }
            sm
        };
        let a = make(n);
        let b = make(n);
        prop_assert_eq!(a.state(1), b.state(1));
        prop_assert_eq!(a.version(), b.version());
        prop_assert_eq!(a.all_transitions(), b.all_transitions());

        let _ = FailureMode::Stranded; // touch unused import to silence warning
    }
}
