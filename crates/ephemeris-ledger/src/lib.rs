//! Ephemeris Kernel capacity ledger.
//!
//! Public Rust API (re-exports from submodules) is the surface other
//! Rust crates (e.g., `ephemeris_des`) consume. The PyO3 binding wraps
//! these for Python consumers.

pub mod errors;
pub mod ledger;
pub mod reservation;
pub mod resource;

#[cfg(feature = "arrow")]
pub mod snapshot;

#[cfg(feature = "pyo3")]
mod python;

pub use errors::{LedgerError, LedgerResult};
pub use ledger::{Ledger, ResourceKey};
pub use reservation::{
    CommitmentLevel, LedgerVersion, OwnerId, Reservation, ReservationId,
};
pub use resource::{
    LaneResource, NodeResource, Resource, ResourceId as TypedResourceId, ResourceKind,
    TimeBucket,
};

#[cfg(feature = "pyo3")]
use pyo3::prelude::*;

#[cfg(feature = "pyo3")]
#[pymodule]
fn ephemeris_ledger_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<python::PyLedger>()?;
    m.add_class::<python::PyReservation>()?;
    Ok(())
}
