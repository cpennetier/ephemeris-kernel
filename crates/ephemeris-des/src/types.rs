//! Sacred Contract type aliases and constants.
//!
//! All time is ATU (1 ATU = 1 millisecond). Sacred Contract G-1.
//! All IDs are dense indices. Sacred Contract G-3.

/// Absolute Time Unit — i64 milliseconds. Sacred Contract G-1.
pub type Atu = i64;

/// Node identifier. Dense [0..N). Sacred Contract G-3.
pub type NodeId = u32;

/// Lane index into CSR. Dense [0..E). Sacred Contract G-3.
pub type LaneIdx = u32;

/// FlowUnit identifier. Sacred Contract G-3.
pub type FlowUnitId = u64;

/// Abstract capacity scalar. Sacred Contract G-2.
pub type CapacityUnits = u32;

/// Cost in microdollars (1 USD = 1_000_000 microdollars).
pub type CostMicros = i64;

/// CO2 in grams.
pub type Co2Grams = i64;

/// Priority class. 0=VIP, 1=Express, 2=Standard, 3=Economy.
pub type PriorityClass = u8;

/// Reason a flow unit failed to deliver. Surfaced in the run-end KPIs
/// (`strand_reasons`), JSON only — see reconciliation in `kpi.rs::finalize`.
///
/// `TopologyUnreachable` — no directed path origin→dest exists in the static
///   graph (checked at inject, ignoring time/capacity).
/// `ScheduleInfeasible` — a path exists but no usable departure slot did (the
///   router found no hop, or a queued flow unit ran out of future slots, with no
///   capacity refusal in its history).
/// `CapacityExhausted` — the flow unit's terminal failure followed a capacity
///   refusal (a full slot); congestion, not topology or schedule.
/// `DeadlineMissed` — the horizon ended with the flow unit undelivered AND its due
///   time already passed. (A flow unit still in flight whose deadline has NOT
///   passed is `in_flight_at_cutoff` — censored, not stranded — and is NOT a
///   `StrandReason`.)
/// `LaneOutage` — RESERVED. Lane outages currently defer (re-queue) rather than
///   strand, so this is never set; kept for a future terminal-outage path.
/// `NodeOutage` — a flow unit arrived at a node inside a §E shock window.
///
/// Note: `strand_reasons` is JSON-only. The `SurfaceBlock` Arrow schema
/// reserves NO per-variant column (see `surface_writer.rs` "Out of scope"), so
/// adding/removing variants is NOT an Arrow/fingerprint migration.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StrandReason {
    TopologyUnreachable = 0,
    ScheduleInfeasible = 1,
    CapacityExhausted = 2,
    DeadlineMissed = 3,
    LaneOutage = 4,
    NodeOutage = 5,
}

/// Number of `StrandReason` variants.
pub const STRAND_REASON_COUNT: usize = 6;

/// WHERE in the engine a strand was decided (observability only — feeds the
/// per-flow unit `StrandRecord.site`; never read by routing/strand logic).
///
/// `RouterNone` — `try_board`: the router found no path from the current node
///   (`engine.rs` "dynamic dead-end" arm).
/// `ChosenLaneNoSlot` — `try_board`: the router chose a first-hop lane, but that
///   lane has no departure at/after now (the stale-hour-bucket defect surface).
/// `QueueDeath` — `strand_lane_queue`: an lane overflow queue died because the
///   lane has no future slot; every queued flow unit strands here.
/// `TopologyPrecheck` — `handle_inject`: static-reachability pre-check failed.
/// `NodeOutageArrival` — `handle_arrive`: arrival into a §E node-outage window.
/// `DeadlineFinalize` — `finalize_strands`: end-of-run sweep, due already past.
/// `MissedPinnedSlot` — slot-pinned execution safety net: a pinned flow unit
///   could not board its reserved slot (arrived late, slot missing, or
///   capacity violated). With a capacity-legal plan this count is ZERO by
///   construction — any non-zero value is a bug detector, not an outcome.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StrandSite {
    RouterNone = 0,
    ChosenLaneNoSlot = 1,
    QueueDeath = 2,
    TopologyPrecheck = 3,
    NodeOutageArrival = 4,
    DeadlineFinalize = 5,
    MissedPinnedSlot = 6,
}

// ATU constants
pub const ATU_PER_SECOND: Atu = 1_000;
pub const ATU_PER_MINUTE: Atu = 60_000;
pub const ATU_PER_HOUR: Atu = 3_600_000;
pub const ATU_PER_DAY: Atu = 86_400_000;
