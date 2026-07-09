//! Capacity ledger core. Direct query and write API.
//!
//! - Storage: `DashMap<ResourceKey, BTreeMap<ReservationId, Reservation>>`
//!   for sharded concurrent reads with deterministic per-key iteration.
//! - Capacity registry: `DashMap<(kind, id), HashMap<bucket, u64>>` —
//!   per-resource per-bucket nominal capacity, populated by
//!   `register_resource`.
//! - Atomics for `version` and `next_reservation_id`.
//!
//! Admissibility: `Hard` and `Soft` reservations require
//! `free_on(..., Soft) >= units` (i.e., capacity minus already-booked
//! Hard+Soft). `Forecast` reservations are always admissible — they
//! consume nothing in the Hard/Soft pool but are visible in queries
//! at level `Forecast`.

use crate::errors::{LedgerError, LedgerResult};
use crate::reservation::{
    CommitmentLevel, LedgerVersion, OwnerId, Reservation, ReservationId,
};
use crate::resource::{Resource, ResourceId, ResourceKind, TimeBucket};
use dashmap::DashMap;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

/// Storage key. A reservation is uniquely identified by
/// `(kind, resource_id, bucket)` for storage; its `ReservationId` is
/// the within-key insertion order, monotonic globally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ResourceKey {
    pub kind: ResourceKind,
    pub resource_id: ResourceId,
    pub bucket: TimeBucket,
}

impl ResourceKey {
    #[inline]
    pub fn new(kind: ResourceKind, resource_id: ResourceId, bucket: TimeBucket) -> Self {
        Self {
            kind,
            resource_id,
            bucket,
        }
    }
}

/// Per-(kind, resource_id) capacity table, keyed by bucket.
#[derive(Clone, Debug, Default)]
struct CapacityTable {
    by_bucket: HashMap<TimeBucket, u64>,
}

/// The ledger.
///
/// Cloning the `Ledger` shallow-clones the underlying DashMaps; both
/// clones see the same backing storage. This is the intended shape for
/// shared multi-reader access. To get an isolated copy, take a
/// snapshot and rebuild.
#[derive(Debug)]
pub struct Ledger {
    storage: DashMap<ResourceKey, BTreeMap<ReservationId, Reservation>>,
    capacity: DashMap<(ResourceKind, ResourceId), CapacityTable>,
    version: AtomicU64,
    next_reservation_id: AtomicU64,
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new()
    }
}

impl Ledger {
    pub fn new() -> Self {
        Self {
            storage: DashMap::new(),
            capacity: DashMap::new(),
            version: AtomicU64::new(0),
            next_reservation_id: AtomicU64::new(0),
        }
    }

    /// Register a resource for a range of buckets. Idempotent: re-registering
    /// the same `(kind, id, bucket)` triple overwrites the nominal capacity.
    pub fn register_resource<R: Resource>(
        &self,
        resource: &R,
        bucket_range: std::ops::Range<TimeBucket>,
    ) {
        let key = (resource.kind(), resource.id());
        let mut entry = self.capacity.entry(key).or_default();
        for b in bucket_range {
            entry.by_bucket.insert(b, resource.nominal_capacity(b));
        }
        // Capacity registry changes bump version so cached readers can detect.
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    /// Convenience: register an lane with constant capacity across `[0, n_buckets)`.
    pub fn register_lane(
        &self,
        resource_id: ResourceId,
        capacity_per_bucket: u64,
        n_buckets: TimeBucket,
    ) {
        let mut entry = self
            .capacity
            .entry((ResourceKind::Lane, resource_id))
            .or_default();
        for b in 0..n_buckets {
            entry.by_bucket.insert(b, capacity_per_bucket);
        }
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    /// Convenience: register a node with constant capacity across `[0, n_buckets)`.
    pub fn register_node(
        &self,
        resource_id: ResourceId,
        capacity_per_bucket: u64,
        n_buckets: TimeBucket,
    ) {
        let mut entry = self
            .capacity
            .entry((ResourceKind::Node, resource_id))
            .or_default();
        for b in 0..n_buckets {
            entry.by_bucket.insert(b, capacity_per_bucket);
        }
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    /// Total nominal capacity at `(kind, resource_id, bucket)`. Returns 0
    /// if the resource is not registered or the bucket is outside the
    /// registered range.
    pub fn total_capacity(
        &self,
        kind: ResourceKind,
        resource_id: ResourceId,
        bucket: TimeBucket,
    ) -> u64 {
        self.capacity
            .get(&(kind, resource_id))
            .map(|t| *t.by_bucket.get(&bucket).unwrap_or(&0))
            .unwrap_or(0)
    }

    /// Sum of `units` across all reservations on the key at level `>= query`.
    fn booked_at(&self, key: ResourceKey, query: CommitmentLevel) -> u64 {
        self.storage
            .get(&key)
            .map(|inner| {
                inner
                    .iter()
                    .filter(|(_, r)| r.level.counts_at(query))
                    .map(|(_, r)| r.units)
                    .sum::<u64>()
            })
            .unwrap_or(0)
    }

    /// Free units.
    pub fn free_on(
        &self,
        kind: ResourceKind,
        resource_id: ResourceId,
        bucket: TimeBucket,
        level: CommitmentLevel,
    ) -> u64 {
        let total = self.total_capacity(kind, resource_id, bucket);
        let key = ResourceKey::new(kind, resource_id, bucket);
        let booked = self.booked_at(key, level);
        total.saturating_sub(booked)
    }

    /// Reserve `units` of resource at bucket with the given commitment level.
    /// Returns the new ReservationId on success.
    pub fn reserve(
        &self,
        kind: ResourceKind,
        resource_id: ResourceId,
        bucket: TimeBucket,
        units: u64,
        level: CommitmentLevel,
        owner: OwnerId,
    ) -> LedgerResult<ReservationId> {
        if units == 0 {
            return Err(LedgerError::ZeroUnits);
        }
        // Capacity must be registered (resource known to ledger).
        let total = self.total_capacity(kind, resource_id, bucket);
        if total == 0 && !self.capacity.contains_key(&(kind, resource_id)) {
            return Err(LedgerError::ResourceNotRegistered {
                kind,
                id: resource_id,
            });
        }
        // Hard and Soft both check against free-at-Soft (i.e., capacity
        // minus existing Hard+Soft). Forecast is always admissible.
        let key = ResourceKey::new(kind, resource_id, bucket);
        if matches!(level, CommitmentLevel::Hard | CommitmentLevel::Soft) {
            let booked_soft = self.booked_at(key, CommitmentLevel::Soft);
            let free = total.saturating_sub(booked_soft);
            if free < units {
                return Err(LedgerError::InsufficientCapacity {
                    kind,
                    id: resource_id,
                    bucket,
                    requested: units,
                    free,
                });
            }
        }
        // Allocate IDs and insert. Both atomic; insert under DashMap shard lock.
        let new_id = self.next_reservation_id.fetch_add(1, Ordering::SeqCst);
        let new_version = self.version.fetch_add(1, Ordering::SeqCst) + 1;
        let reservation = Reservation {
            id: new_id,
            resource_kind: kind,
            resource_id,
            bucket,
            units,
            level,
            owner,
            created_at_version: new_version,
        };
        self.storage
            .entry(key)
            .or_default()
            .insert(new_id, reservation);
        Ok(new_id)
    }

    /// Release a reservation by its id. Returns `Ok(())` whether or not
    /// the id was found (idempotent semantics).
    pub fn release(&self, id: ReservationId) -> LedgerResult<()> {
        // Linear scan over DashMap shards. For v1 this is acceptable;
        // an index `ReservationId -> ResourceKey` is a future optimisation.
        let mut found = false;
        for mut entry in self.storage.iter_mut() {
            if entry.value().contains_key(&id) {
                entry.value_mut().remove(&id);
                found = true;
                break;
            }
        }
        if found {
            self.version.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Promote a reservation to a strictly higher commitment level.
    /// Capacity at the new level must be sufficient (the promotion adds
    /// no new units but moves units from a lower-priority pool to a
    /// higher one).
    ///
    /// Forecast → Soft and Forecast → Hard require capacity at the
    /// `Soft` admissibility view (because the units now count as
    /// committed). Soft → Hard requires no extra capacity check (the
    /// units already count at the Soft view).
    pub fn promote(
        &self,
        id: ReservationId,
        new_level: CommitmentLevel,
    ) -> LedgerResult<()> {
        // Find the reservation and its key.
        let key_opt = self
            .storage
            .iter()
            .find_map(|entry| {
                if entry.value().contains_key(&id) {
                    Some(*entry.key())
                } else {
                    None
                }
            });
        let key = key_opt.ok_or(LedgerError::ReservationNotFound(id))?;
        // Read current level under the shard lock.
        let current_level = {
            let inner = self.storage.get(&key).ok_or(LedgerError::ReservationNotFound(id))?;
            inner
                .get(&id)
                .map(|r| r.level)
                .ok_or(LedgerError::ReservationNotFound(id))?
        };
        if new_level <= current_level {
            return Err(LedgerError::InvalidPromotion {
                from: current_level,
                to: new_level,
            });
        }
        // If promoting Forecast → Soft/Hard, units now count at Soft view.
        // Check that the increment fits.
        if current_level == CommitmentLevel::Forecast {
            let total = self.total_capacity(key.kind, key.resource_id, key.bucket);
            let booked_soft_excluding_this = {
                let inner = self.storage.get(&key);
                inner
                    .map(|i| {
                        i.iter()
                            .filter(|(rid, r)| **rid != id && r.level.counts_at(CommitmentLevel::Soft))
                            .map(|(_, r)| r.units)
                            .sum::<u64>()
                    })
                    .unwrap_or(0)
            };
            let units = {
                let inner = self.storage.get(&key).ok_or(LedgerError::ReservationNotFound(id))?;
                inner
                    .get(&id)
                    .map(|r| r.units)
                    .ok_or(LedgerError::ReservationNotFound(id))?
            };
            let free = total.saturating_sub(booked_soft_excluding_this);
            if free < units {
                return Err(LedgerError::InsufficientCapacity {
                    kind: key.kind,
                    id: key.resource_id,
                    bucket: key.bucket,
                    requested: units,
                    free,
                });
            }
        }
        // Apply the promotion.
        if let Some(mut entry) = self.storage.get_mut(&key) {
            if let Some(r) = entry.get_mut(&id) {
                r.level = new_level;
            } else {
                return Err(LedgerError::ReservationNotFound(id));
            }
        } else {
            return Err(LedgerError::ReservationNotFound(id));
        }
        self.version.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Downgrade a reservation's commitment level. Strictly downward
    /// (Hard → Soft → Forecast). Use `promote` for upward moves.
    ///
    /// Downgrade always frees capacity (the units move to a less-binding
    /// level), so no admissibility check is needed. The version counter
    /// still increments to invalidate caches.
    ///
    /// Added per B-D1 (Block 2 didactic review): the state machine's
    /// HoldByControl transition previously emitted a malformed Promote
    /// op with new_level=Soft, which Promote rejects because Promote
    /// only moves upward. Downgrade is the proper op for that case.
    pub fn downgrade(
        &self,
        id: ReservationId,
        new_level: CommitmentLevel,
    ) -> LedgerResult<()> {
        // Find the reservation and its key.
        let key_opt = self
            .storage
            .iter()
            .find_map(|entry| {
                if entry.value().contains_key(&id) {
                    Some(*entry.key())
                } else {
                    None
                }
            });
        let key = key_opt.ok_or(LedgerError::ReservationNotFound(id))?;
        // Read current level under the shard lock.
        let current_level = {
            let inner = self.storage.get(&key).ok_or(LedgerError::ReservationNotFound(id))?;
            inner
                .get(&id)
                .map(|r| r.level)
                .ok_or(LedgerError::ReservationNotFound(id))?
        };
        if new_level >= current_level {
            return Err(LedgerError::InvalidDowngrade {
                from: current_level,
                to: new_level,
            });
        }
        // Apply the downgrade — no capacity check needed (units move to
        // a less-binding level, freeing capacity at the stricter views).
        if let Some(mut entry) = self.storage.get_mut(&key) {
            if let Some(r) = entry.get_mut(&id) {
                r.level = new_level;
            } else {
                return Err(LedgerError::ReservationNotFound(id));
            }
        } else {
            return Err(LedgerError::ReservationNotFound(id));
        }
        self.version.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Reservations on a key at level `>= query`, ordered by id.
    pub fn reservations_for(
        &self,
        kind: ResourceKind,
        resource_id: ResourceId,
        bucket: TimeBucket,
        level: CommitmentLevel,
    ) -> Vec<Reservation> {
        let key = ResourceKey::new(kind, resource_id, bucket);
        self.storage
            .get(&key)
            .map(|inner| {
                inner
                    .iter()
                    .filter(|(_, r)| r.level.counts_at(level))
                    .map(|(_, r)| r.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// All reservations across the ledger, ordered by id (for snapshot).
    pub fn all_reservations(&self) -> Vec<Reservation> {
        let mut all: Vec<Reservation> = self
            .storage
            .iter()
            .flat_map(|entry| entry.value().values().cloned().collect::<Vec<_>>())
            .collect();
        all.sort_by_key(|r| r.id);
        all
    }

    /// Current monotonic version.
    pub fn version(&self) -> LedgerVersion {
        self.version.load(Ordering::SeqCst)
    }

    /// Number of reservations across all keys (diagnostic).
    pub fn n_reservations(&self) -> usize {
        self.storage.iter().map(|e| e.value().len()).sum()
    }
}
