//! Transition reason enum + Transition record.

use crate::state::{DeliveryQuality, EntityState};
use serde::{Deserialize, Serialize};

pub type NodeId = u64;
pub type LedgerVersion = u64;

/// What caused the transition. `ArriveDestination` carries the
/// `DeliveryQuality` the caller has pre-computed from per-flow unit
/// constraints (the state machine does not infer "clean vs violating").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransitionReason {
    PlanFound,
    PlanFailed,
    EnterHub(NodeId),
    DepartHub(NodeId),
    ArriveIntermediate(NodeId),
    ArriveDestination { quality: DeliveryQuality },
    HoldByControl,
    ReleaseHold,
    Defer(u32),
    Replan,
    Escalate,
    Recover,
    Expire,
    Strand,
    Unreachable,
    PolicyTerminate,
    /// The state-machine rule cascade (v2).
    /// Records that the controller's R2 "preserve-deliverable" rule
    /// fired: predicted delivery probability ≥
    /// θ.preserve_deliverable_confidence, so the flow unit is released
    /// through congestion that R7 would otherwise flag for failure.
    /// Self-loop: state unchanged (Dwelling→Dwelling mid-journey,
    /// Created→Created at admission). Logged for observability.
    PreserveDeliverable,
}

impl TransitionReason {
    /// Encode as a string for PyO3 / JSON round-trip.
    pub fn as_string(&self) -> String {
        match self {
            TransitionReason::PlanFound => "plan_found".into(),
            TransitionReason::PlanFailed => "plan_failed".into(),
            TransitionReason::EnterHub(h) => format!("enter_node:{}", h),
            TransitionReason::DepartHub(h) => format!("depart_node:{}", h),
            TransitionReason::ArriveIntermediate(h) => {
                format!("arrive_intermediate:{}", h)
            }
            TransitionReason::ArriveDestination { quality } => match quality {
                DeliveryQuality::OnTimeClean => {
                    "arrive_destination:on_time_clean".into()
                }
                DeliveryQuality::LateClean => "arrive_destination:late_clean".into(),
                DeliveryQuality::OnTimeViolating => {
                    "arrive_destination:on_time_violating".into()
                }
                DeliveryQuality::LateViolating => {
                    "arrive_destination:late_violating".into()
                }
            },
            TransitionReason::HoldByControl => "hold_by_control".into(),
            TransitionReason::ReleaseHold => "release_hold".into(),
            TransitionReason::Defer(n) => format!("defer:{}", n),
            TransitionReason::Replan => "replan".into(),
            TransitionReason::Escalate => "escalate".into(),
            TransitionReason::Recover => "recover".into(),
            TransitionReason::Expire => "expire".into(),
            TransitionReason::Strand => "strand".into(),
            TransitionReason::Unreachable => "unreachable".into(),
            TransitionReason::PolicyTerminate => "policy_terminate".into(),
            TransitionReason::PreserveDeliverable => "preserve_deliverable".into(),
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "plan_found" => Some(TransitionReason::PlanFound),
            "plan_failed" => Some(TransitionReason::PlanFailed),
            "hold_by_control" => Some(TransitionReason::HoldByControl),
            "release_hold" => Some(TransitionReason::ReleaseHold),
            "replan" => Some(TransitionReason::Replan),
            "escalate" => Some(TransitionReason::Escalate),
            "recover" => Some(TransitionReason::Recover),
            "expire" => Some(TransitionReason::Expire),
            "strand" => Some(TransitionReason::Strand),
            "unreachable" => Some(TransitionReason::Unreachable),
            "policy_terminate" => Some(TransitionReason::PolicyTerminate),
            "preserve_deliverable" => Some(TransitionReason::PreserveDeliverable),
            "arrive_destination:on_time_clean" => Some(TransitionReason::ArriveDestination {
                quality: DeliveryQuality::OnTimeClean,
            }),
            "arrive_destination:late_clean" => Some(TransitionReason::ArriveDestination {
                quality: DeliveryQuality::LateClean,
            }),
            "arrive_destination:on_time_violating" => Some(TransitionReason::ArriveDestination {
                quality: DeliveryQuality::OnTimeViolating,
            }),
            "arrive_destination:late_violating" => Some(TransitionReason::ArriveDestination {
                quality: DeliveryQuality::LateViolating,
            }),
            _ => {
                // Try parameterized forms: enter_node:N, depart_node:N,
                // arrive_intermediate:N, defer:N.
                if let Some(rest) = s.strip_prefix("enter_node:") {
                    return rest.parse().ok().map(TransitionReason::EnterHub);
                }
                if let Some(rest) = s.strip_prefix("depart_node:") {
                    return rest.parse().ok().map(TransitionReason::DepartHub);
                }
                if let Some(rest) = s.strip_prefix("arrive_intermediate:") {
                    return rest.parse().ok().map(TransitionReason::ArriveIntermediate);
                }
                if let Some(rest) = s.strip_prefix("defer:") {
                    return rest.parse().ok().map(TransitionReason::Defer);
                }
                None
            }
        }
    }
}

/// One row in the event log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub entity_id: u64,
    pub from: EntityState,
    pub to: EntityState,
    pub reason: TransitionReason,
    pub at_bucket: u32,
    pub at_version: LedgerVersion,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_string_roundtrip_simple() {
        for r in [
            TransitionReason::PlanFound,
            TransitionReason::PlanFailed,
            TransitionReason::HoldByControl,
            TransitionReason::ReleaseHold,
            TransitionReason::Replan,
            TransitionReason::Escalate,
            TransitionReason::Recover,
            TransitionReason::Expire,
            TransitionReason::Strand,
            TransitionReason::Unreachable,
            TransitionReason::PolicyTerminate,
            TransitionReason::PreserveDeliverable,
        ] {
            assert_eq!(TransitionReason::from_str(&r.as_string()), Some(r));
        }
    }

    #[test]
    fn preserve_deliverable_string_form() {
        // Canonical wire-format is `preserve_deliverable`.
        let r = TransitionReason::PreserveDeliverable;
        assert_eq!(r.as_string(), "preserve_deliverable");
        assert_eq!(
            TransitionReason::from_str("preserve_deliverable"),
            Some(TransitionReason::PreserveDeliverable),
        );
    }

    #[test]
    fn reason_string_roundtrip_with_node_id() {
        let r = TransitionReason::EnterHub(42);
        assert_eq!(r.as_string(), "enter_node:42");
        assert_eq!(TransitionReason::from_str(&r.as_string()), Some(r));

        let r = TransitionReason::DepartHub(7);
        assert_eq!(TransitionReason::from_str(&r.as_string()), Some(r));

        let r = TransitionReason::ArriveIntermediate(99);
        assert_eq!(TransitionReason::from_str(&r.as_string()), Some(r));
    }

    #[test]
    fn reason_string_roundtrip_with_defer_buckets() {
        let r = TransitionReason::Defer(5);
        assert_eq!(r.as_string(), "defer:5");
        assert_eq!(TransitionReason::from_str(&r.as_string()), Some(r));
    }

    #[test]
    fn reason_string_roundtrip_arrive_destination_all_qualities() {
        for q in [
            DeliveryQuality::OnTimeClean,
            DeliveryQuality::LateClean,
            DeliveryQuality::OnTimeViolating,
            DeliveryQuality::LateViolating,
        ] {
            let r = TransitionReason::ArriveDestination { quality: q };
            assert_eq!(TransitionReason::from_str(&r.as_string()), Some(r));
        }
    }

    #[test]
    fn invalid_reason_returns_none() {
        assert_eq!(TransitionReason::from_str("nope"), None);
        assert_eq!(TransitionReason::from_str("enter_node:abc"), None);
        assert_eq!(TransitionReason::from_str("arrive_destination:bogus"), None);
    }
}
