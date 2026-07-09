//! Reservation type and commitment levels.
//!
//! Commitment levels form a strict total order:
//! `Forecast < Soft < Hard`. The numeric values encode this; queries
//! at level `L` include all reservations at levels `>= L`.

use crate::resource::{ResourceId, ResourceKind, TimeBucket};
use serde::{Deserialize, Serialize};

pub type ReservationId = u64;
pub type OwnerId = u64;
pub type LedgerVersion = u64;

/// Commitment levels, ordered.
///
/// `Hard` reservations must deliver; `Soft` are tentative; `Forecast`
/// are predictions with no admissibility check.
///
/// Numeric values are part of the public API (used in Arrow snapshots
/// and Python bindings). Do not renumber.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[repr(u8)]
pub enum CommitmentLevel {
    /// Predicted only; never blocks admission.
    Forecast = 0,
    /// Tentative reservation. Counts against capacity.
    Soft = 1,
    /// Booked. Must deliver.
    Hard = 2,
}

impl CommitmentLevel {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(CommitmentLevel::Forecast),
            1 => Some(CommitmentLevel::Soft),
            2 => Some(CommitmentLevel::Hard),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CommitmentLevel::Forecast => "forecast",
            CommitmentLevel::Soft => "soft",
            CommitmentLevel::Hard => "hard",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "forecast" => Some(CommitmentLevel::Forecast),
            "soft" => Some(CommitmentLevel::Soft),
            "hard" => Some(CommitmentLevel::Hard),
            _ => None,
        }
    }

    /// Returns true if this level should be counted when querying at
    /// the given query level. Query at `Hard` counts Hard only; query
    /// at `Soft` counts Hard+Soft; query at `Forecast` counts all three.
    #[inline]
    pub fn counts_at(self, query: CommitmentLevel) -> bool {
        self >= query
    }
}

/// A single reservation. Fully self-describing for snapshot purposes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub id: ReservationId,
    pub resource_kind: ResourceKind,
    pub resource_id: ResourceId,
    pub bucket: TimeBucket,
    pub units: u64,
    pub level: CommitmentLevel,
    pub owner: OwnerId,
    pub created_at_version: LedgerVersion,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commitment_ordering() {
        use CommitmentLevel::*;
        assert!(Forecast < Soft);
        assert!(Soft < Hard);
        assert!(Forecast < Hard);
    }

    #[test]
    fn counts_at_hard_only_hard() {
        use CommitmentLevel::*;
        assert!(Hard.counts_at(Hard));
        assert!(!Soft.counts_at(Hard));
        assert!(!Forecast.counts_at(Hard));
    }

    #[test]
    fn counts_at_soft_includes_hard() {
        use CommitmentLevel::*;
        assert!(Hard.counts_at(Soft));
        assert!(Soft.counts_at(Soft));
        assert!(!Forecast.counts_at(Soft));
    }

    #[test]
    fn counts_at_forecast_includes_all() {
        use CommitmentLevel::*;
        assert!(Hard.counts_at(Forecast));
        assert!(Soft.counts_at(Forecast));
        assert!(Forecast.counts_at(Forecast));
    }

    #[test]
    fn level_roundtrip_u8() {
        for l in [
            CommitmentLevel::Forecast,
            CommitmentLevel::Soft,
            CommitmentLevel::Hard,
        ] {
            assert_eq!(CommitmentLevel::from_u8(l.as_u8()), Some(l));
        }
    }

    #[test]
    fn level_roundtrip_str() {
        for l in [
            CommitmentLevel::Forecast,
            CommitmentLevel::Soft,
            CommitmentLevel::Hard,
        ] {
            assert_eq!(CommitmentLevel::from_str(l.as_str()), Some(l));
        }
        assert_eq!(CommitmentLevel::from_str("HARD"), Some(CommitmentLevel::Hard));
    }
}
