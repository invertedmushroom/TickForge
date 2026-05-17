/// Dense array index for entity component storage.
///
/// Inside the tick pipeline, all component arrays (health, buffs, threat,
/// etc.) are indexed by `EntityIndex` instead of hashing `EntityId`.
/// The mapping `EntityId → EntityIndex` is resolved once at entity creation;
/// all subsequent per-tick access uses the index directly.
///
/// Generational: each index carries a generation counter that is incremented
/// when a slot is reused after an entity is removed. This prevents stale
/// indices from silently accessing a different entity's data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityIndex {
    slot: u32,
    generation: u32,
}

impl EntityIndex {
    /// Create a new EntityIndex. Should only be called by EntityStore.
    #[inline]
    pub(crate) fn new(slot: u32, generation: u32) -> Self {
        Self { slot, generation }
    }

    /// Raw slot offset for indexing into component arrays.
    #[inline]
    pub fn as_usize(self) -> usize {
        self.slot as usize
    }

    /// Generation counter for stale-index detection.
    #[inline]
    pub fn generation(self) -> u32 {
        self.generation
    }

    /// Test-only constructor for unit tests that need raw indices without
    /// going through EntityStore. Generation defaults to 0.
    #[cfg(test)]
    #[inline]
    pub fn dangling(slot: u32) -> Self {
        Self { slot, generation: 0 }
    }
}

impl std::fmt::Display for EntityIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Idx({}g{})", self.slot, self.generation)
    }
}
