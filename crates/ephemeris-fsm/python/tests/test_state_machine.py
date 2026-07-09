"""Python-side tests for the ephemeris-fsm PyO3 binding.

These tests cover:
  - the binding surface (register/propose/commit/state/history/snapshot)
  - the canonical transition table
  - all 4 FailureMode variants reachable
  - all 4 DeliveryQuality variants reachable
  - stale-envelope detection
  - atomic ledger+state application via `Ledger.apply(op_dict)`
  - PyArrow snapshot interop

The Rust integration suite remains the authoritative invariant
checker; these tests exercise the Python integration end-to-end.
"""

from __future__ import annotations

import json

import pytest

# Skip gracefully on environments where maturin wheels aren't built.
esm = pytest.importorskip("ephemeris_fsm_py")
cl = pytest.importorskip("ephemeris_ledger_py")


# ── helpers ───────────────────────────────────────────────────────────


def planned_route_hints(lane_id: int = 1, bucket: int = 0, units: int = 1) -> str:
    return json.dumps(
        {
            "planned_route": [
                {
                    "kind": "lane",
                    "resource_id": lane_id,
                    "bucket": bucket,
                    "units": units,
                }
            ]
        }
    )


def disc(state):
    """Discriminant name only (drop the optional structured detail)."""
    return state[0] if state is not None else None


# ── basic invariants ──────────────────────────────────────────────────


def test_fresh_machine_is_empty():
    sm = esm.StateMachine()
    assert sm.n_entities() == 0
    assert sm.log_len() == 0
    assert sm.version() == 0
    assert sm.state(1) is None


def test_register_creates_entity_in_created_state():
    sm = esm.StateMachine()
    sm.register(1)
    assert sm.state(1) == ("Created", None)
    assert sm.n_entities() == 1
    assert sm.log_len() == 0  # registration is not a transition


def test_register_is_not_idempotent():
    sm = esm.StateMachine()
    sm.register(1)
    with pytest.raises(ValueError):
        sm.register(1)


def test_unknown_entity_raises():
    sm = esm.StateMachine()
    with pytest.raises(ValueError):
        sm.propose(99, "plan_found", 0, planned_route_hints())


# ── canonical transitions (positive cases) ───────────────────────────


def test_clean_lifecycle_to_on_time_clean():
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    assert disc(sm.state(1)) == "Planned"
    sm.propose_and_commit(1, "enter_node:42", 1)
    assert disc(sm.state(1)) == "Dwelling"
    sm.propose_and_commit(1, "depart_node:42", 2)
    assert disc(sm.state(1)) == "Moving"
    sm.propose_and_commit(1, "arrive_destination:on_time_clean", 3)
    assert sm.state(1) == ("Delivered", "OnTimeClean")


def test_late_clean_delivery():
    sm = esm.StateMachine()
    sm.register(7)
    sm.propose_and_commit(7, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(7, "enter_node:1", 1)
    sm.propose_and_commit(7, "depart_node:1", 2)
    sm.propose_and_commit(7, "arrive_destination:late_clean", 3)
    assert sm.state(7) == ("Delivered", "LateClean")


def test_on_time_violating_delivery():
    sm = esm.StateMachine()
    sm.register(7)
    sm.propose_and_commit(7, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(7, "enter_node:1", 1)
    sm.propose_and_commit(7, "depart_node:1", 2)
    sm.propose_and_commit(7, "arrive_destination:on_time_violating", 3)
    assert sm.state(7) == ("Delivered", "OnTimeViolating")


def test_late_violating_delivery():
    sm = esm.StateMachine()
    sm.register(7)
    sm.propose_and_commit(7, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(7, "enter_node:1", 1)
    sm.propose_and_commit(7, "depart_node:1", 2)
    sm.propose_and_commit(7, "arrive_destination:late_violating", 3)
    assert sm.state(7) == ("Delivered", "LateViolating")


def test_hold_and_release():
    """Hold/Release: Dwelling → Held → Dwelling."""
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(1, "enter_node:1", 1)
    sm.propose_and_commit(1, "hold_by_control", 2)
    assert disc(sm.state(1)) == "Held"
    sm.propose_and_commit(1, "release_hold", 3)
    assert disc(sm.state(1)) == "Dwelling"


def test_defer_and_replan():
    """Defer/Replan: Dwelling → Deferred → Planned."""
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(1, "enter_node:1", 1)
    sm.propose_and_commit(1, "defer:2", 2)
    assert disc(sm.state(1)) == "Deferred"
    sm.propose_and_commit(1, "replan", 3, planned_route_hints())
    assert disc(sm.state(1)) == "Planned"


def test_escalate_and_recover():
    """Escalate/Recover: Dwelling → Escalated → Recovered."""
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(1, "enter_node:1", 1)
    sm.propose_and_commit(1, "escalate", 2)
    assert disc(sm.state(1)) == "Escalated"
    sm.propose_and_commit(1, "recover", 3)
    assert disc(sm.state(1)) == "Recovered"


def test_arrive_intermediate_returns_to_dwelling():
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(1, "enter_node:1", 1)
    sm.propose_and_commit(1, "depart_node:1", 2)
    sm.propose_and_commit(1, "arrive_intermediate:2", 3)
    assert disc(sm.state(1)) == "Dwelling"


# ── all four FailureMode variants reachable ──────────────────────────


def _drive_to(sm, eid, target_disc):
    """Drive `eid` from Created to the requested non-terminal discriminant
    using only legal transitions."""
    sm.propose_and_commit(eid, "plan_found", 0, planned_route_hints())
    if target_disc == "Planned":
        return
    sm.propose_and_commit(eid, "enter_node:1", 1)
    if target_disc == "Dwelling":
        return
    if target_disc == "Held":
        sm.propose_and_commit(eid, "hold_by_control", 2)
        return
    raise ValueError(f"unhandled target {target_disc}")


@pytest.mark.parametrize(
    "from_disc,reason,expected_mode",
    [
        ("Created", "unreachable", "UnreachableTopology"),
        ("Planned", "strand", "Stranded"),
        ("Dwelling", "expire", "Expired"),
        ("Held", "policy_terminate", "PolicyForbid"),
    ],
)
def test_all_failure_modes_reachable(from_disc, reason, expected_mode):
    """Every FailureMode is reachable from at least one non-terminal
    state. The four routes below cover all four modes."""
    sm = esm.StateMachine()
    sm.register(1)
    if from_disc != "Created":
        _drive_to(sm, 1, from_disc)
    bucket = sm.log_len()
    sm.propose_and_commit(1, reason, bucket)
    assert sm.state(1) == ("Failed", expected_mode)


# ── illegal transitions rejected ──────────────────────────────────────


def test_illegal_enter_from_created_rejected():
    sm = esm.StateMachine()
    sm.register(1)
    with pytest.raises(ValueError):
        sm.propose(1, "enter_node:1", 0)


def test_no_transitions_after_terminal():
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(1, "strand", 1)
    with pytest.raises(ValueError):
        sm.propose(1, "plan_found", 2, planned_route_hints())


# ── envelope / stale detection ────────────────────────────────────────


def test_propose_does_not_mutate():
    sm = esm.StateMachine()
    sm.register(1)
    v0 = sm.version()
    env = sm.propose(1, "plan_found", 0, planned_route_hints())
    assert disc(sm.state(1)) == "Created"
    assert sm.version() == v0
    assert env.transition.from_state == ("Created", None)
    assert env.transition.to_state == ("Planned", None)


def test_commit_envelope_advances_state_and_version():
    sm = esm.StateMachine()
    sm.register(1)
    v0 = sm.version()
    env = sm.propose(1, "plan_found", 0, planned_route_hints())
    sm.commit_envelope(env)
    assert disc(sm.state(1)) == "Planned"
    assert sm.version() == v0 + 1
    assert sm.log_len() == 1


def test_stale_envelope_rejected():
    sm = esm.StateMachine()
    sm.register(1)
    env_a = sm.propose(1, "plan_found", 0, planned_route_hints())
    # A second, identical proposal is also valid against current state.
    env_b = sm.propose(1, "plan_found", 0, planned_route_hints())
    sm.commit_envelope(env_a)
    # env_b's transition.from is Created; current state is Planned ⇒ stale.
    with pytest.raises(ValueError):
        sm.commit_envelope(env_b)
    # State and version must not have changed from the rejected commit.
    assert disc(sm.state(1)) == "Planned"


# ── ledger ops & atomic apply ─────────────────────────────────────────


def test_plan_found_emits_reserve_ops():
    sm = esm.StateMachine()
    sm.register(1)
    env = sm.propose(
        1,
        "plan_found",
        0,
        planned_route_hints(lane_id=42, bucket=3, units=2),
    )
    ops = env.ledger_ops_for_pyledger()
    assert len(ops) >= 1
    op = ops[0]
    assert op["type"] == "reserve"
    assert op["kind"] == "lane"
    assert op["resource_id"] == 42
    assert op["bucket"] == 3
    assert op["units"] == 2


def test_terminal_emits_release_ops_for_current_reservations():
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    # Strand with two outstanding reservations declared in hints.
    hints = json.dumps(
        {
            "current_reservations": [[10, "hard"], [11, "soft"]],
        }
    )
    env = sm.propose(1, "strand", 1, hints)
    ops = env.ledger_ops_for_pyledger()
    release_rids = sorted(o["reservation_id"] for o in ops if o["type"] == "release")
    assert release_rids == [10, 11]


def test_hold_by_control_emits_downgrade_ops():
    """B-D1 fix: HoldByControl emits Downgrade(new_level='soft') for each
    currently-Hard reservation. Soft and Forecast reservations are not
    touched. Pre-B-D1 this branch emitted Promote{new_level='soft'} which
    PyLedger.apply rejects because Promote only moves upward."""
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(1, "enter_node:1", 1)
    hints = json.dumps(
        {
            "current_reservations": [[5, "hard"], [6, "soft"]],
        }
    )
    env = sm.propose(1, "hold_by_control", 2, hints)
    ops = env.ledger_ops_for_pyledger()
    downgrades = [o for o in ops if o["type"] == "downgrade"]
    # Only the Hard reservation gets a downgrade op; Soft is left as-is.
    assert len(downgrades) == 1
    assert downgrades[0]["reservation_id"] == 5
    assert downgrades[0]["new_level"] == "soft"
    # No leftover Promote ops from the old buggy code path.
    assert not [o for o in ops if o["type"] == "promote"]


def test_hold_by_control_downgrade_applies_to_pyledger():
    """B-D1 end-to-end: an envelope from HoldByControl applies cleanly to
    a live PyLedger that holds the corresponding Hard reservation. Pre-B-D1
    this raised InvalidPromotion."""
    ledger = cl.Ledger()
    ledger.register_lane(7, capacity_per_bucket=10, n_buckets=24)
    rid = ledger.reserve("lane", 7, 0, 4, "hard", owner=1)
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(1, "enter_node:1", 1)
    hints = json.dumps({"current_reservations": [[rid, "hard"]]})
    env = sm.propose(1, "hold_by_control", 2, hints)
    for op in env.ledger_ops_for_pyledger():
        ledger.apply(op)
    # After downgrade: Hard view sees 10 free; Soft view sees 6 free.
    assert ledger.free_on("lane", 7, 0, "hard") == 10
    assert ledger.free_on("lane", 7, 0, "soft") == 6


def test_atomic_apply_against_pyledger():
    """The envelope's ledger ops must be applicable to PyLedger.apply
    and produce the same effect as a hand-written reservation."""
    ledger = cl.Ledger()
    ledger.register_lane(42, capacity_per_bucket=100, n_buckets=24)
    free_before = ledger.free_on("lane", 42, 0, "hard")
    assert free_before == 100

    sm = esm.StateMachine()
    sm.register(1)
    env = sm.propose(
        1,
        "plan_found",
        0,
        planned_route_hints(lane_id=42, bucket=0, units=7),
    )
    rids = []
    for op in env.ledger_ops_for_pyledger():
        rid = ledger.apply(op)
        if rid is not None:
            rids.append(rid)
    sm.commit_envelope(env)

    # Ledger state advanced by the reservation.
    assert ledger.free_on("lane", 42, 0, "hard") == 100 - 7
    assert len(rids) == 1
    # State machine advanced.
    assert disc(sm.state(1)) == "Planned"


def test_full_lifecycle_against_pyledger_terminal_releases_capacity():
    ledger = cl.Ledger()
    ledger.register_lane(1, 100, 24)
    sm = esm.StateMachine()
    sm.register(1)

    # PlanFound — reserve.
    env = sm.propose(1, "plan_found", 0, planned_route_hints(units=5))
    reserved_rid = None
    for op in env.ledger_ops_for_pyledger():
        rid = ledger.apply(op)
        if rid is not None:
            reserved_rid = rid
    sm.commit_envelope(env)
    assert ledger.free_on("lane", 1, 0, "hard") == 95
    assert reserved_rid is not None

    # Reach Dwelling so Expire is legal.
    sm.propose_and_commit(1, "enter_node:1", 1)

    # Terminal Failed(Expired) — must release the outstanding reservation.
    hints = json.dumps({"current_reservations": [[reserved_rid, "hard"]]})
    env_t = sm.propose(1, "expire", 2, hints)
    for op in env_t.ledger_ops_for_pyledger():
        ledger.apply(op)
    sm.commit_envelope(env_t)

    assert ledger.free_on("lane", 1, 0, "hard") == 100
    assert sm.state(1) == ("Failed", "Expired")


# ── history / introspection ───────────────────────────────────────────


def test_history_chain_invariant():
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(1, "enter_node:1", 1)
    sm.propose_and_commit(1, "hold_by_control", 2)
    sm.propose_and_commit(1, "release_hold", 3)

    hist = sm.history(1)
    assert len(hist) == 4
    # history[i].from == history[i-1].to
    for i in range(1, len(hist)):
        assert hist[i].from_state == hist[i - 1].to_state


def test_entities_in_state_filter():
    sm = esm.StateMachine()
    for eid in range(10):
        sm.register(eid)
    for eid in range(5):
        sm.propose_and_commit(eid, "plan_found", 0, planned_route_hints())
    planned = sm.entities_in_state("Planned")
    created = sm.entities_in_state("Created")
    assert sorted(planned) == [0, 1, 2, 3, 4]
    assert sorted(created) == [5, 6, 7, 8, 9]


# ── pyarrow snapshot ──────────────────────────────────────────────────


def test_snapshot_log_returns_pyarrow_recordbatch():
    """The snapshot is an Arrow RecordBatch. Discriminant columns are
    `UInt8` (the enum-as-u8 index, see snapshot.rs); detail columns are
    nullable Utf8 carrying the structured variant name on terminals."""
    pa = pytest.importorskip("pyarrow")
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(1, "strand", 1)

    batch = sm.snapshot_log()
    assert isinstance(batch, pa.RecordBatch)
    assert batch.num_rows == 2
    cols = {f.name for f in batch.schema}
    expected = {
        "entity_id",
        "from_discriminant",
        "from_detail",
        "to_discriminant",
        "to_detail",
        "reason",
        "at_bucket",
        "at_version",
    }
    assert expected.issubset(cols)
    # Discriminant columns are u8. The Failed variant is enum-index 9
    # (Created=0..Failed=9 per snapshot.rs::discriminant_u8).
    last_to_disc = batch.column("to_discriminant")[1].as_py()
    last_to_detail = batch.column("to_detail")[1].as_py()
    assert last_to_disc == 9  # Failed
    assert last_to_detail == "Stranded"
    # First row: Created→Planned, no detail on either side.
    assert batch.column("from_discriminant")[0].as_py() == 0  # Created
    assert batch.column("from_detail")[0].as_py() is None
    assert batch.column("to_discriminant")[0].as_py() == 1  # Planned
    assert batch.column("to_detail")[0].as_py() is None


# ── PreserveDeliverable variant ──────────────────────────────────────


def test_preserve_deliverable_string_parses():
    """PreserveDeliverable round-trips through the PyO3 wire format
    `preserve_deliverable`."""
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(1, "enter_node:1", 1)
    # If from_str doesn't accept the string, propose_and_commit raises.
    env = sm.propose_and_commit(1, "preserve_deliverable", 2)
    assert env.transition.reason == "preserve_deliverable"


def test_preserve_deliverable_self_loop_dwelling_via_python():
    """Dwelling → Dwelling self-loop; state is unchanged but log grows."""
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(1, "enter_node:1", 1)
    assert disc(sm.state(1)) == "Dwelling"
    log_before = sm.log_len()
    sm.propose_and_commit(1, "preserve_deliverable", 2)
    assert disc(sm.state(1)) == "Dwelling"  # unchanged
    assert sm.log_len() == log_before + 1


def test_preserve_deliverable_in_created_via_python():
    """Admission case: Created → Created self-loop."""
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "preserve_deliverable", 0)
    assert disc(sm.state(1)) == "Created"
    hist = sm.history(1)
    assert hist[-1].reason == "preserve_deliverable"


def test_preserve_deliverable_illegal_from_moving_via_python():
    """Moving is not a legal source state for PreserveDeliverable."""
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(1, "enter_node:1", 1)
    sm.propose_and_commit(1, "depart_node:1", 2)
    assert disc(sm.state(1)) == "Moving"
    with pytest.raises(ValueError, match="IllegalTransition|illegal"):
        sm.propose_and_commit(1, "preserve_deliverable", 3)


def test_preserve_deliverable_no_ledger_ops_via_python():
    """PreserveDeliverable emits zero ledger ops."""
    sm = esm.StateMachine()
    sm.register(1)
    sm.propose_and_commit(1, "plan_found", 0, planned_route_hints())
    sm.propose_and_commit(1, "enter_node:1", 1)
    env = sm.propose(1, "preserve_deliverable", 2)
    ops = env.ledger_ops_for_pyledger()
    assert ops == []
