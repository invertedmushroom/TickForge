use std::collections::HashMap;

use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_schema::DamageType;
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
    /// Spawn a sensor hitbox on the entity.
    ///
    /// `offset` is in entity-local space (forward = +Z). Use Vec3f::ZERO for
    /// centred effects; use a forward offset for melee/directional attacks.
    SpawnHitbox { shape: SkillShape, offset: game_schema::Vec3f },
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

/// Static definition of an ability — damage, shape, timing metadata.
///
/// Abilities are data-driven: the tick pipeline looks up `AbilityData`
/// by `ability_id` to know how much damage a hitbox deals, what damage
/// type it is, and what shape to spawn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AbilityData {
    pub ability_id: u32,
    pub name: String,
    pub base_damage: f32,
    pub damage_type: DamageType,
    pub shape: SkillShape,
    /// Threat multiplier (1.0 = threat equal to damage dealt).
    pub threat_multiplier: f32,
}

/// Registry of all known abilities, keyed by ability_id.
#[derive(Clone, Debug, Default)]
pub struct AbilityRegistry {
    abilities: HashMap<u32, AbilityData>,
    timelines: HashMap<u32, AbilityTimeline>,
}

impl AbilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, data: AbilityData) {
        self.abilities.insert(data.ability_id, data);
    }

    /// Register an ability's tick-offset timeline (timing of hitbox spawn, damage frame, cooldown).
    pub fn register_timeline(&mut self, timeline: AbilityTimeline) {
        self.timelines.insert(timeline.ability_id, timeline);
    }

    pub fn get(&self, ability_id: u32) -> Option<&AbilityData> {
        self.abilities.get(&ability_id)
    }

    /// Look up the timeline for an ability. Required by UseAbility wiring.
    pub fn get_timeline(&self, ability_id: u32) -> Option<&AbilityTimeline> {
        self.timelines.get(&ability_id)
    }
}
