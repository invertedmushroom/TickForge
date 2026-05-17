use std::collections::{HashMap, HashSet};

use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;

/// Unique key for an active hitbox: (owner entity, ability id).
pub type HitboxKey = (EntityId, u32);

/// Runtime record for a currently active hitbox collider.
///
/// Created when an ability timeline fires `SpawnHitbox` and removed
/// on `RemoveHitbox` (or when the owning entity despawns).
/// The `already_hit` set prevents the same hitbox from damaging a
/// target more than once per activation window.
#[derive(Clone, Debug)]
pub struct ActiveHitbox {
    /// Entity that owns this hitbox.
    pub owner: EntityId,
    /// Ability that spawned it.
    pub ability_id: u32,
    /// Tick when the hitbox was created.
    pub spawned_at: TickId,
    /// Entities already hit by this hitbox instance (single-hit dedup).
    pub already_hit: HashSet<EntityId>,
}

/// Tracks all active hitbox colliders in the simulation.
///
/// Keyed by `(owner, ability_id)` — an entity can have at most one
/// active hitbox per ability at a time. If an ability needs multiple
/// simultaneous hitboxes, each gets a distinct synthetic ability_id.
#[derive(Clone, Debug, Default)]
pub struct HitboxStore {
    active: HashMap<HitboxKey, ActiveHitbox>,
}

impl HitboxStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new active hitbox.
    /// Replaces any existing hitbox for the same (owner, ability_id).
    pub fn spawn(&mut self, owner: EntityId, ability_id: u32, tick: TickId) {
        self.active.insert((owner, ability_id), ActiveHitbox {
            owner,
            ability_id,
            spawned_at: tick,
            already_hit: HashSet::new(),
        });
    }

    /// Remove a hitbox. Returns true if it existed.
    pub fn remove(&mut self, owner: EntityId, ability_id: u32) -> bool {
        self.active.remove(&(owner, ability_id)).is_some()
    }

    /// Look up an active hitbox.
    pub fn get(&self, owner: EntityId, ability_id: u32) -> Option<&ActiveHitbox> {
        self.active.get(&(owner, ability_id))
    }

    /// Look up an active hitbox mutably.
    pub fn get_mut(&mut self, owner: EntityId, ability_id: u32) -> Option<&mut ActiveHitbox> {
        self.active.get_mut(&(owner, ability_id))
    }

    /// Check whether this hitbox has already hit a given target.
    pub fn has_hit(&self, owner: EntityId, ability_id: u32, target: EntityId) -> bool {
        self.active
            .get(&(owner, ability_id))
            .map_or(false, |hb| hb.already_hit.contains(&target))
    }

    /// Record that this hitbox hit a target.
    /// Returns false if the hitbox doesn't exist or the target was already recorded.
    pub fn record_hit(&mut self, owner: EntityId, ability_id: u32, target: EntityId) -> bool {
        match self.active.get_mut(&(owner, ability_id)) {
            Some(hb) => hb.already_hit.insert(target),
            None => false,
        }
    }

    /// Remove all hitboxes owned by an entity. Returns the ability ids that were active.
    pub fn remove_all_for_entity(&mut self, entity_id: EntityId) -> Vec<u32> {
        let keys: Vec<HitboxKey> = self.active
            .keys()
            .filter(|(owner, _)| *owner == entity_id)
            .copied()
            .collect();
        keys.iter().map(|(_, aid)| {
            self.active.remove(&(entity_id, *aid));
            *aid
        }).collect()
    }

    /// Iterate all active hitboxes for a given entity.
    pub fn hitboxes_for(&self, entity_id: EntityId) -> Vec<&ActiveHitbox> {
        self.active
            .values()
            .filter(|hb| hb.owner == entity_id)
            .collect()
    }

    /// Total number of active hitboxes.
    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eid(n: u64) -> EntityId {
        EntityId(n)
    }

    #[test]
    fn spawn_and_remove() {
        let mut store = HitboxStore::new();
        store.spawn(eid(1), 100, TickId(5));
        assert_eq!(store.len(), 1);
        assert!(store.get(eid(1), 100).is_some());

        assert!(store.remove(eid(1), 100));
        assert!(store.is_empty());
        assert!(!store.remove(eid(1), 100)); // already gone
    }

    #[test]
    fn hit_dedup() {
        let mut store = HitboxStore::new();
        store.spawn(eid(1), 42, TickId(0));

        assert!(!store.has_hit(eid(1), 42, eid(2)));
        assert!(store.record_hit(eid(1), 42, eid(2))); // first hit → true
        assert!(store.has_hit(eid(1), 42, eid(2)));
        assert!(!store.record_hit(eid(1), 42, eid(2))); // duplicate → false
    }

    #[test]
    fn remove_all_for_entity() {
        let mut store = HitboxStore::new();
        store.spawn(eid(1), 10, TickId(0));
        store.spawn(eid(1), 20, TickId(1));
        store.spawn(eid(2), 10, TickId(0));

        let mut removed = store.remove_all_for_entity(eid(1));
        removed.sort();
        assert_eq!(removed, vec![10, 20]);
        assert_eq!(store.len(), 1);
        assert!(store.get(eid(2), 10).is_some());
    }

    #[test]
    fn spawn_replaces_existing() {
        let mut store = HitboxStore::new();
        store.spawn(eid(1), 10, TickId(0));
        store.record_hit(eid(1), 10, eid(5));
        assert!(store.has_hit(eid(1), 10, eid(5)));

        // Re-spawn resets already_hit
        store.spawn(eid(1), 10, TickId(3));
        assert!(!store.has_hit(eid(1), 10, eid(5)));
        assert_eq!(store.get(eid(1), 10).unwrap().spawned_at, TickId(3));
    }

    #[test]
    fn record_hit_on_missing_hitbox() {
        let mut store = HitboxStore::new();
        assert!(!store.record_hit(eid(1), 99, eid(2)));
    }
}
