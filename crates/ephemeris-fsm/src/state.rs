//! State enum and transition validity table.
//!
//! 8 non-terminal + 2 structured terminal variants
//! (16 cells total). `EntityState::Delivered` carries `DeliveryQuality`;
//! `EntityState::Failed` carries `FailureMode`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeliveryQuality {
    OnTimeClean,
    LateClean,
    OnTimeViolating,
    LateViolating,
}

impl DeliveryQuality {
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryQuality::OnTimeClean => "OnTimeClean",
            DeliveryQuality::LateClean => "LateClean",
            DeliveryQuality::OnTimeViolating => "OnTimeViolating",
            DeliveryQuality::LateViolating => "LateViolating",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "OnTimeClean" | "on_time_clean" => Some(DeliveryQuality::OnTimeClean),
            "LateClean" | "late_clean" => Some(DeliveryQuality::LateClean),
            "OnTimeViolating" | "on_time_violating" => Some(DeliveryQuality::OnTimeViolating),
            "LateViolating" | "late_violating" => Some(DeliveryQuality::LateViolating),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FailureMode {
    Stranded,
    Expired,
    UnreachableTopology,
    PolicyForbid,
}

impl FailureMode {
    pub fn as_str(self) -> &'static str {
        match self {
            FailureMode::Stranded => "Stranded",
            FailureMode::Expired => "Expired",
            FailureMode::UnreachableTopology => "UnreachableTopology",
            FailureMode::PolicyForbid => "PolicyForbid",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "Stranded" | "stranded" => Some(FailureMode::Stranded),
            "Expired" | "expired" => Some(FailureMode::Expired),
            "UnreachableTopology" | "unreachable_topology" | "unreachable" => {
                Some(FailureMode::UnreachableTopology)
            }
            "PolicyForbid" | "policy_forbid" => Some(FailureMode::PolicyForbid),
            _ => None,
        }
    }
}

/// The canonical state. 16 cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntityState {
    Created,
    Planned,
    Dwelling,
    Moving,
    Held,
    Deferred,
    Escalated,
    Recovered,
    Delivered(DeliveryQuality),
    Failed(FailureMode),
}

impl EntityState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, EntityState::Delivered(_) | EntityState::Failed(_))
    }

    pub fn discriminant(&self) -> StateDiscriminant {
        match self {
            EntityState::Created => StateDiscriminant::Created,
            EntityState::Planned => StateDiscriminant::Planned,
            EntityState::Dwelling => StateDiscriminant::Dwelling,
            EntityState::Moving => StateDiscriminant::Moving,
            EntityState::Held => StateDiscriminant::Held,
            EntityState::Deferred => StateDiscriminant::Deferred,
            EntityState::Escalated => StateDiscriminant::Escalated,
            EntityState::Recovered => StateDiscriminant::Recovered,
            EntityState::Delivered(_) => StateDiscriminant::Delivered,
            EntityState::Failed(_) => StateDiscriminant::Failed,
        }
    }
}

/// Coarse-grained discriminant for transition-table lookups.
/// Terminal detail is ignored at this level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StateDiscriminant {
    Created,
    Planned,
    Dwelling,
    Moving,
    Held,
    Deferred,
    Escalated,
    Recovered,
    Delivered,
    Failed,
}

impl StateDiscriminant {
    pub fn as_str(self) -> &'static str {
        match self {
            StateDiscriminant::Created => "Created",
            StateDiscriminant::Planned => "Planned",
            StateDiscriminant::Dwelling => "Dwelling",
            StateDiscriminant::Moving => "Moving",
            StateDiscriminant::Held => "Held",
            StateDiscriminant::Deferred => "Deferred",
            StateDiscriminant::Escalated => "Escalated",
            StateDiscriminant::Recovered => "Recovered",
            StateDiscriminant::Delivered => "Delivered",
            StateDiscriminant::Failed => "Failed",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "Created" | "created" => Some(StateDiscriminant::Created),
            "Planned" | "planned" => Some(StateDiscriminant::Planned),
            "Dwelling" | "dwelling" => Some(StateDiscriminant::Dwelling),
            "Moving" | "moving" => Some(StateDiscriminant::Moving),
            "Held" | "held" => Some(StateDiscriminant::Held),
            "Deferred" | "deferred" => Some(StateDiscriminant::Deferred),
            "Escalated" | "escalated" => Some(StateDiscriminant::Escalated),
            "Recovered" | "recovered" => Some(StateDiscriminant::Recovered),
            "Delivered" | "delivered" => Some(StateDiscriminant::Delivered),
            "Failed" | "failed" => Some(StateDiscriminant::Failed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_terminal_matches_delivered_or_failed() {
        assert!(EntityState::Delivered(DeliveryQuality::OnTimeClean).is_terminal());
        assert!(EntityState::Failed(FailureMode::Stranded).is_terminal());
        assert!(!EntityState::Created.is_terminal());
        assert!(!EntityState::Planned.is_terminal());
        assert!(!EntityState::Dwelling.is_terminal());
        assert!(!EntityState::Moving.is_terminal());
        assert!(!EntityState::Held.is_terminal());
        assert!(!EntityState::Deferred.is_terminal());
        assert!(!EntityState::Escalated.is_terminal());
        assert!(!EntityState::Recovered.is_terminal());
    }

    #[test]
    fn discriminant_strips_terminal_detail() {
        assert_eq!(
            EntityState::Delivered(DeliveryQuality::OnTimeClean).discriminant(),
            StateDiscriminant::Delivered
        );
        assert_eq!(
            EntityState::Delivered(DeliveryQuality::LateViolating).discriminant(),
            StateDiscriminant::Delivered
        );
        assert_eq!(
            EntityState::Failed(FailureMode::Stranded).discriminant(),
            StateDiscriminant::Failed
        );
        assert_eq!(
            EntityState::Failed(FailureMode::PolicyForbid).discriminant(),
            StateDiscriminant::Failed
        );
    }

    #[test]
    fn delivery_quality_roundtrip_str() {
        for q in [
            DeliveryQuality::OnTimeClean,
            DeliveryQuality::LateClean,
            DeliveryQuality::OnTimeViolating,
            DeliveryQuality::LateViolating,
        ] {
            assert_eq!(DeliveryQuality::from_str(q.as_str()), Some(q));
        }
    }

    #[test]
    fn failure_mode_roundtrip_str() {
        for f in [
            FailureMode::Stranded,
            FailureMode::Expired,
            FailureMode::UnreachableTopology,
            FailureMode::PolicyForbid,
        ] {
            assert_eq!(FailureMode::from_str(f.as_str()), Some(f));
        }
    }
}
