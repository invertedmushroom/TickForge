use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use serde::{Deserialize, Serialize};

/// Skill shape taxonomy per spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillShape {
    /// Point-blank AoE, auras.
    Sphere,
    /// Frontal attacks, blocks.
    Cone,
    /// Melee slashes.
    CapsuleSweep,
    /// Ranged skills.
    Projectile,
    /// Line-based attacks.
    LineSweep,
    /// Persistent ground effects.
    HazardZone,
}

/// Ability timing model — TERA-style frame windows.
///
/// Each ability defines explicit frame windows:
/// - startup: cast animation, can be interrupted
/// - active: hit window open, hitbox spawned
/// - recovery: animation recovery, vulnerable
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AbilityTimeline {
    pub ability_id: u32,
    pub actions: Vec<ScheduledAbilityAction>,
}

/// A single timed action within an ability's timeline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduledAbilityAction {
    /// Tick offset from ability start.
    pub tick_offset: u32,
    pub action: AbilityAction,
}

/// Actions that occur at specific frames during an ability.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AbilityAction {
    SpawnHitbox { shape: SkillShape },
    ApplyDamageFrame,
    RemoveHitbox,
    CooldownStart { duration_ticks: u32 },
}

/// Per-entity scheduled action for the ability scheduler.
///
/// The simulation tick consumes due actions each frame, enabling
/// clean overlap of cast windows, hit frames, and cooldown expirations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduledAction {
    pub tick_id: TickId,
    pub entity: EntityId,
    pub action_type: ScheduledActionType,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ScheduledActionType {
    AbilityFrame { ability_id: u32, action: AbilityAction },
    BuffExpire { buff_id: u32 },
    CooldownExpire { ability_id: u32 },
}
