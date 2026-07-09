//! Resource trait and concrete impls (node, lane).
//!
//! Node and lane resources are unified behind a single
//! `Resource` trait. The trait is generic enough to admit future
//! resource kinds (compute jobs, quantum gate-time, grid lines) without
//! core ledger changes.

use serde::{Deserialize, Serialize};

/// Discrete time bucket identifier. Zero-indexed. Bucket size is
/// fixed per ledger instance (typically 1 hour = 3_600_000 ATU at the
/// DES engine layer, but the ledger itself is unit-agnostic).
pub type TimeBucket = u32;

/// Resource identifier within a kind. Type-erased to `u64` for storage;
/// downstream callers may keep their own typed wrappers.
pub type ResourceId = u64;

/// What kind of finite-capacity resource this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum ResourceKind {
    /// Node processing capacity (sortation throughput, dock slots).
    Node = 0,
    /// Lane transport capacity (vehicle slots on a lane).
    Lane = 1,
}

impl ResourceKind {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(ResourceKind::Node),
            1 => Some(ResourceKind::Lane),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ResourceKind::Node => "node",
            ResourceKind::Lane => "lane",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "node" => Some(ResourceKind::Node),
            "lane" => Some(ResourceKind::Lane),
            _ => None,
        }
    }
}

/// Resource trait. Future resource kinds implement this without
/// modifying the ledger core.
pub trait Resource: Send + Sync + 'static {
    fn id(&self) -> ResourceId;
    fn kind(&self) -> ResourceKind;
    /// Nominal capacity at a given bucket. v1 typically returns a
    /// constant; time-varying capacity (e.g., nodes that close
    /// weekends) is admissible without core changes.
    fn nominal_capacity(&self, bucket: TimeBucket) -> u64;
}

/// Lane with constant per-bucket capacity. v1 baseline.
#[derive(Clone, Debug)]
pub struct LaneResource {
    pub id: ResourceId,
    pub base_capacity_per_bucket: u64,
}

impl Resource for LaneResource {
    fn id(&self) -> ResourceId {
        self.id
    }
    fn kind(&self) -> ResourceKind {
        ResourceKind::Lane
    }
    fn nominal_capacity(&self, _bucket: TimeBucket) -> u64 {
        self.base_capacity_per_bucket
    }
}

/// Node with constant per-bucket processing capacity. v1 baseline.
#[derive(Clone, Debug)]
pub struct NodeResource {
    pub id: ResourceId,
    pub base_processing_per_bucket: u64,
}

impl Resource for NodeResource {
    fn id(&self) -> ResourceId {
        self.id
    }
    fn kind(&self) -> ResourceKind {
        ResourceKind::Node
    }
    fn nominal_capacity(&self, _bucket: TimeBucket) -> u64 {
        self.base_processing_per_bucket
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_roundtrip_u8() {
        for k in [ResourceKind::Node, ResourceKind::Lane] {
            assert_eq!(ResourceKind::from_u8(k.as_u8()), Some(k));
        }
        assert_eq!(ResourceKind::from_u8(42), None);
    }

    #[test]
    fn kind_roundtrip_str() {
        for k in [ResourceKind::Node, ResourceKind::Lane] {
            assert_eq!(ResourceKind::from_str(k.as_str()), Some(k));
        }
        assert_eq!(ResourceKind::from_str("NODE"), Some(ResourceKind::Node));
        assert_eq!(ResourceKind::from_str("nope"), None);
    }

    #[test]
    fn lane_resource_capacity_is_constant() {
        let e = LaneResource {
            id: 7,
            base_capacity_per_bucket: 100,
        };
        assert_eq!(e.kind(), ResourceKind::Lane);
        for b in 0..10 {
            assert_eq!(e.nominal_capacity(b), 100);
        }
    }

    #[test]
    fn node_resource_capacity_is_constant() {
        let h = NodeResource {
            id: 3,
            base_processing_per_bucket: 50,
        };
        assert_eq!(h.kind(), ResourceKind::Node);
        for b in 0..10 {
            assert_eq!(h.nominal_capacity(b), 50);
        }
    }
}
