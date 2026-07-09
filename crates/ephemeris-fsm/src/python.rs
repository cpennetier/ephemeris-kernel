//! PyO3 bindings for the entity state machine.

#![cfg(feature = "pyo3")]

use crate::envelope::{
    LedgerCommitmentLevel, LedgerOp, LedgerOpHints, LedgerResourceKind, RouteSegment,
    TransitionEnvelope,
};
use crate::machine::StateMachine;
use crate::state::{EntityState, StateDiscriminant};
use crate::transition::{Transition, TransitionReason};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::sync::Arc;

fn parse_reason(s: &str) -> PyResult<TransitionReason> {
    TransitionReason::from_str(s).ok_or_else(|| {
        PyValueError::new_err(format!(
            "invalid transition reason {:?}",
            s
        ))
    })
}

fn state_to_tuple(state: EntityState) -> (String, Option<String>) {
    match state {
        EntityState::Delivered(q) => ("Delivered".into(), Some(q.as_str().into())),
        EntityState::Failed(f) => ("Failed".into(), Some(f.as_str().into())),
        other => (other.discriminant().as_str().into(), None),
    }
}

fn parse_kind(s: &str) -> PyResult<LedgerResourceKind> {
    match s {
        "node" | "Node" => Ok(LedgerResourceKind::Node),
        "lane" | "Lane" => Ok(LedgerResourceKind::Lane),
        _ => Err(PyValueError::new_err(format!(
            "invalid resource kind {:?}",
            s
        ))),
    }
}

fn parse_level(s: &str) -> PyResult<LedgerCommitmentLevel> {
    match s {
        "hard" | "Hard" => Ok(LedgerCommitmentLevel::Hard),
        "soft" | "Soft" => Ok(LedgerCommitmentLevel::Soft),
        "forecast" | "Forecast" => Ok(LedgerCommitmentLevel::Forecast),
        _ => Err(PyValueError::new_err(format!(
            "invalid commitment level {:?}",
            s
        ))),
    }
}

/// Parse `LedgerOpHints` from a JSON string. Schema:
/// ```json
/// {
///   "current_reservations": [[<rid>, "<level>"], ...],
///   "planned_route": [
///     {"kind": "lane", "resource_id": 1, "bucket": 0, "units": 1}, ...
///   ],
///   "current_node": <u64> | null,
///   "deadline_bucket": <u32> | null
/// }
/// ```
fn parse_hints(json: &str) -> PyResult<LedgerOpHints> {
    if json.is_empty() {
        return Ok(LedgerOpHints::default());
    }
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| PyValueError::new_err(format!("hints JSON parse: {}", e)))?;
    let mut hints = LedgerOpHints::default();
    if let Some(arr) = v.get("current_reservations").and_then(|x| x.as_array()) {
        for entry in arr {
            let pair = entry.as_array().ok_or_else(|| {
                PyValueError::new_err("current_reservations entry must be [rid, level]")
            })?;
            if pair.len() != 2 {
                return Err(PyValueError::new_err(
                    "current_reservations entry must be [rid, level]",
                ));
            }
            let rid = pair[0]
                .as_u64()
                .ok_or_else(|| PyValueError::new_err("rid must be u64"))?;
            let level = parse_level(
                pair[1]
                    .as_str()
                    .ok_or_else(|| PyValueError::new_err("level must be string"))?,
            )?;
            hints.current_reservations.push((rid, level));
        }
    }
    if let Some(arr) = v.get("planned_route").and_then(|x| x.as_array()) {
        for seg in arr {
            let obj = seg.as_object().ok_or_else(|| {
                PyValueError::new_err("planned_route entry must be object")
            })?;
            let kind = parse_kind(
                obj.get("kind")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| PyValueError::new_err("kind required"))?,
            )?;
            let resource_id = obj
                .get("resource_id")
                .and_then(|x| x.as_u64())
                .ok_or_else(|| PyValueError::new_err("resource_id required"))?;
            let bucket = obj
                .get("bucket")
                .and_then(|x| x.as_u64())
                .ok_or_else(|| PyValueError::new_err("bucket required"))?
                as u32;
            let units = obj
                .get("units")
                .and_then(|x| x.as_u64())
                .ok_or_else(|| PyValueError::new_err("units required"))?;
            hints.planned_route.push(RouteSegment {
                kind,
                resource_id,
                bucket,
                units,
            });
        }
    }
    hints.current_node = v.get("current_node").and_then(|x| x.as_u64());
    hints.deadline_bucket = v.get("deadline_bucket").and_then(|x| x.as_u64()).map(|x| x as u32);
    Ok(hints)
}

#[pyclass(name = "Transition")]
#[derive(Clone)]
pub struct PyTransition {
    inner: Transition,
}

#[pymethods]
impl PyTransition {
    #[getter]
    fn entity_id(&self) -> u64 {
        self.inner.entity_id
    }
    #[getter]
    fn from_state(&self) -> (String, Option<String>) {
        state_to_tuple(self.inner.from)
    }
    #[getter]
    fn to_state(&self) -> (String, Option<String>) {
        state_to_tuple(self.inner.to)
    }
    #[getter]
    fn reason(&self) -> String {
        self.inner.reason.as_string()
    }
    #[getter]
    fn at_bucket(&self) -> u32 {
        self.inner.at_bucket
    }
    #[getter]
    fn at_version(&self) -> u64 {
        self.inner.at_version
    }
}

#[pyclass(name = "TransitionEnvelope")]
#[derive(Clone)]
pub struct PyTransitionEnvelope {
    pub(crate) inner: TransitionEnvelope,
}

#[pymethods]
impl PyTransitionEnvelope {
    #[getter]
    fn transition(&self) -> PyTransition {
        PyTransition {
            inner: self.inner.transition.clone(),
        }
    }

    /// Convert the envelope's ledger ops into a list of dicts that
    /// `ephemeris_ledger_py.Ledger.apply(op_dict)` can consume. Schema:
    ///   {"type": "reserve", "kind": "lane", "resource_id": ..., ...}
    ///   {"type": "release", "reservation_id": ...}
    ///   {"type": "promote", "reservation_id": ..., "new_level": "hard"}
    fn ledger_ops_for_pyledger<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        let mut out = Vec::new();
        for op in &self.inner.ledger_ops {
            let d = PyDict::new(py);
            match op {
                LedgerOp::Reserve {
                    kind,
                    resource_id,
                    bucket,
                    units,
                    level,
                    owner,
                } => {
                    d.set_item("type", "reserve")?;
                    d.set_item("kind", kind.as_str())?;
                    d.set_item("resource_id", resource_id)?;
                    d.set_item("bucket", bucket)?;
                    d.set_item("units", units)?;
                    d.set_item("level", level.as_str())?;
                    d.set_item("owner", owner)?;
                }
                LedgerOp::Release { reservation_id } => {
                    d.set_item("type", "release")?;
                    d.set_item("reservation_id", reservation_id)?;
                }
                LedgerOp::Promote {
                    reservation_id,
                    new_level,
                } => {
                    d.set_item("type", "promote")?;
                    d.set_item("reservation_id", reservation_id)?;
                    d.set_item("new_level", new_level.as_str())?;
                }
                LedgerOp::Downgrade {
                    reservation_id,
                    new_level,
                } => {
                    d.set_item("type", "downgrade")?;
                    d.set_item("reservation_id", reservation_id)?;
                    d.set_item("new_level", new_level.as_str())?;
                }
            }
            out.push(d);
        }
        Ok(out)
    }
}

impl From<TransitionEnvelope> for PyTransitionEnvelope {
    fn from(inner: TransitionEnvelope) -> Self {
        Self { inner }
    }
}

#[pyclass(name = "StateMachine")]
pub struct PyStateMachine {
    inner: Arc<StateMachine>,
}

#[pymethods]
impl PyStateMachine {
    #[new]
    fn new() -> Self {
        Self {
            inner: Arc::new(StateMachine::new()),
        }
    }

    fn register(&self, entity_id: u64) -> PyResult<()> {
        self.inner
            .register(entity_id)
            .map_err(|e| PyValueError::new_err(format!("{}", e)))
    }

    fn state(&self, entity_id: u64) -> Option<(String, Option<String>)> {
        self.inner.state(entity_id).map(state_to_tuple)
    }

    fn version(&self) -> u64 {
        self.inner.version()
    }

    fn n_entities(&self) -> usize {
        self.inner.n_entities()
    }

    fn log_len(&self) -> usize {
        self.inner.log_len()
    }

    fn entities_in_state(&self, discriminant: &str) -> PyResult<Vec<u64>> {
        let disc = StateDiscriminant::from_str(discriminant)
            .ok_or_else(|| PyValueError::new_err(format!("invalid state {:?}", discriminant)))?;
        Ok(self.inner.entities_in_state(disc))
    }

    fn history(&self, entity_id: u64) -> Vec<PyTransition> {
        self.inner
            .history(entity_id)
            .into_iter()
            .map(|t| PyTransition { inner: t })
            .collect()
    }

    #[pyo3(signature = (entity_id, reason, at_bucket, hints_json="".to_string()))]
    fn propose(
        &self,
        entity_id: u64,
        reason: &str,
        at_bucket: u32,
        hints_json: String,
    ) -> PyResult<PyTransitionEnvelope> {
        let reason = parse_reason(reason)?;
        let hints = parse_hints(&hints_json)?;
        self.inner
            .propose(entity_id, reason, at_bucket, &hints)
            .map(PyTransitionEnvelope::from)
            .map_err(|e| PyValueError::new_err(format!("{}", e)))
    }

    fn commit_envelope(&self, envelope: &PyTransitionEnvelope) -> PyResult<()> {
        self.inner
            .commit_envelope(&envelope.inner)
            .map_err(|e| PyValueError::new_err(format!("{}", e)))
    }

    #[pyo3(signature = (entity_id, reason, at_bucket, hints_json="".to_string()))]
    fn propose_and_commit(
        &self,
        entity_id: u64,
        reason: &str,
        at_bucket: u32,
        hints_json: String,
    ) -> PyResult<PyTransitionEnvelope> {
        let reason = parse_reason(reason)?;
        let hints = parse_hints(&hints_json)?;
        self.inner
            .propose_and_commit(entity_id, reason, at_bucket, &hints)
            .map(PyTransitionEnvelope::from)
            .map_err(|e| PyValueError::new_err(format!("{}", e)))
    }

    #[cfg(feature = "arrow-pyarrow")]
    fn snapshot_log<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        use arrow_pyarrow::ToPyArrow;
        let s = self.inner.snapshot_log();
        Ok(s.batch.to_pyarrow(py)?.into_bound(py))
    }
}
