//! Criterion benchmarks (workloads W1-W5).

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use ephemeris_fsm_py::{
    LedgerOpHints, LedgerResourceKind, RouteSegment, StateMachine, TransitionReason,
};

fn make_sm_with_n_entities(n: u64) -> StateMachine {
    let sm = StateMachine::new();
    for i in 0..n {
        sm.register(i).unwrap();
    }
    sm
}

/// W1: pure propose — does not mutate state. 1000 calls per batch.
fn bench_w1_propose(c: &mut Criterion) {
    let sm = make_sm_with_n_entities(10_000);
    let hints = LedgerOpHints {
        planned_route: vec![RouteSegment {
            kind: LedgerResourceKind::Lane,
            resource_id: 1,
            bucket: 0,
            units: 1,
        }],
        ..Default::default()
    };
    c.bench_function("W1_propose_1000", |b| {
        b.iter(|| {
            for i in 0..1000u64 {
                let _ = black_box(sm.propose(
                    i % 10_000,
                    TransitionReason::PlanFound,
                    0,
                    &hints,
                ));
            }
        });
    });
}

/// W2: pure commit_envelope — applies a previously-proposed transition.
fn bench_w2_commit(c: &mut Criterion) {
    c.bench_function("W2_commit_1000", |b| {
        b.iter_with_setup(
            || {
                let sm = make_sm_with_n_entities(2000);
                let hints = LedgerOpHints {
                    planned_route: vec![RouteSegment {
                        kind: LedgerResourceKind::Lane,
                        resource_id: 1,
                        bucket: 0,
                        units: 1,
                    }],
                    ..Default::default()
                };
                let envs: Vec<_> = (0..1000u64)
                    .map(|i| sm.propose(i, TransitionReason::PlanFound, 0, &hints).unwrap())
                    .collect();
                (sm, envs)
            },
            |(sm, envs)| {
                for env in &envs {
                    let _ = sm.commit_envelope(env);
                }
            },
        );
    });
}

/// W3: propose_and_commit roundtrip (the common case).
fn bench_w3_roundtrip(c: &mut Criterion) {
    let hints = LedgerOpHints::default();
    c.bench_function("W3_propose_and_commit_1000", |b| {
        b.iter_with_setup(
            || {
                let sm = make_sm_with_n_entities(2000);
                // Move each to Planned so we can do Planned→Dwelling cycles.
                let pf_hints = LedgerOpHints {
                    planned_route: vec![RouteSegment {
                        kind: LedgerResourceKind::Lane,
                        resource_id: 1,
                        bucket: 0,
                        units: 1,
                    }],
                    ..Default::default()
                };
                for i in 0..1000u64 {
                    sm.propose_and_commit(i, TransitionReason::PlanFound, 0, &pf_hints).unwrap();
                }
                sm
            },
            |sm| {
                for i in 0..1000u64 {
                    let _ = sm.propose_and_commit(
                        i,
                        TransitionReason::EnterHub(1),
                        1,
                        &hints,
                    );
                }
            },
        );
    });
}

/// W4: snapshot at various log sizes.
#[cfg(feature = "arrow")]
fn bench_w4_snapshot(c: &mut Criterion) {
    for n in [1_000u64, 10_000, 100_000] {
        let sm = StateMachine::new();
        let hints = LedgerOpHints {
            planned_route: vec![RouteSegment {
                kind: LedgerResourceKind::Lane,
                resource_id: 1,
                bucket: 0,
                units: 1,
            }],
            ..Default::default()
        };
        for i in 0..n {
            sm.register(i).unwrap();
            sm.propose_and_commit(i, TransitionReason::PlanFound, 0, &hints)
                .unwrap();
        }
        c.bench_function(&format!("W4_snapshot_{}", n), |b| {
            b.iter(|| black_box(sm.snapshot_log()));
        });
    }
}

/// W5: realistic DES-like — register + plan + enter + depart + arrive per entity.
fn bench_w5_des_like(c: &mut Criterion) {
    c.bench_function("W5_des_like_1000_flow_units", |b| {
        b.iter(|| {
            let sm = StateMachine::new();
            let pf_hints = LedgerOpHints {
                planned_route: vec![RouteSegment {
                    kind: LedgerResourceKind::Lane,
                    resource_id: 1,
                    bucket: 0,
                    units: 1,
                }],
                ..Default::default()
            };
            let empty = LedgerOpHints::default();
            for i in 0..1000u64 {
                sm.register(i).unwrap();
                sm.propose_and_commit(i, TransitionReason::PlanFound, 0, &pf_hints).unwrap();
                sm.propose_and_commit(i, TransitionReason::EnterHub(1), 1, &empty).unwrap();
                sm.propose_and_commit(i, TransitionReason::DepartHub(1), 2, &empty).unwrap();
                sm.propose_and_commit(
                    i,
                    TransitionReason::ArriveDestination {
                        quality: ephemeris_fsm_py::DeliveryQuality::OnTimeClean,
                    },
                    3,
                    &empty,
                )
                .unwrap();
            }
        });
    });
}

#[cfg(feature = "arrow")]
criterion_group!(
    benches,
    bench_w1_propose,
    bench_w2_commit,
    bench_w3_roundtrip,
    bench_w4_snapshot,
    bench_w5_des_like
);
#[cfg(not(feature = "arrow"))]
criterion_group!(
    benches,
    bench_w1_propose,
    bench_w2_commit,
    bench_w3_roundtrip,
    bench_w5_des_like
);
criterion_main!(benches);
