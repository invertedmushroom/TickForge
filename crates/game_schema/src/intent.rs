use serde::{Deserialize, Serialize};

use crate::types::Vec3f;

/// Direction vector for movement intents.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub struct MoveDir {
    pub dir_x: f32,
    pub dir_y: f32,
    pub dir_z: f32,
}

/// Ability use data for intent actions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub struct UseAbilityData {
    pub ability_id: u32,
    pub target: AbilityTarget,
}

/// The action a player wants to perform.
///
/// Uses tuple variants with wrapper structs for SpacetimeDB `SpacetimeType`
/// compatibility. Both native and WASM crates share this exact definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub enum IntentAction {
    /// Move in a direction (normalized).
    Move(MoveDir),
    /// Use an ability on a target.
    UseAbility(UseAbilityData),
    /// Stop movement.
    Stop,
    /// Face a direction without moving.
    FaceTo(MoveDir),
    /// Interact with a world object (entity_id as u64).
    Interact(u64),
    /// Hold block stance this tick.
    Block,
    /// Release a charging ability. Sent when the player releases the hold key.
    /// The server resolves the achieved charge tier from elapsed ticks.
    ReleaseAbility(u32),
}

/// Target specification for abilities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub enum AbilityTarget {
    /// No explicit target (self-cast, PBAoE).
    None,
    /// Target a specific entity (entity_id as u64).
    Entity(u64),
    /// Target a world position.
    Position(Vec3f),
    /// Target a direction (cone, line).
    Direction(Vec3f),
}
