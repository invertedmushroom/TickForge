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
    /// Warning that a telegraphed/lock-on attack is incoming.
    /// Emitted when the ability timeline fires a `Telegraph` action,
    /// giving the target advance notice to dodge or counter.
    LockOnWarning {
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
    /// A projectile was removed (hit a target or exceeded max range).
    /// Clients kill the predicted visual for this `execution_id`.
    ProjectileRemoved {
        execution_id: u64,
    },
    CooldownReady {
        ability_id: u32,
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

    // ── System events ──────────────────────────────────
    TickBoundary,
}

