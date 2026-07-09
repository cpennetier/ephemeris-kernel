//! Transition envelopes and ledger ops.
//!
//! The state machine returns a `TransitionEnvelope` from `propose`.
//! The envelope contains the transition plus the ledger ops the caller
//! should apply to its `PyLedger` before `commit_envelope`.
//!
//! The ledger op enum mirrors ephemeris-ledger's surface but is owned
//! here as plain data — the state machine has no runtime ledger
//! dependency.

use crate::transition::Transition;
use serde::{Deserialize, Serialize};

/// Resource kind enum, mirroring ephemeris-ledger's `ResourceKind`.
/// Duplicated here as plain data so the state machine doesn't depend
/// on the ledger crate at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LedgerResourceKind {
    Node,
    Lane,
}

impl LedgerResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LedgerResourceKind::Node => "node",
            LedgerResourceKind::Lane => "lane",
        }
    }
}

/// Commitment level mirror, same shape as ephemeris-ledger's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LedgerCommitmentLevel {
    Forecast,
    Soft,
    Hard,
}

impl LedgerCommitmentLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            LedgerCommitmentLevel::Forecast => "forecast",
            LedgerCommitmentLevel::Soft => "soft",
            LedgerCommitmentLevel::Hard => "hard",
        }
    }
}

/// One ledger op. Caller maps these to its `PyLedger` calls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LedgerOp {
    Reserve {
        kind: LedgerResourceKind,
        resource_id: u64,
        bucket: u32,
        units: u64,
        level: LedgerCommitmentLevel,
        owner: u64,
    },
    Release {
        reservation_id: u64,
    },
    Promote {
        reservation_id: u64,
        new_level: LedgerCommitmentLevel,
    },
    /// Downgrade an existing reservation to a strictly lower commitment
    /// level (Hard -> Soft, Hard -> Forecast, Soft -> Forecast). Mirrors
    /// `ephemeris_ledger_py.Ledger::downgrade`. Used by HoldByControl in
    /// `machine.rs` to release Hard reservations back to Soft without
    /// re-running admissibility (added in B-D1 cleanup).
    Downgrade {
        reservation_id: u64,
        new_level: LedgerCommitmentLevel,
    },
}

/// Hints carried into `propose` so the state machine can build the
/// envelope without reading the ledger directly.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LedgerOpHints {
    /// Existing reservations the entity holds, with their current level.
    pub current_reservations: Vec<(u64, LedgerCommitmentLevel)>,
    /// Planned route segments (used when transitioning Created→Planned).
    pub planned_route: Vec<RouteSegment>,
    /// The node the entity is currently at (or about to enter).
    pub current_node: Option<u64>,
    /// Per-flow unit deadline expressed as a bucket index.
    pub deadline_bucket: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteSegment {
    pub kind: LedgerResourceKind,
    pub resource_id: u64,
    pub bucket: u32,
    pub units: u64,
}

/// The output of `propose`. The caller applies `ledger_ops` to its
/// ledger, then calls `commit_envelope(envelope)`.
#[derive(Clone, Debug)]
pub struct TransitionEnvelope {
    pub transition: Transition,
    pub ledger_ops: Vec<LedgerOp>,
}
