# Ephemeris Kernel

**A deterministic time-and-truth runtime for discrete-event systems.**

Ephemeris Kernel answers one question, exactly and reproducibly: *at time t,
what is the state of the world, what events caused it, and can you prove it?*

It provides an integer-time event queue with a total event order,
deterministic replay, append-only state and transition ledgers with
byte-identical snapshots, capacity reservation with monotonic versioning, and
cryptographic run manifests (seed, schema fingerprint, config hash) for every
run.

It contains **no optimizer, no policy, and no domain.** Decision-making
systems built on top impose their choices through the kernel's public hooks;
the kernel executes, records, and replays them. Whatever your entities flow
through — compute jobs through a cluster, packets through a network, cargo
units through a network of lanes, power through a grid — the kernel gives you
the same guarantee: **a certified account of what happened.**

## Components

| Crate | Python module | Provides |
|---|---|---|
| `crates/ephemeris-des` | `ephemeris._engine` | The discrete-event engine: integer-time clock, min-heap event queue with total order `(time, event_id)`, capacitated-network dynamics, Arrow IPC ingestion, surface/KPI emission, run manifests. |
| `crates/ephemeris-fsm` | `ephemeris_fsm_py` | Entity state machine: propose/commit transitions against a validity table, append-only per-entity history, deterministic Arrow snapshots. |
| `crates/ephemeris-ledger` | `ephemeris_ledger_py` | Capacity ledger: reserve/promote/release with monotonic versioning and deterministic ordered snapshots. |

## Install

```bash
# needs a Rust toolchain (cargo); maturin is fetched automatically
pip install "ephemeris-kernel @ git+https://github.com/cpennetier/ephemeris-kernel@v0.1.0"
```

The ledger and FSM are also independently installable:

```bash
pip install "ephemeris-ledger @ git+https://github.com/cpennetier/ephemeris-kernel@v0.1.0#subdirectory=crates/ephemeris-ledger"
pip install "ephemeris-fsm @ git+https://github.com/cpennetier/ephemeris-kernel@v0.1.0#subdirectory=crates/ephemeris-fsm"
```

## Extension points (v0.1)

The kernel calls the hooks; consumers define their meaning.

- **Decision imposition** — `set_forced_routes[_pinned]`, `apply_schedule_ops`,
  `inject_demand`, `set_lane_outages` / `set_node_outages`. The kernel executes
  decisions; it never makes them.
- **Routing strategy** — the `Router` trait (`static`, `time_aware`), selected
  at engine construction.
- **Observation** — `drain_surfaces_arrow`, `get_kpis`, strand/delivery/hop
  records, `oracle_verdicts`, `slot_state`: read-only Arrow views for
  consumer-side logic.
- **Audit & replay** — run manifests carry a consumer-supplied config hash the
  kernel never interprets; snapshots are byte-identical under identical
  inputs.

**The extension rule:** if a consumer needs a capability the kernel does not
expose, the fix is a new *neutral* extension point added here under semver —
never a consumer-specific change to the kernel. Neutrality is enforced in CI
(`tests/test_no_domain_vocabulary.py`): no industry vocabulary, no reference
to any downstream consumer.

## Development

```bash
make test-rust    # full crate suites (no Python linkage needed)
make develop      # maturin develop --release into the active venv
make guard        # neutrality invariant
```

## Determinism contract

Identical inputs (Arrow tables, seed, config) produce byte-identical event
streams, snapshots, and manifests on a given platform. The total event order
is `(time_atu ASC, event_id ASC)` over an integer clock — no float time, no
tie-breaking ambiguity.
