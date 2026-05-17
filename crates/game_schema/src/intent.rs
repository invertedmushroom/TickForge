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
    /// Client-side soft-lock hint: the entity the crosshair was over when the
    /// player pressed the ability key. The server uses this as a tie-breaker
    /// for aim-assist cone checks, never as an authoritative override.
    /// `None` = pure direction aim or self-cast.
    pub target_hint: Option<u64>,
}

/// Block input payload.
///
/// Carries the player's intended look direction for this block tick so the
/// server can rotate the blocker internally without a separate `FaceTo` intent.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub struct BlockData {
    pub look_dir: MoveDir,
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
    /// Hold block stance this tick and face `look_dir`.
    Block(BlockData),
    /// Release a charging ability. Sent when the player releases the hold key.
    /// The server resolves the achieved charge tier from elapsed ticks.
    ReleaseAbility(u32),
    /// Jump. Server only accepts this when the entity is grounded and not
    /// ability-rooted. Horizontal direction during the jump is controlled by
    /// concurrent `Move` intents — the arc's XZ velocity is updated each tick
    /// the player holds a movement key (GW2-style free air directional control).
    Jump,
    /// Toggle active weapon set (0↔1). Rejected while stunned, mid-cast,
    /// or on weapon swap cooldown.
    WeaponSwap,
    /// Tag an entity for a TERA-style lock-on ability.
    ///
    /// Only valid when the player has an active lock-on session (started by
    /// `UseAbility` for a `TargetingMode::LockOn` ability). The server validates
    /// range, LoS, and max-target count; invalid tags are silently dropped.
    /// Emits `LockOnWarning` to the tagged entity on success.
    TagTarget(u64),
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
