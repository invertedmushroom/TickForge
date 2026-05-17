//! Gameplay volumes — generic spatial regions for boss mechanics, puzzles,
//! water, checkpoints, cleanse pools, and arena activation.
//!
//! A `Volume` is an authored shape + layer + tags + occupancy state,
//! materialized into the simulation as a world sensor. Per tick the worker
//! syncs occupants from `physics.sensor_intersections` into each volume,
//! emitting `VolumeEnter` / `VolumeExit` events on edges. Different gameplay
//! systems (encounters, future puzzle/water systems) consume those events
//! and the per-tick occupant list.
//!
//! Step 2.5 keeps the surface minimal: spawn / despawn / list occupants /
//! enter-exit edges. Dwell-time triggers, satisfaction rules, and
//! data-driven authoring (volumes in dungeons.ron) are deliberately deferred.

use std::collections::HashMap;

use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_protocol::types::Vec3f;
use game_schema::entity::EntityKind;
use serde::{Deserialize, Serialize};

/// Stable identifier for a runtime volume. Allocated by `VolumeStore`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VolumeId(pub u64);

/// Authored shape for a volume. Mirrors the sphere/capsule split used by the
/// physics backend's `SensorShape`. The capsule variant is included for
/// upright cylindrical zones (e.g., totems, beam columns).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum VolumeShape {
    Sphere {
        radius: f32,
    },
    Capsule {
        half_height: f32,
        radius: f32,
    },
    /// Horizontal annulus centered on the volume position. Used by encounter
    /// mechanics where distance from an anchor matters more than a filled AoE.
    Ring {
        inner_radius: f32,
        outer_radius: f32,
        half_height: f32,
    },
}

/// Policy for which entities count as occupants of a [`Volume`].
///
/// Real mechanics need to filter occupancy by team/role: "3 players on safe
/// pad" must ignore the boss in its own arena, "boss exits enrage zone"
/// must ignore players. The filter is evaluated by the worker before
/// edge-detection, so `VolumeEnter` / `VolumeExit` events and
/// `OccupancyCmp` checks see only matching entities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EntityKindFilter {
    /// Any overlapping entity counts.
    Any,
    /// Only the listed kinds. Empty list = nothing matches.
    Kinds(Vec<EntityKind>),
    /// Any kind except the volume's owning entity (typically the boss).
    /// Excludes the owner from its own arena. Equivalent to `Any` when
    /// the volume has no owner.
    ExcludeOwner,
}

impl Default for EntityKindFilter {
    fn default() -> Self {
        EntityKindFilter::Any
    }
}

impl EntityKindFilter {
    /// Returns `true` if `(kind, entity)` should count as an occupant of a
    /// volume owned by `owner`.
    pub fn accepts(&self, kind: EntityKind, entity: EntityId, owner: Option<EntityId>) -> bool {
        match self {
            EntityKindFilter::Any => true,
            EntityKindFilter::Kinds(allowed) => allowed.iter().any(|k| *k == kind),
            EntityKindFilter::ExcludeOwner => match owner {
                Some(o) => o != entity,
                None => true,
            },
        }
    }
}

/// A live gameplay volume tracked by the simulation.
///
/// `sensor_handle` is the physics-backend handle returned by
/// `spawn_world_sensor`; `occupants` mirrors the latest
/// `sensor_intersections(handle)` result, sorted by `EntityId.0` for
/// deterministic iteration. `tag` is a free-form authoring label
/// (`"boss_arena"`, `"safe_zone"`, `"north_pad"`, etc.).
#[derive(Clone, Debug)]
pub struct Volume {
    pub id: VolumeId,
    pub tag: String,
    pub shape: VolumeShape,
    pub position: Vec3f,
    pub layer: u32,
    pub sensor_handle: u64,
    /// Tick when the volume was spawned. Used by future dwell-time logic.
    pub spawned_at: TickId,
    /// Optional auto-despawn tick. `None` = persistent until DespawnVolume.
    pub expires_at: Option<TickId>,
    /// Sorted list of currently-overlapping entities (post-filter).
    pub occupants: Vec<EntityId>,
    /// Owner entity (typically the boss this volume belongs to). Used
    /// by `EntityKindFilter::ExcludeOwner`. `None` for ownerless volumes.
    pub owner: Option<EntityId>,
    /// Which overlapping entities count as occupants.
    pub entity_filter: EntityKindFilter,
    /// When `true` the worker re-resolves `position` from the owner each
    /// tick during `phase_volume_sync` and moves the physics sensor.
    /// Volumes spawned with `VolumeAnchor::FollowBoss` set this to `true`.
    pub follow_owner: bool,
}

impl Volume {
    pub fn contains(&self, entity: EntityId) -> bool {
        self.occupants.binary_search(&entity).is_ok()
    }

    pub fn occupant_count(&self) -> usize {
        self.occupants.len()
    }
}

/// In-memory store of all live volumes. Owned by the tick pipeline.
pub struct VolumeStore {
    next_id: u64,
    volumes: HashMap<VolumeId, Volume>,
    /// Reverse index: tag → ids matching, in insertion order. A single tag
    /// may map to many volumes (e.g., per-player spread zones).
    by_tag: HashMap<String, Vec<VolumeId>>,
}

impl VolumeStore {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            volumes: HashMap::new(),
            by_tag: HashMap::new(),
        }
    }

    /// Allocate the next [`VolumeId`].
    pub fn allocate_id(&mut self) -> VolumeId {
        let id = VolumeId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Insert a new volume. Caller is responsible for spawning the underlying
    /// physics sensor and supplying its handle. Returns the volume's id.
    pub fn insert(&mut self, volume: Volume) -> VolumeId {
        let id = volume.id;
        self.by_tag.entry(volume.tag.clone()).or_default().push(id);
        self.volumes.insert(id, volume);
        id
    }

    /// Remove a volume by id. Returns the removed entry so the caller can
    /// despawn its physics sensor.
    pub fn remove(&mut self, id: VolumeId) -> Option<Volume> {
        let removed = self.volumes.remove(&id)?;
        if let Some(ids) = self.by_tag.get_mut(&removed.tag) {
            ids.retain(|x| *x != id);
            if ids.is_empty() {
                self.by_tag.remove(&removed.tag);
            }
        }
        Some(removed)
    }

    /// Remove all volumes matching `tag`. Returns the removed entries so the
    /// caller can despawn their physics sensors.
    pub fn remove_by_tag(&mut self, tag: &str) -> Vec<Volume> {
        let ids = self.by_tag.remove(tag).unwrap_or_default();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(v) = self.volumes.remove(&id) {
                out.push(v);
            }
        }
        out
    }

    pub fn get(&self, id: VolumeId) -> Option<&Volume> {
        self.volumes.get(&id)
    }

    pub fn get_mut(&mut self, id: VolumeId) -> Option<&mut Volume> {
        self.volumes.get_mut(&id)
    }

    /// Volume ids matching `tag` in insertion order.
    pub fn ids_with_tag(&self, tag: &str) -> &[VolumeId] {
        self.by_tag.get(tag).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// First volume matching `tag` (most-recently-inserted call sites usually
    /// want a unique tag and benefit from this shortcut).
    pub fn first_with_tag(&self, tag: &str) -> Option<&Volume> {
        self.by_tag
            .get(tag)
            .and_then(|ids| ids.first())
            .and_then(|id| self.volumes.get(id))
    }

    /// Iterate all live volumes in id order (deterministic).
    pub fn iter_sorted(&self) -> Vec<&Volume> {
        let mut ids: Vec<&VolumeId> = self.volumes.keys().collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| self.volumes.get(id))
            .collect()
    }

    /// Iterate ids in id order (deterministic). Preferred when callers need
    /// to mutate the store while iterating.
    pub fn ids_sorted(&self) -> Vec<VolumeId> {
        let mut ids: Vec<VolumeId> = self.volumes.keys().copied().collect();
        ids.sort();
        ids
    }

    pub fn len(&self) -> usize {
        self.volumes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.volumes.is_empty()
    }

    /// Replace a volume's occupant list and return the (entered, exited)
    /// edges relative to the previous list. Both inputs and outputs are
    /// sorted by `EntityId.0`. Used by the per-tick occupant sync.
    pub fn update_occupants(
        &mut self,
        id: VolumeId,
        mut new_occupants: Vec<EntityId>,
    ) -> Option<(Vec<EntityId>, Vec<EntityId>)> {
        new_occupants.sort_by_key(|e| e.0);
        new_occupants.dedup();
        let volume = self.volumes.get_mut(&id)?;
        let entered = diff_sorted(&new_occupants, &volume.occupants);
        let exited = diff_sorted(&volume.occupants, &new_occupants);
        volume.occupants = new_occupants;
        Some((entered, exited))
    }
}

impl Default for VolumeStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Set difference for two sorted-unique slices: returns elements in `a` not in `b`.
fn diff_sorted(a: &[EntityId], b: &[EntityId]) -> Vec<EntityId> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].0.cmp(&b[j].0) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    while i < a.len() {
        out.push(a[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(id: VolumeId, tag: &str) -> Volume {
        Volume {
            id,
            tag: tag.to_string(),
            shape: VolumeShape::Sphere { radius: 5.0 },
            position: Vec3f::ZERO,
            layer: 0,
            sensor_handle: 0,
            spawned_at: TickId(0),
            expires_at: None,
            occupants: Vec::new(),
            owner: None,
            entity_filter: EntityKindFilter::Any,
            follow_owner: false,
        }
    }

    #[test]
    fn insert_and_get_by_id_and_tag() {
        let mut store = VolumeStore::new();
        let id = store.allocate_id();
        store.insert(vol(id, "north_pad"));
        assert_eq!(store.len(), 1);
        assert!(store.get(id).is_some());
        assert_eq!(store.ids_with_tag("north_pad"), &[id]);
        assert!(store.first_with_tag("north_pad").is_some());
        assert!(store.first_with_tag("missing").is_none());
    }

    #[test]
    fn remove_clears_tag_index() {
        let mut store = VolumeStore::new();
        let id = store.allocate_id();
        store.insert(vol(id, "north_pad"));
        let removed = store.remove(id).expect("removed");
        assert_eq!(removed.tag, "north_pad");
        assert!(store.ids_with_tag("north_pad").is_empty());
        assert!(store.is_empty());
    }

    #[test]
    fn multiple_volumes_share_tag() {
        let mut store = VolumeStore::new();
        let a = store.allocate_id();
        let b = store.allocate_id();
        store.insert(vol(a, "spread"));
        store.insert(vol(b, "spread"));
        assert_eq!(store.ids_with_tag("spread"), &[a, b]);
        let removed = store.remove_by_tag("spread");
        assert_eq!(removed.len(), 2);
        assert!(store.is_empty());
    }

    #[test]
    fn update_occupants_emits_correct_edges() {
        let mut store = VolumeStore::new();
        let id = store.allocate_id();
        store.insert(vol(id, "arena"));

        // Initial enter: 1, 3, 5.
        let (entered, exited) = store
            .update_occupants(id, vec![EntityId(5), EntityId(1), EntityId(3)])
            .expect("present");
        assert_eq!(entered, vec![EntityId(1), EntityId(3), EntityId(5)]);
        assert!(exited.is_empty());

        // 1 leaves, 7 joins.
        let (entered, exited) = store
            .update_occupants(id, vec![EntityId(3), EntityId(7), EntityId(5)])
            .expect("present");
        assert_eq!(entered, vec![EntityId(7)]);
        assert_eq!(exited, vec![EntityId(1)]);

        // No change.
        let (entered, exited) = store
            .update_occupants(id, vec![EntityId(7), EntityId(3), EntityId(5)])
            .expect("present");
        assert!(entered.is_empty());
        assert!(exited.is_empty());

        // Duplicates in input are deduped.
        let (entered, exited) = store
            .update_occupants(id, vec![EntityId(7), EntityId(3), EntityId(5), EntityId(7)])
            .expect("present");
        assert!(entered.is_empty());
        assert!(exited.is_empty());

        // All leave.
        let (entered, exited) = store.update_occupants(id, Vec::new()).expect("present");
        assert!(entered.is_empty());
        assert_eq!(exited, vec![EntityId(3), EntityId(5), EntityId(7)]);
    }

    #[test]
    fn contains_uses_binary_search() {
        let mut v = vol(VolumeId(1), "x");
        v.occupants = vec![EntityId(2), EntityId(4), EntityId(6)];
        assert!(v.contains(EntityId(4)));
        assert!(!v.contains(EntityId(3)));
    }
}
