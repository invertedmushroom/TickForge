use spacetimedb::{ScheduleAt, SpacetimeType, table};

// Re-export shared types from game_schema so the rest of server_module
// (and generated client bindings) can use them directly.
pub use game_schema::{
    AbilityTarget, BlockData, DamageType, EntityKind, EntityState, EquipmentSlot, IntentAction,
    MoveDir, NpcAiState, UseAbilityData, Vec3f,
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
    /// Next dynamic instance layer to allocate. Layers 0–99 are reserved;
    /// dynamic instances start at 100 and increment.
    pub next_instance_layer: u32,
    /// Monotonically increasing counter tracking the latest sim_tick row inserted.
    /// Eliminates the O(N) `sim_tick().iter().max_by_key()` scan in tick_trigger.
    pub next_tick_id: u64,
    /// Maximum number of ticks into the past that lag compensation will rewind.
    pub global_max_rewind_ticks: u32,
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
    /// RLS join key — always 0. Enables equi-join with trusted_worker for
    /// client_visibility_filter (cross-joins unsupported by subscription engine).
    #[index(btree)]
    pub rls_group: u8,
}

// ── Team Assignment ─────────────────────────────────────────────────
// Associates entities with teams for faction-based visibility (stealth).
// Primary writer during simulation commits: commit_tick_results reducer.

#[table(accessor = entity_team, public)]
pub struct EntityTeam {
    #[primary_key]
    pub entity_id: u64,
    pub team_id: u32,
}

// ── Layer Assignment ────────────────────────────────────────────────
// Public projection of entity_region.layer so the simulation worker can
// subscribe and track layer changes without exposing the private
// entity_region table (which contains spatial data used for stealth/AOI).
// Written alongside entity_region inserts/updates that change layer.

#[table(accessor = entity_layer, public)]
pub struct EntityLayer {
    #[primary_key]
    pub entity_id: u64,
    pub layer: u32,
}

// ── Encounter Add Membership ────────────────────────────────────────
// Ties a scripted encounter add (spawned by `Effect::SpawnAdds`) to its
// owning boss and the tags assigned at spawn time. Worker subscribes to
// rebuild in-memory `add_to_boss` and `entity_tags` so encounter rules
// like `OnEntityDied { tag }` can fire deterministically in production.
// See `docs/contracts/spawn_add_membership_contract.md`.
//
// Primary writer: `commit_tick_results` reducer (insert at spawn time).
// Cleanup writer: `commit_tick_results` reducer when the add or its boss
// is removed; instance teardown also cascades.

#[table(accessor = encounter_add, public, index(accessor = by_boss, btree(columns = [boss_entity])))]
pub struct EncounterAdd {
    #[primary_key]
    pub add_entity: u64,
    pub boss_entity: u64,
    pub tags: Vec<String>,
    pub archetype: String,
    pub spawned_at_tick: u64,
}

// ── Stealth ─────────────────────────────────────────────────────────
// Tracks which entities are currently stealthed and their team.
// Private — enemies must not be able to query this table directly.
// The nearby_* views read it to filter invisible entities.
// Primary writer during simulation commits: commit_tick_results reducer
// (derived from buff state).

#[table(accessor = stealthed_entity)]
pub struct StealthedEntity {
    #[primary_key]
    pub entity_id: u64,
    pub team_id: u32,
}

// ── Transforms ──────────────────────────────────────────────────────
// Authoritative spatial state replicated to clients.
// Primary writer during simulation commits: the worker's physics snapshot.
// Server reducers also seed or restore rows during spawn, respawn, and
// instance-management flows.

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
    #[index(btree)]
    pub rls_group: u8,
}

// ── Health ──────────────────────────────────────────────────────────
// Authoritative health state replicated to clients.
// Primary writer during simulation commits: the worker combat snapshot.
// Server reducers also seed or restore rows during spawn, respawn, and
// instance-management flows.

#[table(accessor = entity_health, public)]
pub struct EntityHealth {
    #[primary_key]
    pub entity_id: u64,
    pub hp: f32,
    pub max_hp: f32,
    #[index(btree)]
    pub rls_group: u8,
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
    #[index(btree)]
    pub entity_id: u64,
    pub sequence_id: u64,
    pub target_tick: u64,
    pub client_observed_tick: u64,
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

/// Persisted buff state — only per-instance runtime fields.
///
/// Static template data (modifiers, buff_kind, max_stacks, etc.) is
/// reconstructed from `BuffRegistry::get(buff_id)` at rehydration time.
/// This keeps the commit payload small: ~10 columns instead of ~25.
#[table(accessor = active_buff, public)]
pub struct ActiveBuff {
    #[primary_key]
    #[auto_inc]
    pub buff_instance_id: u64,
    #[index(btree)]
    pub entity_id: u64,
    pub buff_id: u32,
    pub source_entity: u64,
    pub stacks: u32,
    pub expires_at_tick: Option<u64>,
    /// AI override kind: 0=ForceFlee, 1=ForceIdle, 2=ForceFocus. None = no override.
    /// Persisted because ForceFocus carries a per-instance target entity.
    pub mod_ai_override_kind: Option<u8>,
    /// Target entity for ForceFocus override. Only meaningful when ai_override_kind == 2.
    pub mod_ai_override_target: Option<u64>,
    /// Stealth flag — persisted so the reducer can derive `stealthed_entity` rows
    /// without needing the buff registry.
    pub mod_stealth: Option<bool>,
    /// Tick when DoT damage was last applied. Per-instance runtime state.
    pub last_dot_tick: Option<u64>,
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

// ── NPC Config ──────────────────────────────────────────────────────
// Static per-NPC spawn configuration read by the coordinator at entity
// creation. Unlike NpcState (which is updated every tick by AI), this
// table is typically written during spawn/setup and then treated as
// read-mostly runtime configuration.

#[table(accessor = npc_config, public)]
pub struct NpcConfig {
    #[primary_key]
    pub entity_id: u64,
    /// Encounter rules key used by worker boss registration. None => fallback behavior.
    pub encounter_name: Option<String>,
    pub archetype_id: Option<String>,
    /// Training dummy — never enters combat, ignores threat.
    pub passive: bool,
    /// Fights back but does not chase (stationary turret).
    pub no_chase: bool,
    /// Up to 4 ability IDs the NPC can use. `None` slots are skipped.
    /// If all are None, defaults to ability 1 (Slash).
    pub ability_id_1: Option<u32>,
    pub ability_id_2: Option<u32>,
    pub ability_id_3: Option<u32>,
    pub ability_id_4: Option<u32>,
    /// Max distance from home position before NPC evades back. 0 = no leash.
    pub leash_radius: f32,
    /// Proximity aggro radius. Idle/patrol NPCs attack players within this. 0 = disabled.
    pub aggro_radius: f32,
    /// Authoritative physics body shape for this NPC, encoded as a
    /// `BodyShape` discriminant (see `game_core::physics_backend::BodyShape::to_u8`).
    /// `None` means "use the kind's default capsule" (NpcCapsule for Npc,
    /// BossCapsule for Boss).
    pub body_shape: Option<u8>,
}

// ── Event Tables ────────────────────────────────────────────────────
// Per spec: event tables hold transient rows — inserted during a
// reducer transaction, broadcast to subscribers on commit, then
// effectively consumed. We use public tables so clients subscribe.
// DamageType is in game_schema.

#[table(accessor = combat_event, public, event)]
pub struct CombatEvent {
    #[primary_key]
    #[auto_inc]
    pub event_id: u64,
    #[index(btree)]
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
pub struct HealedData {
    pub amount: f32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct BuffAppliedData {
    pub buff_id: u32,
    pub duration_ticks: u32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct CastStartData {
    pub ability_id: u32,
    pub cast_duration_ticks: u32,
    /// Cooldown duration in ticks after applying server-side
    /// reductions. Counts forward from the tick of the parent
    /// `combat_event` row. `0` if the ability has no cooldown.
    /// See `docs/contracts/ability_cast_lifecycle_contract.md`.
    pub effective_cooldown_ticks: u32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct ChargeStartData {
    pub ability_id: u32,
    pub max_ticks: u32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct ChargeTierReachedData {
    pub ability_id: u32,
    pub tier: u8,
}

#[derive(SpacetimeType, Clone, Debug)]
pub enum CombatEventKind {
    CastStart(CastStartData),
    ChargeStart(ChargeStartData),
    ChargeTierReached(ChargeTierReachedData),
    BlockStart,
    BlockEnd,
    Damage(DamageData),
    Healed(HealedData),
    SkillHit(u32),
    BuffApplied(BuffAppliedData),
    BuffExpired(u32),
    EntityDied(Option<u64>),
    Dodged(u32),
    Blocked(BlockedData),
    Covered(CoveredData),
    TelegraphWarning(TelegraphWarningData),
    AreaTelegraph(AreaTelegraphData),
    EncounterCue(EncounterCueData),
    LockOnAcquired,
    LockOnSessionStarted(LockOnSessionStartedData),
    LockOnCanceled(LockOnCanceledData),
    LockOnFired(LockOnFiredData),
    ProjectileLaunched(ProjectileLaunchedData),
    HazardSpawned(HazardSpawnedData),
    ContactHitboxSpawned(ContactHitboxSpawnedData),
    SkillObjectRemoved(u64),
    Teleported(TeleportedData),
    // CC events
    Knockback(KnockbackData),
    Launched,
    Stunned(StunnedData),
    KnockedDown(KnockedDownData),
    Pulled,
    Slept(SleptData),
    Silenced(SilencedData),
    Feared(FearedData),
    StabilityConsumed(u32),
    WeaponSwapped(u8),
    CCCleared(CCClearedData),
    Cleansed(CleansedData),
    Stunbreak,
    CCImmune(CCImmuneData),
    /// An in-flight cast or charge was cancelled.
    /// **Append-only**: this MUST stay at the tail of the enum to
    /// preserve discriminants for existing client decoders.
    AbilityCancelled(AbilityCancelledData),
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct AbilityCancelledData {
    pub ability_id: u32,
    pub reason: AbilityCancelReasonWire,
}

/// Wire mirror of `game_protocol::event::AbilityCancelReason`.
///
/// **Append-only**: variant order matches the in-memory enum. New
/// reasons must be appended in both enums simultaneously.
#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbilityCancelReasonWire {
    Death,
    HardCC,
    Manual,
    Movement,
    Damage,
    Replaced,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct TelegraphWarningData {
    pub target: u64,
    pub impact_tick: u64,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct AreaTelegraphData {
    pub ability_id: u32,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
    pub radius: f32,
    pub shape: String,
    pub impact_tick: u64,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct EncounterCueData {
    pub cue_id: String,
    pub anchor_entity: Option<u64>,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
    pub shape: String,
    pub inner_radius: f32,
    pub outer_radius: f32,
    pub half_height: f32,
    pub starts_at_tick: u64,
    pub expires_at_tick: u64,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct LockOnSessionStartedData {
    pub ability_id: u32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct LockOnCanceledData {
    pub target: u64,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct LockOnFiredData {
    pub targets: Vec<u64>,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct KnockbackData {
    pub force: f32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct StunnedData {
    pub duration_ticks: u32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct KnockedDownData {
    pub duration_ticks: u32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct SleptData {
    pub duration_ticks: u32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct SilencedData {
    pub duration_ticks: u32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct FearedData {
    pub duration_ticks: u32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct BlockedData {
    pub ability_id: u32,
    pub damage_taken: f32,
    pub perfect: bool,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct CoveredData {
    pub blocker: u64,
    pub ability_id: u32,
    pub damage_taken: f32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct CCClearedData {
    pub cc_effect: game_schema::CCEffect,
    pub source: u64,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct CleansedData {
    pub count: u32,
    pub source: u64,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct CCImmuneData {
    pub cc_effect: game_schema::CCEffect,
    pub source: u64,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct ProjectileLaunchedData {
    pub execution_id: u64,
    pub ability_id: u32,
    pub origin_x: f32,
    pub origin_y: f32,
    pub origin_z: f32,
    pub direction_x: f32,
    pub direction_y: f32,
    pub direction_z: f32,
    pub speed: f32,
    pub max_range: f32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct HazardSpawnedData {
    pub execution_id: u64,
    pub ability_id: u32,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
    pub radius: f32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct ContactHitboxSpawnedData {
    pub execution_id: u64,
    pub parent_execution_id: u64,
    pub ability_id: u32,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
    pub radius: f32,
    pub duration_ticks: u32,
}

#[derive(SpacetimeType, Clone, Debug)]
pub struct TeleportedData {
    pub from_x: f32,
    pub from_y: f32,
    pub from_z: f32,
    pub to_x: f32,
    pub to_y: f32,
    pub to_z: f32,
}

#[table(accessor = world_event, public, event)]
pub struct WorldEvent {
    #[primary_key]
    #[auto_inc]
    pub event_id: u64,
    #[index(btree)]
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
// Table is PRIVATE — clients must use the `my_region` / `nearby_transforms`
// Views for AOI-filtered access (see views.rs).  The simulation worker
// writes via `commit_tick_results` reducer (server-side, unaffected by
// table visibility).

#[table(accessor = entity_region, index(accessor = by_region, btree(columns = [layer, region_x, region_z])))]
pub struct EntityRegion {
    #[primary_key]
    pub entity_id: u64,
    pub region_x: i32,
    pub region_z: i32,
    /// Visibility layer for instancing, phasing, and stealth.
    /// Layer 0 is the default open-world layer. Non-zero layers isolate
    /// entities from each other on the same grid cell (boss instances,
    /// quest phases, stealth states).
    pub layer: u32,
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
    #[index(btree)]
    pub rls_group: u8,
}

// ── Inventory & Equipment ───────────────────────────────────────────
// Economy tables live outside the tick pipeline. Clients call reducers
// directly (swap_item, equip_item, etc.). The simulation worker never
// writes these tables — it only observes player_equipment changes via
// subscription callbacks to trigger stat recalculation.
//
// Single writer: client-facing reducers (validated per-player).

#[table(accessor = player_inventory, public)]
pub struct PlayerInventory {
    #[primary_key]
    #[auto_inc]
    pub row_id: u64,
    /// Entity that owns this inventory slot.
    #[index(btree)]
    pub owner_entity: u64,
    /// Bag slot index (0-based). Unique per owner.
    pub slot_index: u32,
    /// Item definition id.
    pub item_id: u32,
    /// Stack count (1 for non-stackable items).
    pub quantity: u32,
}

#[table(accessor = player_equipment, public)]
pub struct PlayerEquipment {
    #[primary_key]
    #[auto_inc]
    pub row_id: u64,
    /// Entity that owns this equipment slot.
    #[index(btree)]
    pub owner_entity: u64,
    /// Which equipment slot this item occupies.
    pub slot: EquipmentSlot,
    /// Item definition id.
    pub item_id: u32,
}

#[table(accessor = bank, public)]
pub struct Bank {
    #[primary_key]
    #[auto_inc]
    pub row_id: u64,
    /// Entity that owns this bank slot.
    pub owner_entity: u64,
    /// Bank slot index (0-based). Unique per owner.
    pub slot_index: u32,
    /// Item definition id.
    pub item_id: u32,
    /// Stack count.
    pub quantity: u32,
}

#[table(accessor = loot_pile, public)]
pub struct LootPile {
    #[primary_key]
    #[auto_inc]
    pub loot_pile_id: u64,
    pub corpse_entity: u64,
    #[index(btree)]
    pub layer: u32,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
    pub eligible_claimants: Vec<u64>,
    pub created_at_tick: u64,
    pub expires_at_tick: u64,
}

#[table(accessor = loot_pile_item, public)]
pub struct LootPileItem {
    #[primary_key]
    #[auto_inc]
    pub loot_pile_item_id: u64,
    #[index(btree)]
    pub loot_pile_id: u64,
    pub item_id: u32,
    pub quantity: u32,
}

// ── Party System ────────────────────────────────────────────────────
// Party tables enable group play, required for dungeon entry.
// Single writer: client-facing party reducers (validated per-player).

#[table(accessor = party, public)]
pub struct Party {
    #[primary_key]
    #[auto_inc]
    pub party_id: u64,
    /// Entity ID of the party leader.
    pub leader_entity: u64,
    /// Maximum number of members (default 5).
    pub max_members: u32,
    pub created_at: i64,
}

#[table(accessor = party_member, public)]
pub struct PartyMember {
    #[primary_key]
    #[auto_inc]
    pub member_id: u64,
    #[index(btree)]
    pub party_id: u64,
    /// The entity in this party slot.
    #[index(btree)]
    pub entity_id: u64,
}

#[table(accessor = party_invite, public)]
pub struct PartyInvite {
    #[primary_key]
    #[auto_inc]
    pub invite_id: u64,
    #[index(btree)]
    pub party_id: u64,
    pub inviter_entity: u64,
    #[index(btree)]
    pub invitee_entity: u64,
    /// Microseconds since unix epoch.
    pub expires_at: i64,
}

// ── Boss Phase (ADR-0002 Tier 1) ────────────────────────────────────
// Tracks current phase for boss encounters. Written inline by
// commit_tick_results from trusted-worker boss_phase_updates. Worker detects
// HP thresholds and transitions.

#[table(accessor = boss_phase, public)]
pub struct BossPhase {
    #[primary_key]
    pub boss_entity_id: u64,
    pub phase: u32,
    pub entered_at_tick: u64,
}

// ── Zone Counter (ADR-0002 Tier 1) ──────────────────────────────────
// Per-zone counters incremented on kill/damage events. Worker tick output
// writes through commit_tick_results zone_counter_deltas; increment_zone_counter
// remains as a trusted/admin helper. Used by world_clock to evaluate phase
// transitions.
//
// Logical uniqueness on `(layer, region_x, region_z, counter_name)` is
// enforced by `reducers::upsert_zone_counter` — SpacetimeDB v2 cannot
// express composite uniqueness in storage (see the `terrain_set` comment
// below for the workspace pattern). All writers route through that
// helper; `world_clock` additionally sums same-name counters per zone
// defensively in case a legacy bug ever produced a duplicate row.

#[table(accessor = zone_counter, public, index(accessor = by_zone, btree(columns = [layer, region_x, region_z])))]
pub struct ZoneCounter {
    #[primary_key]
    #[auto_inc]
    pub counter_id: u64,
    pub layer: u32,
    pub region_x: i32,
    pub region_z: i32,
    pub counter_name: String,
    pub value: f64,
}

// ── World Phase (ADR-0002 Tier 2) ───────────────────────────────────
// Zone-level progression state. Written by world_clock scheduled reducer.
// Transitions based on zone_counter thresholds.

#[table(accessor = world_phase, public)]
pub struct WorldPhase {
    #[primary_key]
    pub zone_id: u32,
    pub phase_name: String,
    pub started_at: i64,
    pub metadata: String,
}

// ── World Activity Event (messaging spine — step 4 of the 2026-06-09 review) ─
// Scope-keyed Tier 2 event row used to gate director spawns on both a
// scope state transition AND an explicit player-presence requirement.
//
// `WorldPhase` says "this zone reached phase X"; `WorldActivityEvent`
// says "an authored event tagged Y is currently Active in scope Z and
// requires at least N players present to spawn". Together they let
// Tier 2 progression continue (durable counters, timers, world_phase)
// while preventing director spawns from firing offscreen — see
// `docs/contracts/world_activity_policy_contract.md` "no offscreen
// combat" rule and Finding #4 from the 2026-06-09 messaging review.
//
// V1 tags (no payload yet):
//   - `boss_ready` : produced by world_clock alongside the
//                    `world_phase = "boss_ready"` transition.
//   - `completed`  : produced alongside `world_phase = "completed"`.
// Future kinds (timer / chain / escalation) extend through `payload`
// without a schema migration.
//
// Logical uniqueness on `(scope_layer, scope_region_x, scope_region_z,
// tag)` is enforced by `reducers::upsert_world_activity_event` (same
// workspace pattern as `zone_counter`).
//
// Distinct from the legacy per-tick `world_event` table above, which is
// a transient game-event log keyed by `(tick_id, event_sequence)`. The
// two intentionally do not share a row type.
//
// `WorldActivityEventState` itself lives in `game_schema::world_activity`
// so the simulation worker's director can match against the same enum
// the server reducer writes.

pub use game_schema::WorldActivityEventState;

#[table(
    accessor = world_activity_event,
    public,
    index(accessor = by_scope, btree(columns = [scope_layer, scope_region_x, scope_region_z]))
)]
pub struct WorldActivityEvent {
    #[primary_key]
    #[auto_inc]
    pub event_id: u64,
    /// Activity scope: layer + region cell. Open-world events use
    /// `scope_layer = 0` with the real `(rx, rz)`; instance events use
    /// `scope_layer = instance.layer` with `(rx, rz) = (0, 0)`, matching
    /// the canonical zone-keying convention used by `zone_counter` and
    /// `world_phase`.
    pub scope_layer: u32,
    pub scope_region_x: i32,
    pub scope_region_z: i32,
    /// Event identifier within the scope. Logically unique together with
    /// the scope key; enforced by `upsert_world_activity_event`.
    pub tag: String,
    pub state: WorldActivityEventState,
    /// Minimum number of active (non-disconnected) players required in
    /// the event's scope before director triggers gated on this row
    /// fire. `0` means no presence requirement.
    pub required_players: u32,
    pub started_at: i64,
    /// Versioned JSON payload for kind-specific data. Empty for v1
    /// `boss_ready` / `completed` events.
    pub payload: String,
}

// ── NPC Goal (ADR-0002 Tier 2) ──────────────────────────────────────
// High-level NPC directives for Phase 7 AI. There is intentionally at most
// one directive per entity; the table has no producer until Tier 2 lands.

#[table(accessor = npc_goal, public)]
pub struct NpcGoal {
    #[primary_key]
    pub entity_id: u64,
    pub goal_kind: String,
    /// JSON-encoded waypoints.
    pub waypoints: String,
    pub priority: u32,
}

// ── World Clock Scheduler (ADR-0002 Tier 2) ─────────────────────────
// Drives the world_clock reducer at a low frequency (30s) for
// aggregate/timed world state transitions.

#[table(accessor = world_clock_schedule, scheduled(crate::reducers::world_clock))]
pub struct WorldClockSchedule {
    #[primary_key]
    #[auto_inc]
    pub scheduled_id: u64,
    pub scheduled_at: ScheduleAt,
}

// ── Respawn System ──────────────────────────────────────────────────
// Delayed respawn at zone-specific points. Death state is created by
// commit_tick_results when a player entity transitions to DespawnPending;
// consumed by respawn_player reducer after the delay expires.

#[table(accessor = respawn_point, public)]
pub struct RespawnPoint {
    #[primary_key]
    #[auto_inc]
    pub point_id: u64,
    #[index(btree)]
    pub layer: u32,
    pub region_x: i32,
    pub region_z: i32,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
    pub name: String,
}

#[table(accessor = death_state, public)]
pub struct DeathState {
    #[primary_key]
    pub entity_id: u64,
    pub died_at_tick: u64,
    pub respawn_at_tick: u64,
    pub killer_entity: Option<u64>,
    pub layer: u32,
    pub death_pos_x: f32,
    pub death_pos_y: f32,
    pub death_pos_z: f32,
}

// ── Instance Management ─────────────────────────────────────────────
// Dungeon/instanced zone lifecycle. Layer 0 = open world, 1–99 reserved
// persistent zones, 100+ dynamic instances.
// Single writer: instance reducers (create_instance, join_instance, etc.)

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceState {
    Pending,
    Active,
    Completed,
    Expired,
}

#[table(accessor = instance, public)]
pub struct Instance {
    #[primary_key]
    #[auto_inc]
    pub instance_id: u64,
    pub template_id: String,
    pub layer: u32,
    pub layer_group: u32,
    pub state: InstanceState,
    pub created_at: i64,
    pub expires_at: i64,
    pub max_players: u32,
}

#[table(accessor = instance_membership, public)]
pub struct InstanceMembership {
    #[primary_key]
    pub entity_id: u64,
    #[index(btree)]
    pub instance_id: u64,
    /// Set on disconnect. If all members disconnect > grace period → expire instance.
    pub disconnect_at: Option<i64>,
}

// ── Interactable Config ─────────────────────────────────────────────
// Per-entity config for interactive world objects and spawn markers (gates,
// switches, chests, grabs, BossSpawn, NpcSpawn). Sim worker subscribes for
// physics-driven interactions and metadata.
// Single writer: instance spawn path (create_instance) and
// commit_tick_results reducer (trusted worker, inline interactable updates).

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum InteractKind {
    Switch,
    Gate,
    Grab,
    Chest,
    BossSpawn,
    NpcSpawn,
}

#[derive(SpacetimeType, Clone, Copy, Debug, PartialEq, Eq)]
pub enum InteractState {
    /// Default resting state (gate closed, switch off, chest sealed).
    Idle,
    /// Active state (gate open, switch on, chest opened, grab held).
    Active,
    /// Cooldown before returning to Idle (one-shot switches, etc.)
    Cooldown,
}

#[table(accessor = interactable_config, public)]
pub struct InteractableConfig {
    #[primary_key]
    pub entity_id: u64,
    pub interact_kind: InteractKind,
    /// Entity this interactable controls (e.g. switch → gate entity).
    pub linked_entity: Option<u64>,
    /// Stable script identifier from the dungeon template.
    pub script_id: Option<String>,
    /// Stable tags from the dungeon template.
    pub tags: Vec<String>,
    /// Buff required to interact. None = no requirement.
    pub required_buff: Option<u32>,
    /// Item required to interact. None = no requirement.
    pub required_item: Option<u32>,
    /// Max interaction distance. Default 3.0.
    pub interact_range: f32,
    /// Timed multi-lever puzzle group. None = not part of a puzzle.
    pub puzzle_group: Option<String>,
    /// Number of levers required within the window. 0 = infer group size.
    pub puzzle_required_count: u32,
    /// Window in ticks for timed puzzle activation. 0 = no timed puzzle.
    pub puzzle_window_ticks: u32,
    pub state: InteractState,
    /// Authoritative physics body shape for this interactable, encoded as a
    /// `BodyShape` discriminant. Always populated by `create_instance` from
    /// the dungeon `InteractableDef.body_shape` (or a default per
    /// `InteractKind`).
    pub body_shape: u8,
}

// ── Simulation Diagnostics ──────────────────────────────────────────
// Worker-emitted warning/error messages, piggy-backed on commit_tick_results.
// Declared as an event table so rows are auto-deleted after broadcast —
// subscribers see them in real time without any accumulation in persistent
// storage. Stream live with:
//   spacetime subscribe <module> "SELECT * FROM sim_log"
// level: 1=Info 2=Warn 3=Error

#[table(accessor = sim_log, public, event)]
pub struct SimLog {
    #[primary_key]
    #[auto_inc]
    pub log_id: u64,
    #[index(btree)]
    pub tick_id: u64,
    /// Severity: 1=Info 2=Warn 3=Error
    pub level: u8,
    pub message: String,
}

// ── Voxel Terrain (§4.8b Phase 1) ───────────────────────────────────
// Bulk baked terrain rows are rekeyed by `terrain_set_id` (NOT layer) so
// multiple layers can share one bake (PvE/PvP forks of the same map, etc.).
// `WorldLayerDef.terrain_set` and `DungeonTemplate.terrain_set` carry the
// human-readable name; the worker resolves it to `terrain_set_id` via the
// unique-name index on `terrain_set`.
//
// SpacetimeDB v2 allows only one `#[primary_key]` per table; logical
// composite keys are expressed as a surrogate `row_id` plus a multi-column
// btree index, matching the `entity_region` / `zone_counter` pattern above.
// Logical uniqueness on `(terrain_set_id, chunk_morton[, voxel_idx])` is
// enforced by the upsert reducers, not the storage layer.

#[table(accessor = terrain_set, public)]
pub struct TerrainSet {
    #[primary_key]
    #[auto_inc]
    pub terrain_set_id: u32,
    /// Human-readable name referenced by `WorldLayerDef.terrain_set` and
    /// `DungeonTemplate.terrain_set`. Unique.
    #[unique]
    pub name: String,
    /// Editor-bake artefact hash (hex). Bumps on any chunk change inside the set.
    pub content_hash: String,
    /// Combined content + bake-format version. Split into two fields if the
    /// mesher schema diverges from semantic content versioning.
    pub version: u32,
}

#[table(
    accessor = terrain_chunk,
    public,
    index(accessor = by_set_chunk, btree(columns = [terrain_set_id, chunk_morton]))
)]
pub struct TerrainChunk {
    #[primary_key]
    #[auto_inc]
    pub row_id: u64,
    pub terrain_set_id: u32,
    /// Canonical 21-bit zyx Morton key — see `game_schema::morton`.
    pub chunk_morton: u64,
    /// Flat XYZ vertices, length divisible by 3.
    pub vertices: Vec<f32>,
    /// Triangle list, length divisible by 3.
    pub indices: Vec<u32>,
    /// Level of detail. Server collision always uses LOD 0; field is
    /// reserved for client mesh selection / future per-chunk variants.
    pub lod: u8,
}

#[table(
    accessor = terrain_manifest,
    public,
    index(accessor = by_set_chunk, btree(columns = [terrain_set_id, chunk_morton]))
)]
pub struct TerrainManifest {
    #[primary_key]
    #[auto_inc]
    pub row_id: u64,
    pub terrain_set_id: u32,
    pub chunk_morton: u64,
    /// Per-chunk content hash for client cache validation.
    pub content_hash: String,
    pub version: u32,
}

#[table(
    accessor = terrain_core,
    public,
    index(accessor = by_set_chunk_voxel, btree(columns = [terrain_set_id, chunk_morton, voxel_idx]))
)]
pub struct TerrainCore {
    #[primary_key]
    #[auto_inc]
    pub row_id: u64,
    pub terrain_set_id: u32,
    pub chunk_morton: u64,
    /// Flat per-voxel index inside the chunk: `x + y*N + z*N*N`.
    pub voxel_idx: u32,
    pub hermite_normal_x: f32,
    pub hermite_normal_y: f32,
    pub hermite_normal_z: f32,
    pub qef_offset_x: f32,
    pub qef_offset_y: f32,
    pub qef_offset_z: f32,
    pub material_id: u16,
    /// Mesher-defined bitfield (surface-cell, sharp-feature, hidden, …).
    /// Layout documented when the voxel editor lands.
    pub flags: u32,
}
