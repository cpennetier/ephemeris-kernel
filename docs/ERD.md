# ERD: what the engine and its data must provide

Written on 2026-11-11 from commit `9c2525f`. Paths below are relative to `crates/ephemeris-des/` unless they name another crate.

## Inputs and outputs

No separate schema file exists. The schemas live in code, cited here.

Inputs are Arrow IPC stream bytes, read by `src/io.rs`:

| Table | Columns |
|---|---|
| nodes | `node_id` u32, dense `[0, N)`; `tier` utf8 (`"leaf"` marks leaves); `cell_id_r3` u64; `cell_centroid_lat`, `cell_centroid_lon` f64 |
| lanes | `lane_id`, `src_node_id`, `dst_node_id` u32; `lane_mode` u8; `duration_atu`, `cost_usd_micros`, `co2_g` i64 |
| schedules | `lane_id` u32, `depart_atu` i64, `capacity` u32 |
| demand | `flow_unit_id` u64; `appear_atu`, `due_atu` i64; `origin_node_id`, `dest_node_id`, `size_units` u32; `priority_class`, `handling_flags` u8 |

The constructor is `PyEngine(node_ipc, lane_ipc, schedule_ipc, demand_ipc, bucket_atu=3_600_000, router="time_aware")` (`src/python.rs`). Decisions enter only through the hooks the README lists: `set_forced_routes`, `set_forced_routes_pinned`, `cancel_pinned_routes`, `apply_schedule_ops`, `inject_demand`, `set_lane_outages`, `set_node_outages`. Time is `i64` milliseconds and ids are dense integers (`src/types.rs`).

Outputs:
- Surface stream: Arrow IPC, one batch per bucket, 26 columns plus 3 cumulative ones when enabled, schema version `2026.05.surface.v1`. Readers index columns by position. The schema metadata carries `schema_version`, `schema_fingerprint` (the first 16 hex characters of SHA-256 over the sorted column names) and `run_manifest` (`src/surface_writer.rs`).
- Run manifest: JSON at `{runs_dir}/{run_id}/run_manifest.json`, 13 fields in a fixed order (`src/run_manifest.rs`). The caller supplies `created_at`; the engine reads no wall clock for it.
- KPIs: JSON from `get_kpis` (`src/kpi.rs`). It includes `simulation_wall_time_ms`, so KPI JSON is not byte-stable by design.
- Records: strand, delivery, opt-in hop trace, realized boardings, `slot_state`, `oracle_verdicts`.
- Snapshots: the ledger's 8 columns ordered by `reservation_id` (`crates/ephemeris-ledger/src/snapshot.rs`) and the transition log's 8 columns (`crates/ephemeris-fsm/src/snapshot.rs`).

## Guarantees

| Guarantee | Held by |
|---|---|
| Event order `(time_atu, event_id)`, integer time | `OrderedEvent` in `src/events.rs`; `test_time_is_i64_atu`, `test_no_f64_time` |
| Same inputs give the same counts and delivery log within one process | `test_deterministic_output`, `deterministic_same_seed_byte_identical` (`src/tests.rs`) |
| Same inputs give byte-identical surface stream and manifest | **not tested.** The two tests above compare counts and a sorted log, not bytes (R1) |
| Ledger snapshot and machine state are deterministic | `snapshot_is_deterministic`, `two_identical_sequences_produce_identical_state`, `determinism_under_random_legal_ops` |
| Conservation, no double count | `test_mass_conservation_with_in_flight_at_cutoff`, `test_no_double_count_invariant` |
| Capacity is respected | `test_capacity_enforcement`, `hard_reservation_fails_at_capacity_boundary`, `lattice_property_holds_under_arbitrary_ops` |
| No departure on a lane during its outage | `prop_no_event_during_outage` |
| A pinned flow unit boards its exact slot; an illegal plan is rejected whole | `pinned_flow_unit_boards_exactly_its_slot_not_the_earlier_one`, `install_is_atomic_and_rejects_illegal_plans` |
| History is chained; nothing follows a terminal state | `history_chain_from_equals_previous_to`, `lifecycle_invariants_hold_under_random_ops` |
| Ledger versions and reservation ids increase | `version_increments_on_writes`, `reservation_ids_monotonic` |
| Fingerprint ignores column order and changes on rename | `fingerprint_is_deterministic_and_order_independent` |
| No domain vocabulary or consumer name | `tests/test_no_domain_vocabulary.py` |

The engine draws no random numbers; no generator appears in the crate sources. The `seed` is recorded in the manifest for the consumer and is not consumed. Where accumulation order matters, `HashMap` contents are sorted first (`finalize_strands` in `src/engine.rs`).

Not guaranteed today:
- The capacity ledger is not append-only. `release` removes the reservation (`crates/ephemeris-ledger/src/ledger.rs`); only the version records the change (R4).
- The manifest carries SHA-256 hashes but is not signed.

## Data

- Sources: the consumer supplies every table as bytes. The repository ships no scenario. Its only data is the 26-cell H3 fixture inside `tests/h3_equivalence.rs`.
- Owner and location: inputs stay with the consumer. The engine writes only the manifest, under the `runs_dir` the caller names.
- Retention: the caller decides. The engine deletes nothing; a manifest written again for the same `run_id` replaces the old one.
- Private: nothing. No personal data is processed, and the repository is public.

## Budgets

No budget is declared. Measured on 2026-11-11 on the development machine, with a 1-minute load of 9.43:
- `make test`: 6.2 s wall with a warm build cache.
- The two proptest suites: 0.58 s and 0.84 s.

Criterion benchmarks exist (`crates/*/benches/`), with full runs at 50 nodes and 1,000 flow units, 200 and 10,000, and 500 and 10,000. No baseline is recorded. Memory: the engine holds every live flow unit; streaming mode (`step_until`, then `drain_surfaces_arrow`) moves surfaces out, so surfaces held at once are bounded by one chunk. Running costs nothing: no network, no paid service.

## Failure

- Missing column or wrong type in an input table: `IOError::MissingColumn` or `TypeMismatch`, raised in Python as `IOError` (`src/io.rs`, `src/python.rs`).
- Node table without `cell_id_r3`: refused with a regeneration hint, with no fallback (`missing_cell_column_errors_with_hint`).
- `node_id` not dense: `InvalidData`.
- Illegal pinned plan: rejected as a whole, nothing installed.
- Unroutable or late flow units: stranded with a reason and a site (`StrandReason`, `StrandSite` in `src/types.rs`), never dropped silently.
- Known gaps (R3): an unknown `router` name silently selects `time_aware` (constructor in `src/python.rs`); a schedule operation without `at_or_after_atu` panics on `unwrap()` (`apply_schedule_ops`).
- A source cannot be down: the engine reads only the bytes it is given and writes only the manifest.
