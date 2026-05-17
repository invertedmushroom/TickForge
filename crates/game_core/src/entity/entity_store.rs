use std::collections::HashMap;

use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_schema::{EntityKind, EntityState};

use super::entity_index::EntityIndex;

/// Dense entity lifecycle store with EntityId ↔ EntityIndex mapping.
///
/// Uses generational indexing: each slot carries a generation counter that
/// is incremented when the slot is reused after an entity is removed.
/// Stale `EntityIndex` values from a previous generation will not match
/// the current generation and are safely rejected.
///
/// Removed entity slots are pushed to a free list and recycled by the
/// next `spawn()` call. This keeps the dense arrays bounded by the number
/// of *concurrent* entities rather than growing with total lifetime churn.
pub struct EntityStore {
    id_to_index: HashMap<EntityId, EntityIndex>,
    index_to_id: Vec<EntityId>,
    pub kinds: Vec<EntityKind>,
    pub states: Vec<EntityState>,
    pub spawned_at: Vec<TickId>,
    generations: Vec<u32>,
    free_list: Vec<u32>,
}

impl EntityStore {
    pub fn new() -> Self {
        Self {
            id_to_index: HashMap::new(),
            index_to_id: Vec::new(),
            kinds: Vec::new(),
            states: Vec::new(),
            spawned_at: Vec::new(),
            generations: Vec::new(),
            free_list: Vec::new(),
        }
    }

    /// Allocate an entity slot (reusing a free slot if available) and return
    /// its dense index. The second element is `true` if a slot was reused.
    pub fn spawn(&mut self, id: EntityId, kind: EntityKind, tick: TickId) -> (EntityIndex, bool) {
        if let Some(slot) = self.free_list.pop() {
            let s = slot as usize;
            let g = self.generations[s] + 1;
            self.generations[s] = g;
            self.index_to_id[s] = id;
            self.kinds[s] = kind;
            self.states[s] = EntityState::Spawning;
            self.spawned_at[s] = tick;
            let idx = EntityIndex::new(slot, g);
            self.id_to_index.insert(id, idx);
            (idx, true)
        } else {
            let slot = self.index_to_id.len() as u32;
            let g = 0;
            self.generations.push(g);
            self.index_to_id.push(id);
            self.kinds.push(kind);
            self.states.push(EntityState::Spawning);
            self.spawned_at.push(tick);
            let idx = EntityIndex::new(slot, g);
            self.id_to_index.insert(id, idx);
            (idx, false)
        }
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

    /// Get the `EntityId` at a raw slot index, if the slot is active.
    ///
    /// Used by systems that iterate dense component arrays (e.g. tactical)
    /// and need to resolve the owning entity without an `EntityIndex`.
    #[inline]
    pub fn lookup_by_slot(&self, slot: usize) -> Option<EntityId> {
        if slot < self.states.len() && self.states[slot] == EntityState::Active {
            Some(self.index_to_id[slot])
        } else {
            None
        }
    }

    /// Total number of slots (including tombstones).
    #[inline]
    pub fn len(&self) -> usize {
        self.index_to_id.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.index_to_id.is_empty()
    }

    #[inline]
    pub fn is_active(&self, idx: EntityIndex) -> bool {
        self.states[idx.as_usize()] == EntityState::Active
    }

    /// Transition an entity from Spawning to Active.
    pub fn activate(&mut self, idx: EntityIndex) {
        let slot = idx.as_usize();
        debug_assert_eq!(
            self.generations[slot],
            idx.generation(),
            "activate called with stale EntityIndex (slot={} gen={} current={})",
            slot,
            idx.generation(),
            self.generations[slot],
        );
        if self.generations[slot] != idx.generation() {
            return;
        }
        self.states[slot] = EntityState::Active;
    }

    /// Mark an entity for removal at end of tick.
    pub fn mark_despawn(&mut self, idx: EntityIndex) {
        let slot = idx.as_usize();
        debug_assert_eq!(
            self.generations[slot],
            idx.generation(),
            "mark_despawn called with stale EntityIndex (slot={} gen={} current={})",
            slot,
            idx.generation(),
            self.generations[slot],
        );
        if self.generations[slot] != idx.generation() {
            return;
        }
        self.states[slot] = EntityState::DespawnPending;
    }

    /// Tombstone: mark as Removed, drop the id→index mapping, and return
    /// the slot to the free list for reuse.
    pub fn mark_removed(&mut self, idx: EntityIndex) {
        let slot = idx.as_usize();
        debug_assert_eq!(
            self.generations[slot],
            idx.generation(),
            "mark_removed called with stale EntityIndex (slot={} gen={} current={})",
            slot,
            idx.generation(),
            self.generations[slot],
        );
        if self.generations[slot] != idx.generation() {
            return;
        }
        self.states[slot] = EntityState::Removed;
        let id = self.index_to_id[slot];
        self.id_to_index.remove(&id);
        self.free_list.push(slot as u32);
    }

    /// Build a generation-correct `EntityIndex` for a raw slot offset.
    /// Used by internal iteration loops that scan `0..len()`.
    #[inline]
    pub fn index_at(&self, slot: usize) -> EntityIndex {
        EntityIndex::new(slot as u32, self.generations[slot])
    }
}

impl Default for EntityStore {
    fn default() -> Self {
        Self::new()
    }
}
