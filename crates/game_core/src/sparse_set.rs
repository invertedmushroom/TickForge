use crate::entity::entity_index::EntityIndex;

/// Cache-friendly sparse set for optional ECS components.
///
/// Components are stored contiguously in `dense` for fast O(N) iteration.
/// Provides O(1) lookup, insert, and remove via a sparse indirection layer.
///
/// Use this for components that only a subset of entities carry (e.g.
/// `ThreatTable` on NPCs/Bosses, `NpcAiState` on AI entities). Components
/// that nearly every entity has (health, transform) should remain as dense
/// parallel arrays.
pub struct SparseSet<T> {
    /// Contiguous component data — iterated tightly in phase loops.
    dense: Vec<T>,
    /// Maps dense index → EntityIndex (reverse lookup for iteration).
    dense_to_entity: Vec<EntityIndex>,
    /// Maps EntityIndex → dense index. Length equals total spawned entities.
    entity_to_dense: Vec<Option<usize>>,
}

impl<T> SparseSet<T> {
    pub fn new() -> Self {
        Self {
            dense: Vec::new(),
            dense_to_entity: Vec::new(),
            entity_to_dense: Vec::new(),
        }
    }

    /// Extend the sparse array to accommodate a new entity slot.
    /// Must be called for every spawned entity to keep the sparse array
    /// in sync with EntityStore, even if this entity won't carry the component.
    #[inline]
    pub fn push_slot(&mut self) {
        self.entity_to_dense.push(None);
    }

    /// Insert a component for an entity. Panics if the entity already has one.
    pub fn insert(&mut self, idx: EntityIndex, value: T) {
        let i = idx.as_usize();
        debug_assert!(
            i < self.entity_to_dense.len(),
            "push_slot not called for {idx}"
        );
        debug_assert!(
            self.entity_to_dense[i].is_none(),
            "duplicate insert for {idx}"
        );
        let dense_idx = self.dense.len();
        self.dense.push(value);
        self.dense_to_entity.push(idx);
        self.entity_to_dense[i] = Some(dense_idx);
    }

    /// Remove the component for an entity (swap-remove). Returns the removed
    /// value, or `None` if the entity didn't have one.
    pub fn remove(&mut self, idx: EntityIndex) -> Option<T> {
        let i = idx.as_usize();
        let dense_idx = self.entity_to_dense.get_mut(i)?.take()?;

        // Validate generation to prevent stale indices from removing a
        // recycled entity's component.
        if self.dense_to_entity[dense_idx] != idx {
            // Restore the mapping we just cleared — it belongs to a
            // different generation.
            self.entity_to_dense[i] = Some(dense_idx);
            return None;
        }

        // Swap-remove from dense arrays to keep them contiguous.
        let value = self.dense.swap_remove(dense_idx);
        self.dense_to_entity.swap_remove(dense_idx);

        // If we swapped a different element into `dense_idx`, fix its mapping.
        if dense_idx < self.dense.len() {
            let moved_entity = self.dense_to_entity[dense_idx];
            self.entity_to_dense[moved_entity.as_usize()] = Some(dense_idx);
        }

        Some(value)
    }

    /// O(1) immutable lookup by entity index.
    ///
    /// Returns `None` if `idx` refers to a stale generation (the slot was
    /// recycled for a different entity after the caller obtained `idx`).
    #[inline]
    pub fn get(&self, idx: EntityIndex) -> Option<&T> {
        let dense_idx = *self.entity_to_dense.get(idx.as_usize())?.as_ref()?;
        if self.dense_to_entity[dense_idx] != idx {
            return None;
        }
        Some(&self.dense[dense_idx])
    }

    /// O(1) mutable lookup by entity index.
    ///
    /// Returns `None` if `idx` refers to a stale generation.
    #[inline]
    pub fn get_mut(&mut self, idx: EntityIndex) -> Option<&mut T> {
        let dense_idx = *self.entity_to_dense.get(idx.as_usize())?.as_ref()?;
        if self.dense_to_entity[dense_idx] != idx {
            return None;
        }
        Some(&mut self.dense[dense_idx])
    }

    /// Check whether an entity has this component.
    ///
    /// Returns `false` for stale-generation indices.
    #[inline]
    pub fn contains(&self, idx: EntityIndex) -> bool {
        if let Some(&Some(dense_idx)) = self.entity_to_dense.get(idx.as_usize()) {
            self.dense_to_entity[dense_idx] == idx
        } else {
            false
        }
    }

    /// Number of entities that carry this component (dense length).
    #[inline]
    pub fn len(&self) -> usize {
        self.dense.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.dense.is_empty()
    }

    /// Number of entity slots allocated in the sparse array.
    #[inline]
    pub fn sparse_len(&self) -> usize {
        self.entity_to_dense.len()
    }

    /// Iterate over all (EntityIndex, &T) pairs. The dense array is contiguous
    /// so this is cache-friendly for the component data.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (EntityIndex, &T)> {
        self.dense_to_entity.iter().copied().zip(self.dense.iter())
    }

    /// Mutable iteration over all (EntityIndex, &mut T) pairs.
    #[inline]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (EntityIndex, &mut T)> {
        self.dense_to_entity
            .iter()
            .copied()
            .zip(self.dense.iter_mut())
    }

    /// Iterate over only the dense component data (no entity indices).
    #[inline]
    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.dense.iter()
    }

    /// Mutable iteration over only the dense component data.
    #[inline]
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.dense.iter_mut()
    }

    /// Iterate over entity indices that carry this component.
    #[inline]
    pub fn entities(&self) -> impl Iterator<Item = EntityIndex> + '_ {
        self.dense_to_entity.iter().copied()
    }
}

impl<T> Default for SparseSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(n: u32) -> EntityIndex {
        EntityIndex::dangling(n)
    }

    #[test]
    fn insert_and_get() {
        let mut set = SparseSet::new();
        set.push_slot(); // entity 0
        set.push_slot(); // entity 1
        set.push_slot(); // entity 2

        set.insert(idx(1), 42i32);
        assert_eq!(set.get(idx(0)), None);
        assert_eq!(set.get(idx(1)), Some(&42));
        assert_eq!(set.get(idx(2)), None);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn remove_and_swap() {
        let mut set = SparseSet::new();
        for _ in 0..4 {
            set.push_slot();
        }
        set.insert(idx(0), "a");
        set.insert(idx(2), "b");
        set.insert(idx(3), "c");
        assert_eq!(set.len(), 3);

        // Remove middle element — last element swaps into its slot.
        let removed = set.remove(idx(2));
        assert_eq!(removed, Some("b"));
        assert_eq!(set.len(), 2);
        assert_eq!(set.get(idx(2)), None);

        // Remaining elements intact.
        assert_eq!(set.get(idx(0)), Some(&"a"));
        assert_eq!(set.get(idx(3)), Some(&"c"));
    }

    #[test]
    fn remove_last_element() {
        let mut set = SparseSet::new();
        set.push_slot();
        set.insert(idx(0), 99);
        assert_eq!(set.remove(idx(0)), Some(99));
        assert!(set.is_empty());
        assert_eq!(set.get(idx(0)), None);
    }

    #[test]
    fn remove_nonexistent_returns_none() {
        let mut set: SparseSet<i32> = SparseSet::new();
        set.push_slot();
        assert_eq!(set.remove(idx(0)), None);
    }

    #[test]
    fn iter_is_dense_and_complete() {
        let mut set = SparseSet::new();
        for _ in 0..5 {
            set.push_slot();
        }
        set.insert(idx(1), 10);
        set.insert(idx(3), 30);
        set.insert(idx(4), 40);

        let mut collected: Vec<_> = set.iter().collect();
        collected.sort_by_key(|(idx, _)| *idx);
        assert_eq!(collected, vec![(idx(1), &10), (idx(3), &30), (idx(4), &40)]);
    }

    #[test]
    fn iter_mut_allows_modification() {
        let mut set = SparseSet::new();
        set.push_slot();
        set.push_slot();
        set.insert(idx(0), 1);
        set.insert(idx(1), 2);
        for (_, val) in set.iter_mut() {
            *val *= 10;
        }
        assert_eq!(set.get(idx(0)), Some(&10));
        assert_eq!(set.get(idx(1)), Some(&20));
    }

    #[test]
    fn contains_tracks_presence() {
        let mut set: SparseSet<i32> = SparseSet::new();
        set.push_slot();
        assert!(!set.contains(idx(0)));
        set.insert(idx(0), 1);
        assert!(set.contains(idx(0)));
        set.remove(idx(0));
        assert!(!set.contains(idx(0)));
    }

    #[test]
    fn sparse_len_tracks_slots() {
        let mut set: SparseSet<i32> = SparseSet::new();
        assert_eq!(set.sparse_len(), 0);
        set.push_slot();
        set.push_slot();
        assert_eq!(set.sparse_len(), 2);
        assert_eq!(set.len(), 0); // no components inserted
    }

    #[test]
    fn stress_insert_remove_reinsert() {
        let mut set = SparseSet::new();
        for _ in 0..100 {
            set.push_slot();
        }
        // Insert even indices.
        for i in (0..100).step_by(2) {
            set.insert(idx(i), i * 10);
        }
        assert_eq!(set.len(), 50);
        // Remove every fourth.
        for i in (0..100).step_by(4) {
            set.remove(idx(i));
        }
        assert_eq!(set.len(), 25);
        // Reinsert removed slots with new values.
        for i in (0..100).step_by(4) {
            set.insert(idx(i), i * 100);
        }
        assert_eq!(set.len(), 50);
        // Verify all values.
        for i in (0..100u32).step_by(2) {
            let expected = if i % 4 == 0 { i * 100 } else { i * 10 };
            assert_eq!(set.get(idx(i)), Some(&expected));
        }
    }
}
