# PRD: what Ephemeris Kernel is for

Status: ongoing, `v0.1.0` tagged on 2026-07-09. Owner: Christophe Pennetier (`pyproject.toml`). Written on 2026-11-11 from commit `9c2525f`. No earlier PRD existed, so every section is new.

## Problem and why now

A policy can be compared with another only if the environment that executes both is stable. A simulator that mixes execution with decision rules, or that breaks ties by accident, makes such a comparison unreliable. Ephemeris keeps execution apart from decision making: it runs imposed decisions over an integer clock, records what happened, and gives the same result for the same inputs (`README.md`, "Why it exists").

Why now: `v0.1.0` is public and installable by tag. The README states a determinism contract, but part of it is not yet held by a test (`docs/ERD.md`, Guarantees). Writing the requirements down lets the next changes close those gaps without widening the scope.

## For whom

- People and tools that evaluate policies or planners and need a trusted execution layer beneath them. The README names one downstream certification and evaluation harness; it imports Ephemeris, never the reverse.
- The research program stated at the top of `README.md`, in which this repository plays the role of deterministic execution and causal ground truth.

The job it does for them: from Arrow tables for nodes, lanes, schedules and demand, plus decisions imposed through public hooks, it executes the run and returns exact state, surfaces, records and a run manifest that someone else can rerun.

## Goals and non-goals

Goals:
- A total event order `(time_atu, event_id)` over an integer millisecond clock.
- Byte-identical outputs for identical inputs on one platform.
- Capacity reservation, promotion and release with monotonic versions.
- An append-only transition history for every entity.
- Neutral extension points, and no consumer logic.

Non-goals, from `README.md`: an optimizer, a policy framework, an end-to-end application, a learned simulator, a distributed event platform, and any claim of cross-platform byte identity without qualification.

## What success looks like

| Measure | Threshold | How it is measured | On 2026-11-11 |
|---|---|---|---|
| `make test` | green | run before every push to `main` | green: 193 Rust and 15 Python tests, 6.2 s wall with a warm build cache |
| Output bytes of two identical runs | 0 differing bytes in the surface stream and the manifest | a byte-comparison test | not measured, no such test yet (R1) |
| Banned domain or consumer terms in tracked files | 0 | `tests/test_no_domain_vocabulary.py` | 0 |
| Flow units conserved | injected = delivered + stranded + in flight at cutoff | `test_mass_conservation_with_in_flight_at_cutoff` | holds |

Speed and memory thresholds are not set (open question 3).

## The goal it serves

The research program in `README.md`: proposal mechanisms may be heuristic or learned, but the structures they produce, their execution and their value stay independently measurable. Ephemeris supplies the execution and the ground truth that the measurement rests on.

## Where its result goes

- The public repository `github.com/cpennetier/ephemeris-kernel`, under Apache-2.0, installable with pip from a tag (`README.md`, "Install").
- Downstream evaluation code that pins a tag, starting with the harness the README names.
- No paper, post or talk that uses it is recorded in the repository (open question 2).

## Requirements

Must:
- M1. Events run in `(time_atu ASC, event_id ASC)` order with `i64` time. Tests: `test_time_is_i64_atu`, `test_no_f64_time` in `crates/ephemeris-des/src/tests.rs`.
- M2. Identical inputs, seed and configuration give byte-identical surface stream, snapshots and manifest on one platform. Tests: partial today (`docs/ERD.md`); the byte comparison is R1.
- M3. No policy, domain vocabulary or consumer reference enters the code or the internal documents. Test: `tests/test_no_domain_vocabulary.py`.
- M4. Flow units are conserved. Tests: `test_mass_conservation`, `test_mass_conservation_with_in_flight_at_cutoff`.
- M5. Enforced capacity is never exceeded, in the engine or in the ledger. Tests: `test_capacity_enforcement`, `concurrent_reservations_no_double_booking`, `lattice_property_holds_under_arbitrary_ops`.
- M6. Entity history is append-only and chained, and a proposal never mutates state. Tests: `history_chain_from_equals_previous_to`, `propose_does_not_mutate`, `stale_envelope_detected`.
- M7. Ledger versions and reservation ids only increase. Tests: `version_increments_on_writes`, `reservation_ids_monotonic`.
- M8. A run can emit a manifest with the seed, schema version and fingerprint, build SHA and configuration hash, embedded in the surface stream. Tests: `manifest_v1_has_required_fields_and_defaults`, `run_manifest_embeds_in_stream_schema`.

Should:
- S1. The Python package that `pip install` provides is exercised by a test. Today no test imports `ephemeris._engine` (R2).
- S2. A malformed call raises a Python error, never a panic or a silent default (R3).
- S3. Any change to output bytes is either a bug or a new `SCHEMA_VERSION` (`crates/ephemeris-des/src/surface_writer.rs`). Test: none yet beyond R1.

Could:
- C1. A partitioned, multi-threaded executor. The manifest already reserves `partition_id`, `partition_hash` and `thread_count` (`crates/ephemeris-des/src/run_manifest.rs`).
- C2. A Parquet run store, where JSON is the v1 choice (same file).

## Open questions for the owner

1. The capacity ledger's `release` deletes the reservation (`crates/ephemeris-ledger/src/ledger.rs`); only the version shows that something changed. Should released reservations stay on record, so that the README's question "what capacity was reserved or released" can be answered after the fact (R4)?
2. Which paper, post or site will use this result?
3. Which speed and memory budgets should hold, and on which scenario? The benchmarks run 50, 200 and 500 nodes.
4. Should `crates/ephemeris-des/tests/h3_equivalence.rs`, which `make test-rust` skips through `--lib`, run before every push, or move to the offline pipeline it protects (R8)?
