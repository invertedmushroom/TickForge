use std::collections::HashMap;

use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_schema::{DamageType, Vec3f};
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

/// Runtime parameters for a single ability cast.
///
/// Created in Phase 2 alongside `AbilityExecutionContext` so that a single
/// `ability_id` can produce different execution paths (charge tiers, etc.)
/// without requiring separate ability IDs.
///
/// `charge_level`: 0 = base cast, higher values = extended hold time.
/// `variant`: reserved — combo routing now redirects to a separate ability via
///   `active_windows`, so each combo step has its own `ability_id` and timeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbilityParams {
    pub charge_level: u8,
    pub variant: u8, // reserved for future use (combo routing now uses separate ability IDs)
}

/// In-flight charging state for one entity.
///
/// Created when a UseAbility intent targets a chargeable ability.
/// Consumed on ReleaseAbility or auto-release at max tier.
#[derive(Clone, Debug)]
pub struct ChargingState {
    pub ability_id: u32,
    pub started_at: TickId,
    pub targeting: ResolvedTargeting,
    pub rewind_ticks: u32,
    /// Highest tier notified to the client so far (for event dedup).
    pub notified_tier: u8,
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
    /// Open a combo / follow-up eligibility window.
    ///
    /// Phase 3 writes `(next_ability_id, expiry)` into `CombatState::active_windows`
    /// keyed by `(entity, this_ability_id)`.  When the player presses the *same*
    /// ability again within the window, Phase 2 redirects the cast to
    /// `next_ability_id` — a distinct ability with its own timeline, damage, and
    /// cooldown — then consumes the window.  Phase 8 drains expired entries.
    OpenFollowUpWindow { duration_ticks: u32, next_ability_id: u32 },
    /// Apply a buff to the caster (self-buff) from the ability timeline.
    ///
    /// Phase 3 looks up the `buff_id` in the `BuffRegistry` and creates an
    /// `ActiveBuff` from the template with the caster as both source and target.
    /// Stacking logic applies: if a buff with the same `buff_id` already exists,
    /// stacks are incremented (up to `max_stacks`) and the duration is refreshed.
    ApplyBuff { buff_id: u32 },
    /// Set tactical iframe flags on the caster (timeline-driven).
    ///
    /// Phase 3 writes `dodge_active` into `CombatState::tactical`. The flag
    /// persists until a corresponding `StanceEnd` action fires — Phase 8 does
    /// NOT clear `dodge_active`.
    ///
    /// `blocking` is no longer set here — blocking is intent-driven (hold-to-block).
    StanceBegin { dodge_active: bool, #[serde(default)] rooted: bool },
    /// Clear tactical iframe flags on the caster (timeline-driven).
    ///
    /// Paired with `StanceBegin` to define a fixed-duration iframe window.
    /// Phase 3 clears `dodge_active` when this action fires.
    StanceEnd,
    /// Launch the caster in a kinematic arc (vault / leap).
    ///
    /// Phase 3 sets `TacticalState::arc_velocity` and roots the caster.
    /// Phase 2 integrates gravity each tick and feeds the result into
    /// `move_character`. The arc ends automatically when `MoveResult::grounded`
    /// is true (after the launch tick) or when a `StanceEnd` fires.
    ArcMovement { speed: f32, lift: f32, gravity: f32 },
    /// Emit a `LockOnWarning` event to the resolved target.
    ///
    /// `impact_delay` is the number of ticks from now until the damage frame lands.
    /// Phase 3 reads the execution context's `ResolvedTargeting` to determine the
    /// target entity — only `Entity` and `LockOn` targeting produce a warning.
    Telegraph { impact_delay: u32 },
}

/// Per-entity scheduled action for the ability scheduler.
///
/// The simulation tick consumes due actions each frame, enabling
/// clean overlap of cast windows, hit frames, and cooldown expirations.
///
/// `id` is a pipeline-scoped monotonic counter assigned at scheduling time.
/// `source` is the execution that caused this action — `None` for non-ability
/// deferred work such as buff expiry. Together they form the causal chain for
/// replay tracing and double-schedule / missed-expiry investigations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduledAction {
    /// Monotonically increasing ID assigned by the pipeline at scheduling time.
    /// Unique within a single worker session; use for log correlation and replay.
    pub id: u64,
    pub tick_id: TickId,
    pub entity: EntityId,
    /// The ability execution that caused this action, if any.
    /// `None` for buff expiry and other non-ability deferred actions.
    pub source: Option<AbilityExecutionId>,
    pub action_type: ScheduledActionType,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ScheduledActionType {
    AbilityFrame {
        /// Identity of the cast instance that spawned this action.
        /// Look up `AbilityExecutionContext` by this ID to get cast-time targeting
        /// and spatial snapshot — do not re-derive from current world state.
        execution_id: AbilityExecutionId,
        ability_id: u32,
        action: AbilityAction,
    },
    BuffExpire { buff_id: u32 },
    // CooldownExpire removed — cooldown expiry is now owned by TickPipeline::cooldowns.
    // Phase 8 drains the HashMap each tick and emits CooldownReady events directly,
    // making this variant unnecessary and eliminating the O(n) queue scan in is_on_cooldown.
}

/// Discrete charge tier definition (TERA / Monster Hunter style).
///
/// Each tier has a minimum tick threshold and a damage multiplier.
/// Tiers must be sorted ascending by `min_ticks`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChargeTierDef {
    /// Minimum ticks of charging required to reach this tier.
    pub min_ticks: u32,
    /// Damage multiplier applied at this tier (1.0 = base).
    pub damage_mult: f32,
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
    /// Buff IDs applied to the target on each hit (looked up from `BuffRegistry`).
    /// Empty by default (most abilities deal only damage).
    #[serde(default)]
    pub on_hit_buffs: Vec<u32>,
    /// Knockback impulse magnitude applied to the target on hit.
    /// Direction is computed as attacker→target at hit time.
    /// 0.0 = no knockback (default). Blocked targets are immune.
    #[serde(default)]
    pub knockback_force: f32,
    /// If true, targets that leave and re-enter the hitbox can be damaged again.
    /// Default false (single-hit). Useful for lingering hazards and rolling projectiles.
    #[serde(default)]
    pub allow_reentry: bool,
    /// Discrete charge tiers (TERA / Monster Hunter style). `None` = instant cast.
    /// When present, UseAbility starts a charge; ReleaseAbility fires at the achieved tier.
    /// The last tier's `min_ticks` is the auto-release threshold.
    #[serde(default)]
    pub charge_tiers: Option<Vec<ChargeTierDef>>,
    /// Tick interval between periodic re-damage for lingering area effects (HazardZone).
    /// 0 = single-hit only (default). e.g. 20 = re-damage every 20 ticks (1 second at 20 Hz).
    #[serde(default)]
    pub damage_interval_ticks: u32,
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

// ── Ability execution context ────────────────────────────────

/// Monotonically increasing identity for one concrete cast of an ability.
///
/// Two casts of ability 1 by entity A have different `AbilityExecutionId` values,
/// so per-cast state is unambiguous even when the same ability fires twice in quick
/// succession or spawns multiple simultaneous effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AbilityExecutionId(pub u64);

/// Targeting resolved and validated at cast time.
///
/// Stored on `AbilityExecutionContext` so later timeline phases use the targeting
/// intent from Phase 2, not whatever the current world state happens to be.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ResolvedTargeting {
    /// Self-cast or PBAoE — no external target.
    SelfCast,
    /// Validated entity target.
    Entity { target: EntityId },
    /// World-position target (stationary AoEs, ground-targeted abilities).
    Position { point: Vec3f },
    /// Normalised direction (cones, line sweeps).
    Direction { dir: Vec3f },
    /// Lock-on acquired target — re-validated at execution time.
    LockOn { target: EntityId },
}

/// Runtime record for one specific cast of an ability.
///
/// Created in Phase 2 when `UseAbility` is accepted, referenced by all later
/// timeline phases for this cast, and removed when the cast's last action completes.
///
/// Rules:
/// - Cast-time geometry (origin, facing) is snapshotted here in Phase 2.
/// - Damage values and stats come from `AbilityRegistry` at execution time.
/// - Mutable gameplay truth (positions, health) stays in `SimState`.
#[derive(Clone, Debug)]
pub struct AbilityExecutionContext {
    pub execution_id: AbilityExecutionId,
    pub ability_id: u32,
    pub caster: EntityId,
    pub started_at: TickId,
    /// Resolved and validated targeting intent.
    pub targeting: ResolvedTargeting,
    /// Caster world position at cast time.
    pub origin: Vec3f,
    /// Caster facing direction (XZ-plane, unit length) at cast time.
    pub facing: Vec3f,
    /// Runtime parameters for this cast (charge tier, etc.).
    /// Resolved in Phase 2 from intent data.
    pub params: AbilityParams,
    /// Number of ticks to rewind target positions for lag compensation.
    /// Computed in Phase 2 from the intent's observed-tick delta, clamped to MAX_REWIND_TICKS.
    /// 0 means no compensation (local or very-low-latency client).
    pub rewind_ticks: u32,
}

/// Sparse store for all in-flight ability executions.
///
/// One entry per active cast, removed on completion. Keyed by `AbilityExecutionId`
/// rather than entity index because casts span multiple ticks and are not dense
/// enough to benefit from a flat array.
#[derive(Debug, Default)]
pub struct AbilityExecutionStore {
    next_id: u64,
    active: HashMap<AbilityExecutionId, AbilityExecutionContext>,
}

impl AbilityExecutionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next execution ID without inserting a context.
    /// Call this before building the context so the ID can be embedded in
    /// the context struct and passed to `schedule_ability` in the same Phase 2 block.
    pub fn next_id(&mut self) -> AbilityExecutionId {
        let id = AbilityExecutionId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Insert a context built with the ID returned by `next_id()`.
    pub fn insert(&mut self, ctx: AbilityExecutionContext) {
        self.active.insert(ctx.execution_id, ctx);
    }

    pub fn get(&self, id: AbilityExecutionId) -> Option<&AbilityExecutionContext> {
        self.active.get(&id)
    }

    /// Remove a single execution (called when its last timeline action completes).
    pub fn remove(&mut self, id: AbilityExecutionId) -> bool {
        self.active.remove(&id).is_some()
    }

    /// Remove all executions for a given caster (called on entity despawn).
    pub fn remove_all_for_caster(&mut self, caster: EntityId) {
        self.active.retain(|_, ctx| ctx.caster != caster);
    }

    /// Returns a snapshot of all currently active execution IDs.
    /// Used by the culling pass in `phase_skill_scheduling` to find leaking contexts.
    pub fn active_ids(&self) -> Vec<AbilityExecutionId> {
        self.active.keys().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}
