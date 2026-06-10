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
    /// Warning that a world-position AoE is incoming.
    AreaTelegraph {
        source: EntityId,
        ability_id: u32,
        position: crate::types::Vec3f,
        radius: f32,
        shape: String,
        /// Tick when the damage frame will land.
        impact_tick: u64,
    },
    /// Client-facing encounter presentation cue.
    ///
    /// Encounter rules use this for mechanic warnings and renderable
    /// non-damaging elements. It is separate from internal encounter bus
    /// events, which are rule wiring and are not committed for clients.
    EncounterCue {
        source: EntityId,
        target: EntityId,
        cue_id: String,
        anchor_entity: Option<EntityId>,
        position: crate::types::Vec3f,
        shape: String,
        inner_radius: f32,
        outer_radius: f32,
        half_height: f32,
        starts_at_tick: u64,
        expires_at_tick: u64,
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
        /// Cooldown duration in ticks after applying server-side
        /// reductions (buffs, equipment, stats). Counts forward from
        /// the tick of this `CastStart` event. `0` if the ability has
        /// no `CooldownStart` action in its timeline. See
        /// `docs/contracts/ability_cast_lifecycle_contract.md`.
        effective_cooldown_ticks: u32,
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

    // ── Volume events ──────────────────────────────────
    /// An entity entered a gameplay volume (trigger zone, puzzle pad,
    /// water, arena). Emitted on the tick the entity first overlaps the
    /// volume's sensor.
    VolumeEnter {
        volume_id: u64,
        entity: EntityId,
    },
    /// An entity left a gameplay volume. Emitted on the tick its overlap
    /// is no longer reported by the physics backend, including when the
    /// volume itself despawns.
    VolumeExit {
        volume_id: u64,
        entity: EntityId,
    },
    /// An in-flight cast or charge was terminated by a non-natural cause.
    ///
    /// **Append-only**: kept at the tail of the enum so existing serde
    /// ordinals stay stable for replay fixtures and on-disk traces.
    ///
    /// Current emitted reasons are `Death` and `HardCC`. Other variants
    /// are reserved so client `match` statements are exhaustive ahead of
    /// future source-side emission work. See
    /// `docs/contracts/ability_cast_lifecycle_contract.md`.
    AbilityCancelled {
        ability_id: u32,
        reason: AbilityCancelReason,
    },
}

/// Why an in-flight ability cast or charge was cancelled.
///
/// Wire-shared with the SpacetimeDB `AbilityCancelReasonWire` enum in
/// `server_module::tables`. Variant order must stay in sync with that
/// wire enum's discriminants — append new variants at the end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AbilityCancelReason {
    /// The caster transitioned to `DespawnPending` this tick.
    Death,
    /// An interruptible cast or charge was cancelled because the caster
    /// is hard-CC disabled. Abilities flagged `usable_while_cc` are not
    /// cancelled by this sweep.
    HardCC,
    /// Reserved: caster issued a manual cancel intent. Not yet emitted.
    Manual,
    /// Reserved: caster moved outside the allowed movement envelope.
    /// Not yet emitted.
    Movement,
    /// Reserved: caster took damage that breaks the cast. Not yet
    /// emitted.
    Damage,
    /// Reserved: a new cast / charge replaced this one. Not yet
    /// emitted.
    Replaced,
}
