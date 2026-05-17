use serde::{Deserialize, Serialize};

/// Stable entity identifier used across all systems.
///
/// This is the canonical ID that maps an entity across:
/// - SpacetimeDB tables (entity_id column)
/// - Rapier physics handles (via mapping tables)
/// - Client-side entity views
///
/// Using a newtype wrapper prevents mixing entity IDs with other u64 values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EntityId(pub u64);

impl EntityId {
    pub const INVALID: Self = Self(0);

    pub fn is_valid(self) -> bool {
        self.0 != 0
    }
}

impl std::fmt::Display for EntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Entity({})", self.0)
    }
}
