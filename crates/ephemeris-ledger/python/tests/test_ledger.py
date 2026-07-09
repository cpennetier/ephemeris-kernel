"""Python-side tests for the ephemeris-ledger PyO3 binding.

Mirrors the most important Rust invariants from
``services/ephemeris-ledger/tests/integration.rs``. The full invariant
suite is in Rust (lower-level); these tests exercise the binding
surface end-to-end.
"""

from __future__ import annotations

import pytest

# Skip gracefully on environments where the maturin wheel isn't built
# (e.g. CI without the maturin-develop step, or contributor working
# without the Rust toolchain). Local dev: install via
# `cd services/ephemeris-ledger && maturin develop --release`.
cl = pytest.importorskip("ephemeris_ledger_py")


def make_ledger_with_lane(capacity: int = 100, n_buckets: int = 24):
    l = cl.Ledger()
    l.register_lane(1, capacity, n_buckets)
    return l


# ── Basic invariants ──────────────────────────────────────────────────


def test_empty_ledger_returns_zero():
    l = cl.Ledger()
    assert l.total_capacity("lane", 1, 0) == 0
    assert l.version() == 0
    assert l.n_reservations() == 0


def test_register_and_query():
    l = make_ledger_with_lane(100, 24)
    assert l.total_capacity("lane", 1, 0) == 100
    assert l.free_on("lane", 1, 0, "hard") == 100


def test_reserve_and_release():
    l = make_ledger_with_lane(100, 24)
    rid = l.reserve("lane", 1, 0, 10, "hard", 99)
    assert isinstance(rid, int)
    assert l.free_on("lane", 1, 0, "hard") == 90
    l.release(rid)
    assert l.free_on("lane", 1, 0, "hard") == 100


def test_release_is_idempotent():
    l = make_ledger_with_lane(100, 24)
    rid = l.reserve("lane", 1, 0, 10, "hard", 99)
    l.release(rid)
    l.release(rid)  # no-op, no error


def test_version_increments_on_writes():
    l = make_ledger_with_lane(100, 24)
    v0 = l.version()
    l.reserve("lane", 1, 0, 5, "hard", 1)
    v1 = l.version()
    assert v1 > v0


# ── Commitment-level semantics ────────────────────────────────────────


def test_lattice_property():
    l = make_ledger_with_lane(100, 24)
    l.reserve("lane", 1, 0, 10, "hard", 1)
    l.reserve("lane", 1, 0, 20, "soft", 2)
    l.reserve("lane", 1, 0, 30, "forecast", 3)
    f_hard = l.free_on("lane", 1, 0, "hard")
    f_soft = l.free_on("lane", 1, 0, "soft")
    f_fcst = l.free_on("lane", 1, 0, "forecast")
    assert f_hard == 90
    assert f_soft == 70
    assert f_fcst == 40


def test_promote_forecast_to_soft():
    l = make_ledger_with_lane(100, 24)
    rid = l.reserve("lane", 1, 0, 5, "forecast", 1)
    assert l.free_on("lane", 1, 0, "soft") == 100
    l.promote(rid, "soft")
    assert l.free_on("lane", 1, 0, "soft") == 95


def test_node_and_lane_independent():
    l = cl.Ledger()
    l.register_lane(1, 100, 24)
    l.register_node(1, 50, 24)
    l.reserve("lane", 1, 0, 30, "hard", 1)
    l.reserve("node", 1, 0, 20, "hard", 1)
    assert l.free_on("lane", 1, 0, "hard") == 70
    assert l.free_on("node", 1, 0, "hard") == 30


# ── Error cases ────────────────────────────────────────────────────────


def test_zero_units_raises():
    l = make_ledger_with_lane(100, 24)
    with pytest.raises(ValueError, match="zero-unit"):
        l.reserve("lane", 1, 0, 0, "hard", 1)


def test_insufficient_capacity_raises():
    l = make_ledger_with_lane(10, 24)
    l.reserve("lane", 1, 0, 7, "hard", 1)
    with pytest.raises(ValueError, match="insufficient capacity"):
        l.reserve("lane", 1, 0, 5, "hard", 2)


def test_unregistered_resource_raises():
    l = cl.Ledger()
    with pytest.raises(ValueError, match="not registered"):
        l.reserve("lane", 999, 0, 1, "hard", 1)


def test_invalid_kind_string_raises():
    l = make_ledger_with_lane(100, 24)
    with pytest.raises(ValueError, match="invalid kind"):
        l.reserve("nope", 1, 0, 1, "hard", 1)


def test_invalid_level_string_raises():
    l = make_ledger_with_lane(100, 24)
    with pytest.raises(ValueError, match="invalid level"):
        l.reserve("lane", 1, 0, 1, "bogus", 1)


# ── Snapshot ──────────────────────────────────────────────────────────


def test_empty_snapshot_has_correct_schema():
    l = cl.Ledger()
    batch = l.snapshot()
    assert batch.num_rows == 0
    assert batch.schema.names == [
        "reservation_id",
        "resource_kind",
        "resource_id",
        "bucket",
        "units",
        "level",
        "owner",
        "created_at_version",
    ]


def test_populated_snapshot_has_rows():
    l = make_ledger_with_lane(100, 24)
    l.reserve("lane", 1, 0, 10, "hard", 99)
    l.reserve("lane", 1, 0, 5, "soft", 99)
    batch = l.snapshot()
    assert batch.num_rows == 2
    # Reservation IDs are 0 then 1.
    ids = batch.column("reservation_id").to_pylist()
    assert ids == [0, 1]
    units = batch.column("units").to_pylist()
    assert units == [10, 5]
    levels = batch.column("level").to_pylist()
    # 0=Forecast, 1=Soft, 2=Hard
    assert levels == [2, 1]


def test_reservations_for_returns_typed_objects():
    l = make_ledger_with_lane(100, 24)
    rid = l.reserve("lane", 1, 0, 10, "hard", 99)
    res = l.reservations_for("lane", 1, 0, "hard")
    assert len(res) == 1
    r = res[0]
    assert r.id == rid
    assert r.kind == "lane"
    assert r.resource_id == 1
    assert r.bucket == 0
    assert r.units == 10
    assert r.level == "hard"
    assert r.owner == 99
    assert r.created_at_version > 0
