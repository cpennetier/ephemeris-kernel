//! Entity trait + concrete impls.
//!
//! Parallel to ephemeris-ledger's `Resource` trait. v1 implements
//! `FlowUnitEntity`; future entity kinds (jobs, circuits, loads) add
//! without core changes.

use serde::{Deserialize, Serialize};

pub type EntityId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum EntityKind {
    /// Concrete v1 flow-unit entity.
    FlowUnit = 0,
    /// Compute scheduling job — future Sprint extension.
    Job = 1,
    /// Quantum circuit — future Sprint extension.
    Circuit = 2,
    /// Grid load — future Sprint extension.
    Load = 3,
}

impl EntityKind {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(EntityKind::FlowUnit),
            1 => Some(EntityKind::Job),
            2 => Some(EntityKind::Circuit),
            3 => Some(EntityKind::Load),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            EntityKind::FlowUnit => "flow unit",
            EntityKind::Job => "job",
            EntityKind::Circuit => "circuit",
            EntityKind::Load => "load",
        }
    }
}

/// Entity trait — abstracts over kinds so the state machine is generic.
pub trait Entity: Send + Sync + 'static {
    fn id(&self) -> EntityId;
    fn kind(&self) -> EntityKind;
}

/// v1 concrete: a flow unit.
#[derive(Clone, Debug)]
pub struct FlowUnitEntity {
    pub id: EntityId,
}

impl Entity for FlowUnitEntity {
    fn id(&self) -> EntityId {
        self.id
    }
    fn kind(&self) -> EntityKind {
        EntityKind::FlowUnit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_roundtrip() {
        for k in [
            EntityKind::FlowUnit,
            EntityKind::Job,
            EntityKind::Circuit,
            EntityKind::Load,
        ] {
            assert_eq!(EntityKind::from_u8(k.as_u8()), Some(k));
        }
        assert_eq!(EntityKind::from_u8(99), None);
    }

    #[test]
    fn flow_unit_entity_carries_id_and_kind() {
        let p = FlowUnitEntity { id: 42 };
        assert_eq!(p.id(), 42);
        assert_eq!(p.kind(), EntityKind::FlowUnit);
    }
}
