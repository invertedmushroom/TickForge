use std::collections::HashMap;

use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_schema::{EntityKind, EntityState};

use super::entity_index::EntityIndex;

/// Dense entity lifecycle store with EntityId ↔ EntityIndex mapping.
///
/// Entities are assigned a stable `EntityIndex` on spawn. Component arrays
/// throughout `SimState` use this index for O(1) access. Removed entities
/// become tombstones — their index slot is kept but marked `Removed`, and
/// the `EntityId` mapping is dropped so future lookups return `None`.
pub struct EntityStore {
    id_to_index: HashMap<EntityId, EntityIndex>,
    index_to_id: Vec<EntityId>,
    pub kinds: Vec<EntityKind>,
    pub states: Vec<EntityState>,
    pub spawned_at: Vec<TickId>,
}

impl EntityStore {
    pub fn new() -> Self {
        Self {
            id_to_index: HashMap::new(),
            index_to_id: Vec::new(),
            kinds: Vec::new(),
            states: Vec::new(),
            spawned_at: Vec::new(),
        }
    }

    /// Allocate a new entity slot and return its dense index.
    pub fn spawn(&mut self, id: EntityId, kind: EntityKind, tick: TickId) -> EntityIndex {
        let idx = EntityIndex(self.index_to_id.len() as u32);
        self.index_to_id.push(id);
        self.kinds.push(kind);
        self.states.push(EntityState::Spawning);
        self.spawned_at.push(tick);
        self.id_to_index.insert(id, idx);
        idx
    }

    /// Resolve an `EntityId` to its dense index. Returns `None` for
    /// unknown or removed entities.
    #[inline]
    pub fn lookup(&self, id: EntityId) -> Option<EntityIndex> {
        self.id_to_index.get(&id).copied()
    }

    /// Check whether an entity is currently tracked (not removed).
    #[inline]
    pub fn contains(&self, id: EntityId) -> bool {
        self.id_to_index.contains_key(&id)
    }

    /// Get the `EntityId` for a given index.
    #[inline]
    pub fn id_of(&self, idx: EntityIndex) -> EntityId {
        self.index_to_id[idx.as_usize()]
    }

    /// Total number of slots (including tombstones).
    #[inline]
    pub fn len(&self) -> usize {
        self.index_to_id.len()
    }

    #[inline]
    pub fn is_active(&self, idx: EntityIndex) -> bool {
        self.states[idx.as_usize()] == EntityState::Active
    }

    /// Transition an entity from Spawning to Active.
    pub fn activate(&mut self, idx: EntityIndex) {
        self.states[idx.as_usize()] = EntityState::Active;
    }

    /// Mark an entity for removal at end of tick.
    pub fn mark_despawn(&mut self, idx: EntityIndex) {
        self.states[idx.as_usize()] = EntityState::DespawnPending;
    }

    /// Tombstone: mark as Removed and drop the id→index mapping.
    pub fn mark_removed(&mut self, idx: EntityIndex) {
        self.states[idx.as_usize()] = EntityState::Removed;
        let id = self.index_to_id[idx.as_usize()];
        self.id_to_index.remove(&id);
    }
}

impl Default for EntityStore {
    fn default() -> Self {
        Self::new()
    }
}
