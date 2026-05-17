use serde::{Deserialize, Serialize};

use crate::entity_id::EntityId;
use crate::tick::TickId;

// DamageType is the canonical shared type from game_schema.
pub use game_schema::DamageType;

/// Simulation event emitted during a tick.
///
/// Events are collected in-memory during the tick pipeline and only
/// written to SpacetimeDB event tables at the commit stage.
///
/// Each event references a tick_id and event_sequence for total ordering
/// within a tick, enabling deterministic replay.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimEvent {
    pub tick_id: TickId,
    /// Monotonically increasing within a tick for total ordering.
    pub event_sequence: u32,
    pub entity_id: EntityId,
    pub payload: EventPayload,
}

/// Event categories matching the spec.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EventPayload {
    // ── Combat events ──────────────────────────────────
    Damage {
        source: EntityId,
        amount: f32,
        damage_type: DamageType,
    },
    SkillHit {
        skill_id: u32,
        source: EntityId,
    },
    /// Attack was fully dodged (iframe window).
    Dodged {
        source: EntityId,
        ability_id: u32,
    },
    /// Attack was blocked. `perfect` indicates perfect-block timing.
    Blocked {
        source: EntityId,
        ability_id: u32,
        damage_taken: f32,
        perfect: bool,
    },
    /// Warning that a telegraphed attack is incoming.
    /// Emitted when the ability timeline fires a `Telegraph` action,
    /// giving the target advance notice to dodge or counter.
    TelegraphWarning {
        source: EntityId,
        target: EntityId,
        /// Tick when the damage frame will land.
        impact_tick: u64,
    },
    BuffApplied {
        buff_id: u32,
        source: EntityId,
        duration_ticks: u32,
    },
    BuffExpired {
        buff_id: u32,
    },

    // ── Ability lifecycle events ───────────────────
    /// An entity began casting an ability. Enables cast bars,
    /// wind-up animations, and counterplay on other clients.
    CastStart {
        ability_id: u32,
        /// Total timeline duration in ticks (max tick_offset + 1).
        cast_duration_ticks: u32,
    },
    /// An entity began charging a hold-to-release ability.
    /// The client should show a charge bar that fills toward `max_ticks`.
    ChargeStart {
        ability_id: u32,
        /// Auto-release threshold (last tier's `min_ticks`).
        max_ticks: u32,
    },
    /// A charging entity crossed a tier threshold.
    /// The client can play a tier-up VFX/SFX.
    ChargeTierReached {
        ability_id: u32,
        tier: u8,
    },
    /// Damage to this entity was reduced because an ally's block stance
    /// provided cover ("tank cover" / TERA-style shield).
    Covered {
        blocker: EntityId,
        ability_id: u32,
        damage_taken: f32,
    },
    /// An entity raised their block stance (first tick of hold-to-block).
    /// Enables shield-up animations and counterplay visibility.
    BlockStart,
    /// An entity dropped their block stance (stopped holding block).
    BlockEnd,
    HitboxSpawned {
        ability_id: u32,
    },
    DamageFrame {
        ability_id: u32,
    },
    HitboxRemoved {
        ability_id: u32,
    },
    /// A world-space projectile was launched. Clients use this to spawn
    /// a predicted visual that travels along `direction` at `speed`.
    ProjectileLaunched {
        execution_id: u64,
        ability_id: u32,
        origin: crate::types::Vec3f,
        direction: crate::types::Vec3f,
        speed: f32,
        max_range: f32,
    },
    /// A world-space hazard zone was spawned (ground-target or fixed-world-offset).
    /// Clients use this to render a persistent ground indicator at the given position.
    HazardSpawned {
        execution_id: u64,
        ability_id: u32,
        position: crate::types::Vec3f,
        radius: f32,
    },
    /// A short-lived world-space hitbox spawned by another hitbox's `on_contact`
    /// follow-up. Clients render a brief flash at `position` that fades over
    /// `duration_ticks`. `parent_execution_id` is the cast that triggered the
    /// follow-up (useful for grouping/attribution on the client).
    ContactHitboxSpawned {
        execution_id: u64,
        parent_execution_id: u64,
        ability_id: u32,
        position: crate::types::Vec3f,
        radius: f32,
        duration_ticks: u32,
    },
    /// A detached skill object (projectile or hazard) was removed.
    /// Clients kill the predicted/placed visual for this `execution_id`.
    SkillObjectRemoved {
        execution_id: u64,
    },
    CooldownReady {
        ability_id: u32,
    },

    /// Entity took falling damage.
    FallDamage {
        damage: f32,
        impact_speed: f32,
    },

    /// Entity was healed (e.g. NPC evade arrival full-heal).
    Healed {
        amount: f32,
        source: EntityId,
    },

    /// Entity left the ground by jumping. Clients use this to trigger jump
    /// animations and sound. Paired with landing detection on the client side
    /// (arc cleared on `drive_arc_movement` grounded detection).
    Jumped,

    // ── CC events ──────────────────────────────────────────
    /// Entity was knocked back by an attacker (horizontal arc + STUNNED until landing).
    Knockback {
        source: EntityId,
        force: f32,
    },
    /// Entity was launched upward (FLOATING until landing, then KNOCKED_DOWN recovery).
    Launched {
        source: EntityId,
    },
    /// Entity was stunned (cannot act for duration_ticks).
    Stunned {
        source: EntityId,
        duration_ticks: u32,
    },
    /// Entity was knocked down (on ground, cannot act for duration_ticks).
    KnockedDown {
        source: EntityId,
        duration_ticks: u32,
    },
    /// Entity was pulled toward attacker (arc toward source + STUNNED until landing).
    Pulled {
        source: EntityId,
    },
    /// Entity was put to sleep (cannot act; broken by damage).
    Slept {
        source: EntityId,
        duration_ticks: u32,
    },
    /// Entity was silenced (cannot cast abilities; can move/jump/block).
    Silenced {
        source: EntityId,
        duration_ticks: u32,
    },
    /// Entity was feared (forced movement away from source at 50% speed).
    Feared {
        source: EntityId,
        duration_ticks: u32,
    },
    /// Stability absorbed a CC application (one stack consumed).
    StabilityConsumed {
        /// The buff_id of the stability buff that absorbed the CC.
        buff_id: u32,
    },

    // ── World events ───────────────────────────────────
    EntitySpawned,
    EntityDied {
        killer: Option<EntityId>,
    },
    EntityDespawned,
    PickupCollected {
        item_id: u32,
    },
    /// Player interacted with a world object (entity) within proximity range.
    InteractTriggered {
        target: EntityId,
    },

    // ── Equipment events ────────────────────────────────
    /// Entity swapped to a different weapon set.
    WeaponSwapped {
        new_set: u8,
    },

    // ── CC counterplay events ──────────────────────────
    /// A specific CC effect was cleared from the entity (e.g. by ClearCC or Stunbreak).
    CCCleared {
        /// Which CC type was removed.
        cc_effect: game_schema::CCEffect,
        /// Entity that caused the clear (self for self-cleanse).
        source: EntityId,
    },
    /// One or more Condition debuffs were cleansed from the entity.
    Cleansed {
        /// Number of Conditions actually removed.
        count: u32,
        /// Entity that performed the cleanse.
        source: EntityId,
    },
    /// Entity broke free of all CC via stunbreak.
    Stunbreak,
    /// Diminishing returns made the entity fully immune to a CC application.
    CCImmune {
        /// The CC type that was resisted.
        cc_effect: game_schema::CCEffect,
        /// Entity that attempted the CC.
        source: EntityId,
    },

    /// A lag-compensated hit was confirmed against this entity.
    /// Emitted alongside `SkillHit`/`Damage` for observability — clients can
    /// use this to display a "rewound" indicator or log latency diagnostics.
    CompensationApplied {
        source: EntityId,
        ability_id: u32,
        /// How many ticks the target position was rewound.
        rewind_ticks: u32,
    },

    // ── System events ──────────────────────────────────
    TickBoundary,

    // ── Lock-on events ─────────────────────────────────
    /// A TERA-style lock-on selection session started. Client should enter crosshair tagging mode.
    LockOnSessionStarted {
        source: EntityId,
        ability_id: u32,
    },
    /// A tagged target's lock-on was cancelled (session cancelled, caster died, or target
    /// moved out of range at fire time). Client clears the "targeted" indicator.
    LockOnCanceled {
        source: EntityId,
        target: EntityId,
    },
    /// A tagged entity was selected in a lock-on session. Client shows "targeted" indicator.
    LockOnWarning {
        source: EntityId,
        target: EntityId,
    },
    /// Lock-on ability fired. Lists all attempted targets (valid and invalid).
    LockOnFired {
        source: EntityId,
        /// All targets that were in the session at fire time (both hit and skipped).
        targets: Vec<EntityId>,
    },

    // ── Teleport events ────────────────────────────────
    /// An entity was teleported (backstab, blink).
    Teleported {
        entity: EntityId,
        from: crate::types::Vec3f,
        to: crate::types::Vec3f,
    },
}
