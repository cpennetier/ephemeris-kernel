//! Ephemeris Kernel Tier 1 Discrete Event Simulator.
//!
//! Rust crate exposed to Python via PyO3/maturin.
//! Reads Arrow IPC (nodes, lanes, schedules, demand) and simulates
//! flow-unit movement through a capacitated network.

pub mod types;
pub mod events;
pub mod graph;
pub mod schedule;
pub mod demand;
pub mod router;
pub mod time_router;
pub mod engine;
pub mod surfaces;
pub mod surface_writer;
pub mod run_manifest;
pub mod kpi;
pub mod io;
pub mod feasibility_oracle;

#[cfg(test)]
mod tests;

#[cfg(feature = "pyo3")]
mod python;

#[cfg(feature = "pyo3")]
use pyo3::prelude::*;

#[cfg(feature = "pyo3")]
#[pymodule]
fn _engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<python::PyEngine>()?;
    Ok(())
}
