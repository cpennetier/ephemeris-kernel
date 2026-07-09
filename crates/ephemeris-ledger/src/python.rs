//! PyO3 bindings for the capacity ledger.
//!
//! Phase 1B will fill this in. Phase 1A leaves a minimal stub so the
//! crate compiles under `--features pyo3`. The actual binding surface
//! is specified in the PyO3 binding section of the spec.

#![cfg(feature = "pyo3")]

use crate::ledger::Ledger;
use crate::reservation::{CommitmentLevel, Reservation};
use crate::resource::ResourceKind;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use std::sync::Arc;

fn parse_kind(s: &str) -> PyResult<ResourceKind> {
    ResourceKind::from_str(s).ok_or_else(|| {
        PyValueError::new_err(format!("invalid kind {:?}: expected 'node' or 'lane'", s))
    })
}

fn parse_level(s: &str) -> PyResult<CommitmentLevel> {
    CommitmentLevel::from_str(s).ok_or_else(|| {
        PyValueError::new_err(format!(
            "invalid level {:?}: expected 'hard', 'soft', or 'forecast'",
            s
        ))
    })
}

#[pyclass(name = "Reservation")]
#[derive(Clone)]
pub struct PyReservation {
    inner: Reservation,
}

#[pymethods]
impl PyReservation {
    #[getter]
    fn id(&self) -> u64 {
        self.inner.id
    }
    #[getter]
    fn kind(&self) -> &'static str {
        self.inner.resource_kind.as_str()
    }
    #[getter]
    fn resource_id(&self) -> u64 {
        self.inner.resource_id
    }
    #[getter]
    fn bucket(&self) -> u32 {
        self.inner.bucket
    }
    #[getter]
    fn units(&self) -> u64 {
        self.inner.units
    }
    #[getter]
    fn level(&self) -> &'static str {
        self.inner.level.as_str()
    }
    #[getter]
    fn owner(&self) -> u64 {
        self.inner.owner
    }
    #[getter]
    fn created_at_version(&self) -> u64 {
        self.inner.created_at_version
    }
}

impl From<Reservation> for PyReservation {
    fn from(inner: Reservation) -> Self {
        Self { inner }
    }
}

#[pyclass(name = "Ledger")]
pub struct PyLedger {
    inner: Arc<Ledger>,
}

#[pymethods]
impl PyLedger {
    #[new]
    fn new() -> Self {
        Self {
            inner: Arc::new(Ledger::new()),
        }
    }

    fn register_lane(&self, lane_id: u64, capacity_per_bucket: u64, n_buckets: u32) {
        self.inner.register_lane(lane_id, capacity_per_bucket, n_buckets);
    }

    fn register_node(&self, node_id: u64, capacity_per_bucket: u64, n_buckets: u32) {
        self.inner.register_node(node_id, capacity_per_bucket, n_buckets);
    }

    #[pyo3(signature = (kind, resource_id, bucket, units, level, owner))]
    fn reserve(
        &self,
        kind: &str,
        resource_id: u64,
        bucket: u32,
        units: u64,
        level: &str,
        owner: u64,
    ) -> PyResult<u64> {
        let kind = parse_kind(kind)?;
        let level = parse_level(level)?;
        self.inner
            .reserve(kind, resource_id, bucket, units, level, owner)
            .map_err(|e| PyValueError::new_err(format!("{}", e)))
    }

    fn release(&self, reservation_id: u64) -> PyResult<()> {
        self.inner
            .release(reservation_id)
            .map_err(|e| PyValueError::new_err(format!("{}", e)))
    }

    fn promote(&self, reservation_id: u64, new_level: &str) -> PyResult<()> {
        let level = parse_level(new_level)?;
        self.inner
            .promote(reservation_id, level)
            .map_err(|e| PyValueError::new_err(format!("{}", e)))
    }

    fn downgrade(&self, reservation_id: u64, new_level: &str) -> PyResult<()> {
        let level = parse_level(new_level)?;
        self.inner
            .downgrade(reservation_id, level)
            .map_err(|e| PyValueError::new_err(format!("{}", e)))
    }

    fn free_on(&self, kind: &str, resource_id: u64, bucket: u32, level: &str) -> PyResult<u64> {
        let kind = parse_kind(kind)?;
        let level = parse_level(level)?;
        Ok(self.inner.free_on(kind, resource_id, bucket, level))
    }

    fn total_capacity(&self, kind: &str, resource_id: u64, bucket: u32) -> PyResult<u64> {
        let kind = parse_kind(kind)?;
        Ok(self.inner.total_capacity(kind, resource_id, bucket))
    }

    fn reservations_for(
        &self,
        kind: &str,
        resource_id: u64,
        bucket: u32,
        level: &str,
    ) -> PyResult<Vec<PyReservation>> {
        let kind = parse_kind(kind)?;
        let level = parse_level(level)?;
        Ok(self
            .inner
            .reservations_for(kind, resource_id, bucket, level)
            .into_iter()
            .map(PyReservation::from)
            .collect())
    }

    fn version(&self) -> u64 {
        self.inner.version()
    }

    fn n_reservations(&self) -> usize {
        self.inner.n_reservations()
    }

    /// Apply a single ledger op described by a dict. Schema:
    ///   {"type": "reserve", "kind": "lane"|"node", "resource_id": u64,
    ///    "bucket": u32, "units": u64, "level": "hard"|"soft"|"forecast",
    ///    "owner": u64}
    ///   {"type": "release", "reservation_id": u64}
    ///   {"type": "promote", "reservation_id": u64,
    ///    "new_level": "hard"|"soft"|"forecast"}
    ///   {"type": "downgrade", "reservation_id": u64,
    ///    "new_level": "hard"|"soft"|"forecast"}
    ///
    /// Designed for the envelope pattern — callers iterate
    /// `envelope.ledger_ops_for_pyledger(py)` and feed each dict to
    /// `ledger.apply(op_dict)`. Returns the new ReservationId for
    /// `"reserve"` ops; None for `"release"`, `"promote"`, `"downgrade"`.
    fn apply(&self, op: &pyo3::Bound<'_, pyo3::types::PyDict>) -> PyResult<Option<u64>> {
        use pyo3::types::PyAnyMethods;
        let ty: String = op
            .get_item("type")?
            .ok_or_else(|| PyValueError::new_err("op dict missing 'type'"))?
            .extract()?;
        match ty.as_str() {
            "reserve" => {
                let kind: String = op.get_item("kind")?.unwrap().extract()?;
                let resource_id: u64 = op.get_item("resource_id")?.unwrap().extract()?;
                let bucket: u32 = op.get_item("bucket")?.unwrap().extract()?;
                let units: u64 = op.get_item("units")?.unwrap().extract()?;
                let level: String = op.get_item("level")?.unwrap().extract()?;
                let owner: u64 = op.get_item("owner")?.unwrap().extract()?;
                let kind = parse_kind(&kind)?;
                let level = parse_level(&level)?;
                Ok(Some(
                    self.inner
                        .reserve(kind, resource_id, bucket, units, level, owner)
                        .map_err(|e| PyValueError::new_err(format!("{}", e)))?,
                ))
            }
            "release" => {
                let rid: u64 = op.get_item("reservation_id")?.unwrap().extract()?;
                self.inner
                    .release(rid)
                    .map_err(|e| PyValueError::new_err(format!("{}", e)))?;
                Ok(None)
            }
            "promote" => {
                let rid: u64 = op.get_item("reservation_id")?.unwrap().extract()?;
                let new_level: String = op.get_item("new_level")?.unwrap().extract()?;
                let level = parse_level(&new_level)?;
                self.inner
                    .promote(rid, level)
                    .map_err(|e| PyValueError::new_err(format!("{}", e)))?;
                Ok(None)
            }
            "downgrade" => {
                let rid: u64 = op.get_item("reservation_id")?.unwrap().extract()?;
                let new_level: String = op.get_item("new_level")?.unwrap().extract()?;
                let level = parse_level(&new_level)?;
                self.inner
                    .downgrade(rid, level)
                    .map_err(|e| PyValueError::new_err(format!("{}", e)))?;
                Ok(None)
            }
            _ => Err(PyValueError::new_err(format!(
                "unknown op type {:?}",
                ty
            ))),
        }
    }

    #[cfg(feature = "arrow-pyarrow")]
    fn snapshot<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        use arrow_pyarrow::ToPyArrow;
        let s = self.inner.snapshot();
        Ok(s.batch.to_pyarrow(py)?.into_bound(py))
    }
}
