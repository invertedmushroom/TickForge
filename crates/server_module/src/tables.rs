use spacetimedb::{table, ScheduleAt, SpacetimeType};

// Re-export shared types from game_schema so the rest of server_module
// (and generated client bindings) can use them directly.
pub use game_schema::{
    Vec3f, EntityKind, EntityState, NpcAiState, DamageType,
    IntentAction, AbilityTarget, MoveDir, UseAbilityData,
};
// ── Module Config ───────────────────────────────────────────────────
// Stores the admin identity (the CLI identity that published the module).
// Populated during init; used for owner-only reducer auth checks.

#[table(accessor = module_config, public)]
pub struct ModuleConfig {
    #[primary_key]
    pub key: u32,
    pub admin: spacetimedb::Identity,
    /// Last sim tick successfully committed by the simulation worker.
    /// Read by tick_trigger to gate backpressure (skip insert when backlog is too large).
    pub last_committed_tick: u64,
}
// ── Simulation Clock ────────────────────────────────────────────────
// Per spec: tick number must be committed through the database.
// The DB commit timeline defines the canonical tick.

#[table(accessor = sim_tick, public)]
pub struct SimTick {
    #[primary_key]
    pub tick_id: u64,
    pub timestamp_us: i64,
}

// ── Tick Scheduler ──────────────────────────────────────────────────
// Drives the simulation loop via SpacetimeDB scheduled reducers.
// A scheduled reducer fires at a repeating interval (best-effort).

#[table(accessor = tick_schedule, scheduled(crate::reducers::tick_trigger))]
pub struct TickSchedule {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
}

// ── Entity State ────────────────────────────────────────────────────
// Core entity table — one row per simulated entity.
// EntityKind & EntityState are defined in game_schema.

#[table(accessor = entity, public)]
pub struct Entity {
    #[primary_key]
    #[auto_inc]
    pub entity_id: u64,
    pub kind: EntityKind,
    pub state: EntityState,
    pub spawned_at_tick: u64,
    pub owner_identity: Option<spacetimedb::Identity>,
}

// ── Transforms ──────────────────────────────────────────────────────
// Authoritative spatial state. Single writer: physics system.

#[table(accessor = entity_transform, public)]
pub struct EntityTransform {
    #[primary_key]
    pub entity_id: u64,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
    pub rot_x: f32,
    pub rot_y: f32,
    pub rot_z: f32,
    pub rot_w: f32,
    pub vel_x: f32,
    pub vel_y: f32,
    pub vel_z: f32,
    pub angvel_x: f32,
    pub angvel_y: f32,
    pub angvel_z: f32,
    pub last_tick: u64,
}

// ── Health ──────────────────────────────────────────────────────────
// Single writer: combat system.

#[table(accessor = entity_health, public)]
pub struct EntityHealth {
    #[primary_key]
    pub entity_id: u64,
    pub hp: f32,
    pub max_hp: f32,
}

// ── Player Intents ──────────────────────────────────────────────────
// Per spec: inputs bound to target_tick, with sequence_id for replay protection.
// IntentAction, AbilityTarget, MoveDir, UseAbilityData, Vec3f are in game_schema.

#[table(accessor = player_intent, public, index(accessor = by_target_tick, btree(columns = [target_tick])))]
pub struct PlayerIntent {
    #[primary_key]
    #[auto_inc]
    pub intent_id: u64,
    pub client_identity: spacetimedb::Identity,
    pub entity_id: u64,
    pub sequence_id: u64,
    pub target_tick: u64,
    pub client_time_ms: u64,
    pub action: IntentAction,
}

// ── Input Sequence Tracking ─────────────────────────────────────────
// Per spec: server tracks last_processed_sequence per client
// to prevent replay attacks, duplicate inputs, out-of-order handling.

#[table(accessor = client_sequence, public)]
pub struct ClientSequence {
    #[primary_key]
    pub client_identity: spacetimedb::Identity,
    pub last_processed_sequence: u64,
    pub entity_id: u64,
}

// ── Buffs ───────────────────────────────────────────────────────────
// Single writer: combat system.

#[table(accessor = active_buff, public)]
pub struct ActiveBuff {
    #[primary_key]
    #[auto_inc]
    pub buff_instance_id: u64,
    pub entity_id: u64,
    pub buff_id: u32,
    pub source_entity: u64,
    pub stacks: u32,
    pub expires_at_tick: Option<u64>,
}

// ── Aggro ───────────────────────────────────────────────────────────
// Single writer: combat system.

#[table(accessor = threat_entry, public)]
pub struct ThreatEntry {
    #[primary_key]
    #[auto_inc]
    pub threat_id: u64,
    pub npc_entity: u64,
    pub source_entity: u64,
    pub threat: f32,
}

// ── NPC State ───────────────────────────────────────────────────────
// Single writer: AI system. NpcAiState is in game_schema.

#[table(accessor = npc_state, public)]
pub struct NpcState {
    #[primary_key]
    pub entity_id: u64,
    pub ai_state: NpcAiState,
    pub target_entity: Option<u64>,
}

// ── Event Tables ────────────────────────────────────────────────────
// Per spec: event tables hold transient rows — inserted during a
// reducer transaction, broadcast to subscribers on commit, then
// effectively consumed. We use public tables so clients subscribe.
// DamageType is in game_schema.

#[table(accessor = combat_event, public)]
pub struct CombatEvent {
    #[primary_key]
    #[auto_inc]
    pub event_id: u64,
    pub tick_id: u64,
    pub event_sequence: u32,
    pub source_entity: u64,
    pub target_entity: u64,
    pub event_kind: CombatEventKind,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct DamageData {
    pub amount: f32,
    pub damage_type: DamageType,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct BuffAppliedData {
    pub buff_id: u32,
    pub duration_ticks: u32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub enum CombatEventKind {
    Damage(DamageData),
    SkillHit(u32),
    BuffApplied(BuffAppliedData),
    BuffExpired(u32),
    EntityDied(Option<u64>),
}

#[table(accessor = world_event, public)]
pub struct WorldEvent {
    #[primary_key]
    #[auto_inc]
    pub event_id: u64,
    pub tick_id: u64,
    pub event_sequence: u32,
    pub entity_id: u64,
    pub event_kind: WorldEventKind,
}

#[derive(SpacetimeType, Clone, Debug)]
pub enum WorldEventKind {
    EntitySpawned(EntityKind),
    EntityDespawned,
    PickupCollected(u32),
    /// Player entity interacted with a world object within proximity range.
    /// Payload is the target entity_id.
    InteractTriggered(u64),
}

// ── AOI / Spatial ───────────────────────────────────────────────────
// Per spec: world divided into grid cells, clients subscribe by region.

#[table(accessor = entity_region, public, index(accessor = by_region, btree(columns = [region_x, region_z])))]
pub struct EntityRegion {
    #[primary_key]
    pub entity_id: u64,
    pub region_x: i32,
    pub region_z: i32,
}

// ── Trusted Workers ─────────────────────────────────────────────────
// Per spec (security_and_authority): commit_tick_results and clear_events
// are called by the external simulation worker process, whose Identity
// differs from the module Identity. We maintain an allowlist of worker
// identities so the auth guard can accept them.
//
// Workers are registered via the register_worker reducer, which itself
// is restricted to the module identity (called during init or via CLI).

#[table(accessor = trusted_worker, public)]
pub struct TrustedWorker {
    #[primary_key]
    pub worker_identity: spacetimedb::Identity,
}
