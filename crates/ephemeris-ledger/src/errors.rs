//! Error types for the capacity ledger.

use crate::reservation::CommitmentLevel;
use crate::resource::ResourceKind;
use thiserror::Error;

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum LedgerError {
    #[error("resource not registered: {kind:?} #{id}")]
    ResourceNotRegistered { kind: ResourceKind, id: u64 },

    #[error(
        "insufficient capacity on {kind:?} #{id} at bucket {bucket}: \
         requested {requested}, free at Soft level {free}"
    )]
    InsufficientCapacity {
        kind: ResourceKind,
        id: u64,
        bucket: u32,
        requested: u64,
        free: u64,
    },

    #[error("reservation not found: {0}")]
    ReservationNotFound(u64),

    #[error("invalid promotion: from {from:?} to {to:?} not allowed (must be strict increase)")]
    InvalidPromotion {
        from: CommitmentLevel,
        to: CommitmentLevel,
    },

    #[error("invalid downgrade: from {from:?} to {to:?} not allowed (must be strict decrease; use Promote for upward moves)")]
    InvalidDowngrade {
        from: CommitmentLevel,
        to: CommitmentLevel,
    },

    #[error("zero-unit reservation is not allowed")]
    ZeroUnits,
}

pub type LedgerResult<T> = std::result::Result<T, LedgerError>;
