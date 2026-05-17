use serde::{Deserialize, Serialize};

/// What kind of entity this is — determines which systems process it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub enum EntityKind {
    Player,
    Npc,
    Projectile,
    Hazard,
    Boss,
    Prop,
}

/// Entity lifecycle state in the simulation.
///
/// Entities progress through these states:
/// Spawning → Active → DespawnPending → Removed
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub enum EntityState {
    /// Entity is being initialized (physics body creation, etc).
    Spawning,
    /// Entity is fully active in the simulation.
    Active,
    /// Entity is marked for removal at end of tick.
    DespawnPending,
    /// Physics handle freed, DB row updated to Removed (not deleted) so clients
    /// receive the terminal state event. Terminal state.
    Removed,
}

/// NPC AI behavioral state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub enum NpcAiState {
    Idle,
    Patrol,
    Combat,
    Flee,
    Scripted,
}
