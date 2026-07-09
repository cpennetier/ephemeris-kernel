//! Criterion benchmarks for the capacity ledger (workloads W1-W5).

use ephemeris_ledger_py::{CommitmentLevel, Ledger, ResourceKind};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::sync::Arc;

fn setup_ledger(n_resources: u64, n_buckets: u32, capacity: u64) -> Ledger {
    let l = Ledger::new();
    for r in 0..n_resources {
        l.register_lane(r, capacity, n_buckets);
    }
    l
}

fn populate(l: &Ledger, n_reservations: u64, n_resources: u64) {
    for i in 0..n_reservations {
        let r = i % n_resources;
        let b = (i / n_resources) as u32 % 24;
        let _ = l.reserve(ResourceKind::Lane, r, b, 1, CommitmentLevel::Hard, i);
    }
}

/// W1: Pure read — 1M free_on calls on a populated ledger.
fn bench_w1_pure_read(c: &mut Criterion) {
    let l = setup_ledger(100, 24, 100);
    populate(&l, 10_000, 100);
    c.bench_function("W1_pure_read_single_thread", |b| {
        b.iter(|| {
            for i in 0..1000 {
                let r = (i % 100) as u64;
                let bucket = (i / 100) as u32 % 24;
                black_box(l.free_on(ResourceKind::Lane, r, bucket, CommitmentLevel::Hard));
            }
        });
    });
}

/// W2: Pure write — 100K reserve calls on a fresh ledger.
fn bench_w2_pure_write(c: &mut Criterion) {
    c.bench_function("W2_pure_write_single_thread_100k", |b| {
        b.iter_with_setup(
            || setup_ledger(100, 24, 10_000),
            |l| {
                for i in 0..1000 {
                    let r = (i % 100) as u64;
                    let bucket = (i / 100) as u32 % 24;
                    let _ = l.reserve(
                        ResourceKind::Lane,
                        r,
                        bucket,
                        1,
                        CommitmentLevel::Hard,
                        i as u64,
                    );
                }
            },
        );
    });
}

/// W3: Mixed 90/10 read/write workload, 4 threads.
fn bench_w3_mixed_concurrent(c: &mut Criterion) {
    use rayon::prelude::*;
    let l = Arc::new(setup_ledger(100, 24, 10_000));
    populate(&l, 10_000, 100);
    c.bench_function("W3_mixed_90r_10w_4threads", |b| {
        b.iter(|| {
            (0..4).into_par_iter().for_each(|t| {
                for i in 0..250 {
                    if i % 10 == 0 {
                        // Write
                        let r = ((t * 250 + i) % 100) as u64;
                        let bucket = (i % 24) as u32;
                        let _ = l.reserve(
                            ResourceKind::Lane,
                            r,
                            bucket,
                            1,
                            CommitmentLevel::Hard,
                            (t * 250 + i) as u64,
                        );
                    } else {
                        // Read
                        let r = ((t * 250 + i) % 100) as u64;
                        let bucket = (i % 24) as u32;
                        black_box(l.free_on(
                            ResourceKind::Lane,
                            r,
                            bucket,
                            CommitmentLevel::Hard,
                        ));
                    }
                }
            });
        });
    });
}

/// W4: Snapshot latency at various sizes.
#[cfg(feature = "arrow")]
fn bench_w4_snapshot(c: &mut Criterion) {
    for n in [1_000u64, 10_000, 100_000] {
        let l = setup_ledger(100, 24, 1_000_000);
        populate(&l, n, 100);
        c.bench_function(&format!("W4_snapshot_{}", n), |b| {
            b.iter(|| black_box(l.snapshot()));
        });
    }
}

/// W5: DES-like workload — 100K reservations at random buckets.
fn bench_w5_des_like(c: &mut Criterion) {
    c.bench_function("W5_des_like_100k", |b| {
        b.iter_with_setup(
            || setup_ledger(500, 24, 1_000),
            |l| {
                // Pseudo-random bucket choice via deterministic hash.
                for i in 0..1000 {
                    let r = (i * 31 % 500) as u64;
                    let bucket = (i * 17 % 24) as u32;
                    let _ = l.reserve(
                        ResourceKind::Lane,
                        r,
                        bucket,
                        1,
                        CommitmentLevel::Hard,
                        i as u64 % 100,
                    );
                }
            },
        );
    });
}

#[cfg(feature = "arrow")]
criterion_group!(
    benches,
    bench_w1_pure_read,
    bench_w2_pure_write,
    bench_w3_mixed_concurrent,
    bench_w4_snapshot,
    bench_w5_des_like
);
#[cfg(not(feature = "arrow"))]
criterion_group!(
    benches,
    bench_w1_pure_read,
    bench_w2_pure_write,
    bench_w3_mixed_concurrent,
    bench_w5_des_like
);
criterion_main!(benches);
