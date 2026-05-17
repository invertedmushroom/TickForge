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
    BuffApplied {
        buff_id: u32,
        source: EntityId,
        duration_ticks: u32,
    },
    BuffExpired {
        buff_id: u32,
    },

    // ── Ability lifecycle events ───────────────────────
    HitboxSpawned {
        ability_id: u32,
    },
    DamageFrame {
        ability_id: u32,
    },
    HitboxRemoved {
        ability_id: u32,
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

