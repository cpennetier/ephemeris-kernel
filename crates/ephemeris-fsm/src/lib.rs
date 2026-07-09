//! Ephemeris Kernel entity state machine.

pub mod entity;
pub mod envelope;
pub mod errors;
pub mod machine;
pub mod state;
pub mod transition;

#[cfg(feature = "arrow")]
pub mod snapshot;

#[cfg(feature = "pyo3")]
mod python;

pub use entity::{Entity, EntityId, EntityKind, FlowUnitEntity};
pub use envelope::{
    LedgerCommitmentLevel, LedgerOp, LedgerOpHints, LedgerResourceKind, RouteSegment,
    TransitionEnvelope,
};
pub use errors::{Result as SmResult, StateMachineError};
pub use machine::StateMachine;
pub use state::{
    DeliveryQuality, EntityState, FailureMode, StateDiscriminant,
};
pub use transition::{NodeId, LedgerVersion, Transition, TransitionReason};

#[cfg(feature = "pyo3")]
use pyo3::prelude::*;

#[cfg(feature = "pyo3")]
#[pymodule]
fn ephemeris_fsm_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<python::PyStateMachine>()?;
    m.add_class::<python::PyTransitionEnvelope>()?;
    m.add_class::<python::PyTransition>()?;
    Ok(())
}
