# Roadmap

Each item says what it brings and where its result goes. A finished result waits under Results not yet used until it is used.

## Now

- R1: Two identical runs are proven byte-identical. Brings: a test that runs one scenario twice and compares the surface stream and manifest bytes. Goes to: the determinism contract in the README.
- R2: The installed Python engine is tested end to end. Brings: a test that builds `ephemeris._engine`, runs a small scenario from Arrow tables and checks surfaces, KPIs and the manifest. Goes to: the "Verify the artifact" section of the README.
- R3: Malformed engine calls raise clear Python errors. Brings: an unknown router name and a schedule operation without `at_or_after_atu` raise `ValueError` instead of a silent default or a panic. Goes to: the public Python API listed in the README.

## Next

- R4: The capacity ledger keeps a record of every release. Brings: released reservations stay on record, so reserved and released capacity can be replayed. Goes to: the README question on what capacity was reserved or released.
- R5: The README says exactly what each package installs. Brings: install text that matches `pyproject.toml`: the root package ships the engine only, the FSM and ledger install separately. Goes to: the README on the public repository.
- R6: Code comments stand on this repository alone. Brings: comments that cite code outside the repository, such as a TypeScript reader or external paths, rewritten or removed. Goes to: the public source tree.
- R7: Release builds use the intended optimization profile. Brings: `lto` and `codegen-units` moved to the workspace root, where Cargo applies them. Goes to: the wheels built by `make develop` and `make build`.

## Results not yet used

## Parked

- R8: The H3 equivalence check runs where its pipeline lives. Brings: `tests/h3_equivalence.rs` either joins `make test` or moves to the offline pipeline it protects. Goes to: to be decided by the owner.
- R9: Runs can be partitioned across threads. Brings: the parallel executor for which the manifest reserves `partition_id`, `partition_hash` and `thread_count`. Goes to: to be decided by the owner.
- R10: Runs are stored as Parquet. Brings: the run store that `run_manifest.rs` names as future work. Goes to: to be decided by the owner.
- R11: Surfaces report failures per bucket. Brings: a `failed_by_reason` column, out of scope in `surface_writer.rs` until the engine tracks strands per bucket. Goes to: to be decided by the owner.

## Done
