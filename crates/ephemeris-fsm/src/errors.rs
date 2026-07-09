//! Error types.

use crate::state::EntityState;
use crate::transition::TransitionReason;
use thiserror::Error;

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum StateMachineError {
    #[error("entity not registered: {0}")]
    EntityNotRegistered(u64),

    #[error("entity already registered: {0}")]
    EntityAlreadyRegistered(u64),

    #[error("illegal transition: {from:?} → reason {reason:?} (no allowed target)")]
    IllegalTransition {
        from: EntityState,
        reason: TransitionReason,
    },

    #[error(
        "stale envelope: entity state changed since proposal (expected {expected:?}, found {actual:?})"
    )]
    StaleEnvelope {
        expected: EntityState,
        actual: EntityState,
    },

    #[error("entity in terminal state: {0:?}")]
    TerminalState(EntityState),

    #[error("missing hints: {0}")]
    MissingHints(&'static str),

    #[error("inconsistent hints: {0}")]
    InconsistentHints(String),
}

pub type Result<T> = std::result::Result<T, StateMachineError>;
