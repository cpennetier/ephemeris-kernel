# Ephemeris Kernel

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**A deterministic time-and-truth runtime for discrete-event systems.**

> **Research program — verifiable learning and decision systems.**  
> Proposal mechanisms may be heuristic or learned; the structures they produce, their execution, and their value remain independently measurable.
>
> **Role of this repository:** deterministic execution and causal ground truth.  
> [PatternBoost](https://github.com/cpennetier/spectral-graph-patternboost) ·
> [Graph diffusion](https://github.com/cpennetier/spectral-graph-diffusion) ·
> [Ephemeris Kernel](https://github.com/cpennetier/ephemeris-kernel) ·
> [Epure Arena](https://github.com/cpennetier/epure-arena)

## The question

At time \(t\):

- what is the state of the world;
- which ordered events produced it;
- what capacity was reserved or released;
- and can the complete account be replayed exactly?

Ephemeris Kernel answers that question with a small deterministic runtime rather than with inferred traces.

## Why it exists

A policy can be compared with another policy only if the environment beneath them is stable.

Ephemeris therefore keeps **execution** separate from **decision-making**:

```text
external policy or planner
          │
          │ decisions through public hooks
          ▼
┌─────────────────────────────────────────────┐
│              EPHEMERIS KERNEL               │
│                                             │
│  integer clock                              │
│  total event order                          │
│  append-only transition history             │
│  capacity reservations with versioning      │
│  deterministic snapshots and run manifests │
└─────────────────────────────────────────────┘
          │
          ▼
exact event stream, state, surfaces, and audit record
```

The kernel contains **no optimizer and no policy**. It executes imposed decisions, records their consequences, and replays the same run under the same inputs.

## Core contract

Ephemeris provides:

- an integer-time event queue;
- a total event order \((\mathrm{time},\mathrm{event\_id})\);
- deterministic replay;
- append-only state and transition ledgers;
- byte-identical ordered snapshots on a given platform;
- capacity reservation, promotion, and release with monotonic versioning;
- run manifests containing the seed, schema fingerprint, and consumer-supplied configuration hash;
- Arrow surfaces for read-only observation.

Its responsibility ends at factual execution:

> **What happened is owned by the kernel. What should happen is owned by the consumer.**

## Components

| Rust crate | Python module | Responsibility |
|---|---|---|
| `crates/ephemeris-des` | `ephemeris._engine` | Integer-time DES engine, total-ordered event queue, capacity dynamics, Arrow ingestion and surfaces, KPIs, and run manifests |
| `crates/ephemeris-fsm` | `ephemeris_fsm_py` | Entity state machine with propose/commit transitions, validity checks, append-only history, and deterministic Arrow snapshots |
| `crates/ephemeris-ledger` | `ephemeris_ledger_py` | Capacity ledger with reserve/promote/release operations, monotonic versions, and ordered snapshots |

The crates can be used together through `ephemeris-kernel` or installed independently when only the ledger or FSM is required.

## Install

A Rust toolchain is required. `maturin` is resolved by the build configuration.

```bash
pip install \
  "ephemeris-kernel @ git+https://github.com/cpennetier/ephemeris-kernel@v0.1.0"
```

Independent ledger and FSM packages:

```bash
pip install \
  "ephemeris-ledger @ git+https://github.com/cpennetier/ephemeris-kernel@v0.1.0#subdirectory=crates/ephemeris-ledger"

pip install \
  "ephemeris-fsm @ git+https://github.com/cpennetier/ephemeris-kernel@v0.1.0#subdirectory=crates/ephemeris-fsm"
```

## Verify the artifact

```bash
git clone https://github.com/cpennetier/ephemeris-kernel.git
cd ephemeris-kernel

make test-rust
make develop
make guard
```

| Command | Purpose |
|---|---|
| `make test-rust` | Run the full Rust crate suites without Python linkage |
| `make develop` | Build and install the release-mode Python extension into the active environment |
| `make guard` | Check the kernel-neutrality invariant |

## Public extension points

The kernel calls the hooks; consumers define their meaning.

### Decision imposition

- `set_forced_routes[_pinned]`
- `apply_schedule_ops`
- `inject_demand`
- `set_lane_outages`
- `set_node_outages`

These APIs impose decisions or exogenous changes. They do not select them.

### Routing strategy

The `Router` trait provides the routing extension seam. The current implementations include `static` and `time_aware`, selected at engine construction.

### Observation

Read-only outputs include:

- `drain_surfaces_arrow`;
- `get_kpis`;
- strand, delivery, and hop records;
- `oracle_verdicts`;
- `slot_state`.

### Audit and replay

Consumers may attach a configuration hash to the run manifest. The kernel records the hash without interpreting it.

## Determinism contract

For identical Arrow inputs, seed, and configuration, Ephemeris produces byte-identical event streams, ordered snapshots, and manifests on a given platform.

The total event order is

```text
(time_atu ASC, event_id ASC)
```

over an integer clock.

There is:

- no floating-point time;
- no ambiguous tie breaking;
- no dependence on dictionary iteration order for event ordering;
- no policy-dependent mutation of the kernel's semantics.

Determinism is a correctness property, not a best-effort debugging mode.

## Neutral extension rule

When a consumer needs a capability that the kernel does not expose, the correct change is a new **neutral extension point** added under semantic versioning.

A consumer-specific decision rule does not belong in the kernel.

The neutrality guard is enforced by `tests/test_no_domain_vocabulary.py`.

## Non-goals

Ephemeris Kernel is not:

- an optimizer;
- a policy framework;
- an end-to-end application;
- a learned simulator;
- a distributed event-processing platform;
- a claim of cross-platform byte identity without qualification.

It is a deliberately narrow trusted runtime for deterministic discrete-event execution.

## Relationship to Epure Arena

[`epure-arena`](https://github.com/cpennetier/epure-arena) builds certification and policy evaluation above Ephemeris:

```text
policy / planner
      │
      ▼
Epure certifier and evaluation harness
      │
      ▼
Ephemeris deterministic execution
```

The dependency arrow points downward. Ephemeris does not import Epure.

## Repository map

```text
crates/
  ephemeris-des/       discrete-event engine
  ephemeris-fsm/       deterministic entity state machine
  ephemeris-ledger/    versioned capacity ledger
python/ephemeris/      Python package surface
tests/                 neutrality and consumer-firewall guards
Cargo.toml             Rust workspace
pyproject.toml         Python build configuration
Makefile               build, test, and guard targets
```

## License

Apache-2.0 — see [LICENSE](LICENSE).
