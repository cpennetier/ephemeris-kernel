//! StateMachine: per-entity state map, transition log, version counter.
//!
//! States under `parking_lot::RwLock<HashMap<u64, EntityState>>`,
//! log under `parking_lot::RwLock<Vec<Transition>>`, version under
//! `AtomicU64`. The `propose` function is pure — it returns a
//! `TransitionEnvelope` without mutating state. `commit_envelope`
//! is the only function that mutates.

use crate::envelope::{
    LedgerCommitmentLevel, LedgerOp, LedgerOpHints, RouteSegment, TransitionEnvelope,
};
use crate::errors::{Result, StateMachineError};
use crate::state::{EntityState, StateDiscriminant};
use crate::transition::{Transition, TransitionReason};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// The state machine.
#[derive(Debug)]
pub struct StateMachine {
    states: RwLock<HashMap<u64, EntityState>>,
    log: RwLock<Vec<Transition>>,
    version: AtomicU64,
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine {
    pub fn new() -> Self {
        Self {
            states: RwLock::new(HashMap::new()),
            log: RwLock::new(Vec::new()),
            version: AtomicU64::new(0),
        }
    }

    /// Register an entity, initialising it in `Created`. Errors if the
    /// entity is already registered (idempotency is left to caller).
    pub fn register(&self, entity_id: u64) -> Result<()> {
        let mut s = self.states.write();
        if s.contains_key(&entity_id) {
            return Err(StateMachineError::EntityAlreadyRegistered(entity_id));
        }
        s.insert(entity_id, EntityState::Created);
        // Registration bumps version so observers can detect a new entity.
        self.version.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Current state of an entity, if registered.
    pub fn state(&self, entity_id: u64) -> Option<EntityState> {
        self.states.read().get(&entity_id).copied()
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Full chronological history of an entity. Returns an empty vec if
    /// the entity has no transitions (e.g., only registered, never
    /// transitioned).
    pub fn history(&self, entity_id: u64) -> Vec<Transition> {
        self.log
            .read()
            .iter()
            .filter(|t| t.entity_id == entity_id)
            .cloned()
            .collect()
    }

    /// All entities currently in the given coarse-grained discriminant.
    pub fn entities_in_state(&self, discriminant: StateDiscriminant) -> Vec<u64> {
        let s = self.states.read();
        let mut ids: Vec<u64> = s
            .iter()
            .filter(|(_, st)| st.discriminant() == discriminant)
            .map(|(id, _)| *id)
            .collect();
        ids.sort();
        ids
    }

    /// Total count of registered entities.
    pub fn n_entities(&self) -> usize {
        self.states.read().len()
    }

    /// Length of the transition log.
    pub fn log_len(&self) -> usize {
        self.log.read().len()
    }

    /// Compute the target state for `(from, reason)`, without mutating.
    pub fn target_for(
        from: EntityState,
        reason: &TransitionReason,
    ) -> Result<EntityState> {
        if from.is_terminal() {
            return Err(StateMachineError::TerminalState(from));
        }
        let disc = from.discriminant();
        let to = match (disc, reason) {
            // Created
            (StateDiscriminant::Created, TransitionReason::PlanFound) => EntityState::Planned,
            (StateDiscriminant::Created, TransitionReason::Unreachable) => {
                EntityState::Failed(crate::state::FailureMode::UnreachableTopology)
            }
            // PreserveDeliverable is a self-loop —
            // the flow unit proceeds without changing state, only the log
            // entry is appended (admission case, before PlanFound).
            (StateDiscriminant::Created, TransitionReason::PreserveDeliverable) => {
                EntityState::Created
            }
            // Planned
            (StateDiscriminant::Planned, TransitionReason::EnterHub(_)) => EntityState::Dwelling,
            (StateDiscriminant::Planned, TransitionReason::Strand) => {
                EntityState::Failed(crate::state::FailureMode::Stranded)
            }
            // Dwelling
            (StateDiscriminant::Dwelling, TransitionReason::DepartHub(_)) => EntityState::Moving,
            (StateDiscriminant::Dwelling, TransitionReason::HoldByControl) => EntityState::Held,
            (StateDiscriminant::Dwelling, TransitionReason::Defer(_)) => EntityState::Deferred,
            (StateDiscriminant::Dwelling, TransitionReason::Escalate) => EntityState::Escalated,
            (StateDiscriminant::Dwelling, TransitionReason::ArriveDestination { quality }) => {
                EntityState::Delivered(*quality)
            }
            (StateDiscriminant::Dwelling, TransitionReason::Expire) => {
                EntityState::Failed(crate::state::FailureMode::Expired)
            }
            // PreserveDeliverable self-loop in
            // Dwelling — controller releases the flow unit through
            // congestion (predicted to deliver), state unchanged.
            (StateDiscriminant::Dwelling, TransitionReason::PreserveDeliverable) => {
                EntityState::Dwelling
            }
            // Moving
            (StateDiscriminant::Moving, TransitionReason::ArriveIntermediate(_)) => {
                EntityState::Dwelling
            }
            (StateDiscriminant::Moving, TransitionReason::ArriveDestination { quality }) => {
                EntityState::Delivered(*quality)
            }
            (StateDiscriminant::Moving, TransitionReason::Expire) => {
                EntityState::Failed(crate::state::FailureMode::Expired)
            }
            // Held
            (StateDiscriminant::Held, TransitionReason::ReleaseHold) => EntityState::Dwelling,
            (StateDiscriminant::Held, TransitionReason::Defer(_)) => EntityState::Deferred,
            (StateDiscriminant::Held, TransitionReason::Escalate) => EntityState::Escalated,
            (StateDiscriminant::Held, TransitionReason::Strand) => {
                EntityState::Failed(crate::state::FailureMode::Stranded)
            }
            (StateDiscriminant::Held, TransitionReason::Expire) => {
                EntityState::Failed(crate::state::FailureMode::Expired)
            }
            (StateDiscriminant::Held, TransitionReason::PolicyTerminate) => {
                EntityState::Failed(crate::state::FailureMode::PolicyForbid)
            }
            // Deferred
            (StateDiscriminant::Deferred, TransitionReason::Replan) => EntityState::Planned,
            (StateDiscriminant::Deferred, TransitionReason::Strand) => {
                EntityState::Failed(crate::state::FailureMode::Stranded)
            }
            (StateDiscriminant::Deferred, TransitionReason::Expire) => {
                EntityState::Failed(crate::state::FailureMode::Expired)
            }
            // Escalated
            (StateDiscriminant::Escalated, TransitionReason::DepartHub(_)) => EntityState::Moving,
            (StateDiscriminant::Escalated, TransitionReason::EnterHub(_)) => EntityState::Dwelling,
            (StateDiscriminant::Escalated, TransitionReason::Recover) => EntityState::Recovered,
            (StateDiscriminant::Escalated, TransitionReason::Strand) => {
                EntityState::Failed(crate::state::FailureMode::Stranded)
            }
            // Recovered
            (StateDiscriminant::Recovered, TransitionReason::EnterHub(_)) => EntityState::Dwelling,
            (StateDiscriminant::Recovered, TransitionReason::DepartHub(_)) => EntityState::Moving,
            (StateDiscriminant::Recovered, TransitionReason::ArriveDestination { quality }) => {
                EntityState::Delivered(*quality)
            }
            // Anything not in the table is illegal.
            _ => {
                return Err(StateMachineError::IllegalTransition {
                    from,
                    reason: *reason,
                });
            }
        };
        Ok(to)
    }

    /// Build the ledger ops for a given transition. Pure — no mutation.
    fn ledger_ops_for(
        entity_id: u64,
        from: EntityState,
        to: EntityState,
        reason: &TransitionReason,
        hints: &LedgerOpHints,
    ) -> Result<Vec<LedgerOp>> {
        let _ = from;
        let _ = entity_id;
        match reason {
            // Plan found: reserve every segment of the planned route at Hard.
            TransitionReason::PlanFound => {
                if hints.planned_route.is_empty() {
                    return Err(StateMachineError::MissingHints(
                        "planned_route is required for PlanFound",
                    ));
                }
                let owner = entity_id;
                Ok(hints
                    .planned_route
                    .iter()
                    .map(|seg: &RouteSegment| LedgerOp::Reserve {
                        kind: seg.kind,
                        resource_id: seg.resource_id,
                        bucket: seg.bucket,
                        units: seg.units,
                        level: LedgerCommitmentLevel::Hard,
                        owner,
                    })
                    .collect())
            }
            // Hold by control: downgrade all current Hard reservations
            // to Soft via the proper LedgerOp::Downgrade op (added in
            // B-D1 cleanup; pre-B-D1 this branch emitted a malformed
            // Promote { new_level: Soft } that the ledger rejects because
            // Promote only moves upward).
            TransitionReason::HoldByControl => Ok(hints
                .current_reservations
                .iter()
                .filter(|(_, lvl)| *lvl == LedgerCommitmentLevel::Hard)
                .map(|(rid, _)| LedgerOp::Downgrade {
                    reservation_id: *rid,
                    new_level: LedgerCommitmentLevel::Soft,
                })
                .collect()),
            // Release hold: caller is responsible for re-reserving at
            // Hard if needed; the state machine only signals the
            // transition. No ledger ops emitted.
            TransitionReason::ReleaseHold => Ok(Vec::new()),
            // Terminal failures release all reservations.
            TransitionReason::Strand
            | TransitionReason::Expire
            | TransitionReason::Unreachable
            | TransitionReason::PolicyTerminate => Ok(hints
                .current_reservations
                .iter()
                .map(|(rid, _)| LedgerOp::Release {
                    reservation_id: *rid,
                })
                .collect()),
            // Terminal success: release all reservations (the flow unit is gone).
            TransitionReason::ArriveDestination { .. } => {
                let _ = to;
                Ok(hints
                    .current_reservations
                    .iter()
                    .map(|(rid, _)| LedgerOp::Release {
                        reservation_id: *rid,
                    })
                    .collect())
            }
            // Other transitions: no ledger ops (the caller manages
            // reservations along the way).
            _ => Ok(Vec::new()),
        }
    }

    /// Propose a transition. Pure — does not mutate state or log.
    /// Returns a `TransitionEnvelope` the caller can preview, retry,
    /// or commit.
    pub fn propose(
        &self,
        entity_id: u64,
        reason: TransitionReason,
        at_bucket: u32,
        hints: &LedgerOpHints,
    ) -> Result<TransitionEnvelope> {
        let from = self
            .state(entity_id)
            .ok_or(StateMachineError::EntityNotRegistered(entity_id))?;
        let to = Self::target_for(from, &reason)?;
        let ledger_ops = Self::ledger_ops_for(entity_id, from, to, &reason, hints)?;
        let transition = Transition {
            entity_id,
            from,
            to,
            reason,
            at_bucket,
            // Filled in at commit time so the version is correct
            // relative to the actual commit.
            at_version: 0,
        };
        Ok(TransitionEnvelope {
            transition,
            ledger_ops,
        })
    }

    /// Commit a previously-proposed envelope. Validates that the
    /// entity's current state still matches `envelope.transition.from`
    /// (stale-envelope detection). On success, atomically updates state
    /// and appends to the log under a single write lock.
    pub fn commit_envelope(&self, envelope: &TransitionEnvelope) -> Result<()> {
        let entity_id = envelope.transition.entity_id;
        let expected = envelope.transition.from;
        let to = envelope.transition.to;

        // Take both write locks together (states first, then log) and
        // a single version increment to satisfy the atomicity contract.
        let mut s = self.states.write();
        let mut l = self.log.write();
        let actual = s
            .get(&entity_id)
            .copied()
            .ok_or(StateMachineError::EntityNotRegistered(entity_id))?;
        if actual != expected {
            return Err(StateMachineError::StaleEnvelope { expected, actual });
        }
        let new_version = self.version.fetch_add(1, Ordering::SeqCst) + 1;
        s.insert(entity_id, to);
        let mut committed = envelope.transition.clone();
        committed.at_version = new_version;
        l.push(committed);
        Ok(())
    }

    /// Propose and commit in one call (the common case).
    pub fn propose_and_commit(
        &self,
        entity_id: u64,
        reason: TransitionReason,
        at_bucket: u32,
        hints: &LedgerOpHints,
    ) -> Result<TransitionEnvelope> {
        let env = self.propose(entity_id, reason, at_bucket, hints)?;
        self.commit_envelope(&env)?;
        // Return the envelope with the assigned version filled in by
        // re-reading the last log entry.
        let mut env = env;
        let last_version = self.version();
        env.transition.at_version = last_version;
        Ok(env)
    }

    /// Full log, ordered by version (insertion order).
    pub fn all_transitions(&self) -> Vec<Transition> {
        self.log.read().clone()
    }
}
