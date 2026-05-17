/// Dense array index for entity component storage.
///
/// Inside the tick pipeline, all component arrays (health, buffs, threat,
/// etc.) are indexed by `EntityIndex` instead of hashing `EntityId`.
/// The mapping `EntityId → EntityIndex` is resolved once at entity creation;
/// all subsequent per-tick access uses the index directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityIndex(pub u32);

impl EntityIndex {
    #[inline]
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }
}

impl std::fmt::Display for EntityIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Idx({})", self.0)
    }
}
