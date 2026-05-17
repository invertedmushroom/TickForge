//! Simulation coordinator — connects to SpacetimeDB as a client and drives the tick pipeline.
//!
//! # Prerequisites
//!
//! 1. Generate module bindings:
//!    ```sh
//!    spacetime generate --lang rust \
//!        --out-dir crates/simulation_worker/src/module_bindings \
//!        --module-path crates/server_module
//!    ```
//!
//! 2. Build with the `connected` feature:
//!    ```sh
//!    cargo build -p simulation_worker --features connected
//!    ```
//!
//! # Architecture
//!
//! The coordinator:
//!   1. Connects to SpacetimeDB via WebSocket.
//!   2. Registers itself as a trusted worker (`register_worker` reducer).
//!   3. Subscribes to `sim_tick` and `player_intent` tables.
//!   4. On each `sim_tick` update, gathers pending intents, runs the tick pipeline,
//!      and calls `commit_tick_results` to push authoritative state.
//!
//! # Callback ownership
//!
//!   `subscription.on_applied` — Seed coordinator baselines only. No entity mutations.
//!   `entity.on_insert`        — Single spawn gate. `contains()` guard for idempotency.
//!   `entity.on_update`        — Mirror external lifecycle transitions only.
//!   `entity.on_delete`        — Force cleanup guard only.
//!   `sim_tick.on_insert`      — Single tick gate. Advance `last_processed_tick` only on
//!                               successful commit, preserving the truth boundary.

use std::sync::{Arc, Mutex};
use std::path::Path;

use log::{debug, error, info, warn};
use spacetimedb_sdk::{DbContext, Identity, Table, TableWithPrimaryKey};

use crate::module_bindings::*;
use crate::physics::rapier_world::PhysicsWorld;
use crate::tick_pipeline::TickPipeline;
use game_core::combat::skill::{
    AbilityAction, AbilityData, AbilityRegistry, AbilityTimeline, ScheduledAbilityAction, SkillShape,
};
use game_protocol::entity_id::EntityId;
use game_protocol::event::EventPayload;
use game_protocol::tick::TickId;

/// Configuration for connecting the worker to SpacetimeDB.
pub struct CoordinatorConfig {
    /// SpacetimeDB host URI (e.g. "http://localhost:3000").
    pub uri: String,
    /// Module name or Identity published to SpacetimeDB.
    pub module_name: String,
    /// Optional auth token for reconnecting with the same Identity.
    pub auth_token: Option<String>,
}

/// Shared mutable state accessed from subscription callbacks.
struct CoordinatorState {
    pipeline: TickPipeline,
    last_processed_tick: u64,
}

const TOKEN_FILE: &str = ".worker_token";

/// Load a previously-saved auth token from disk.
fn load_token() -> Option<String> {
    std::fs::read_to_string(TOKEN_FILE).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Save the auth token so the worker keeps the same identity across restarts.
fn save_token(token: &str) {
    if let Err(e) = std::fs::write(TOKEN_FILE, token) {
        warn!("Failed to save auth token to {TOKEN_FILE}: {e}");
    } else {
        info!("Auth token saved to {TOKEN_FILE}");
    }
}

/// Start the coordinator loop. This blocks the current thread.
pub fn run(config: CoordinatorConfig) {
    let tick_dt = 1.0 / 20.0; // 20 Hz — must match server TickConfig
    let physics = PhysicsWorld::new(tick_dt);
    let abilities = load_abilities();
    let pipeline = TickPipeline::new(TickId(0), Box::new(physics), tick_dt, abilities);

    let state = Arc::new(Mutex::new(CoordinatorState {
        pipeline,
        last_processed_tick: 0,
    }));

    let state_for_connect = Arc::clone(&state);
    let state_for_tick = Arc::clone(&state);
    let state_for_entity = Arc::clone(&state);

    // Use persisted token if no explicit token provided.
    let auth_token = config.auth_token.or_else(load_token);
    if auth_token.is_some() && Path::new(TOKEN_FILE).exists() {
        info!("Using persisted auth token from {TOKEN_FILE}");
    }

    let conn = DbConnection::builder()
        .with_uri(&config.uri)
        .with_database_name(&config.module_name)
        .with_token(auth_token.as_deref())
        .on_connect(move |ctx: &DbConnection, identity: Identity, token: &str| {
            save_token(token);

            info!("========================================");
            info!("  Worker identity: {identity}");
            info!("========================================");
            info!("Register once with:");
            info!(r#"  spacetime call jump register_worker '{{"__identity__":"0x{identity}"}}' -s local"#);

            subscribe_to_tables(ctx, Arc::clone(&state_for_connect));
        })
        .on_connect_error(|_ctx, err| {
            error!("Connection failed: {err}");
        })
        .on_disconnect(|_ctx, err| {
            if let Some(e) = err {
                error!("Disconnected with error: {e}");
            } else {
                info!("Disconnected gracefully");
            }
        })
        .build()
        .expect("Failed to build DbConnection");

    // Watch for sim_tick inserts — each new tick row triggers a pipeline run.
    //
    // The SpacetimeDB SDK docs explicitly state there is **no ordering guarantee**
    // between `on_insert` and `on_applied` callbacks for initial subscription rows.
    // Checking `ctx.event == Event::SubscribeApplied` is the documented pattern
    // (matching `ctx.Event is not Event<Reducer>.SubscribeApplied` in the C# tutorial)
    // to distinguish initial snapshot rows from live reducer-inserted rows.
    // Historical sim_tick rows must be skipped — they are already committed.
    conn.db.sim_tick().on_insert(move |ctx, new_tick| {
        // Initial subscription snapshot — skip, only process live tick reducer inserts.
        if matches!(ctx.event, spacetimedb_sdk::Event::SubscribeApplied) {
            return;
        }
        let canonical_tick = new_tick.tick_id;

        // Gather intents targeting this tick from the client cache.
        // Collect (intent_id, pipeline_intent) together so we can delete consumed rows in commit.
        let (consumed_ids, intents): (Vec<u64>, Vec<game_protocol::intent::PlayerIntent>) = ctx
            .db
            .player_intent()
            .iter()
            .filter(|i| i.target_tick == canonical_tick)
            .map(|row| (row.intent_id, game_protocol::intent::PlayerIntent {
                client_id: EntityId(row.entity_id),
                sequence_id: row.sequence_id,
                target_tick: TickId(row.target_tick),
                client_time_ms: row.client_time_ms,
                action: convert_intent_action(row.action.clone()),
            }))
            .unzip();

        let mut guard = state_for_tick.lock().unwrap();

        // Skip if we already processed this tick (idempotency).
        if canonical_tick <= guard.last_processed_tick {
            debug!("Tick {canonical_tick} already processed, skipping");
            return;
        }

        // Guard: pipeline must be in sync with the canonical tick.
        // If they differ, the inner target_tick filter in run_tick would silently
        // drop every intent for this tick.  on_applied seeds pipeline.current_tick
        // to max_tick+1; each successful run_tick advances it by one.
        // A mismatch here indicates a gap (skipped tick) — align and warn.
        let pipeline_tick = guard.pipeline.current_tick();
        if pipeline_tick != TickId(canonical_tick) {
            warn!(
                "tick desync: canonical={canonical_tick} pipeline={} — advancing pipeline to match",
                pipeline_tick.0
            );
            guard.pipeline.set_current_tick(TickId(canonical_tick));
        }

        let result = guard.pipeline.run_tick(&intents);
        let summary = result.summary;

        // NOTE: last_processed_tick is NOT advanced here — only after a successful commit.
        // If commit fails the tick remains unacknowledged and can be retried on reconnect.
        // Advancing before commit (Bug #13) would permanently lose the tick.

        // Marshal transforms as typed structs.
        let transforms: Vec<TransformUpdate> = result
            .transforms
            .iter()
            .map(|(eid, t)| TransformUpdate {
                entity_id: eid.0,
                pos_x: t.position.x, pos_y: t.position.y, pos_z: t.position.z,
                rot_x: t.rotation.x, rot_y: t.rotation.y, rot_z: t.rotation.z, rot_w: t.rotation.w,
                vel_x: t.linear_velocity.x, vel_y: t.linear_velocity.y, vel_z: t.linear_velocity.z,
                angvel_x: t.angular_velocity.x, angvel_y: t.angular_velocity.y, angvel_z: t.angular_velocity.z,
            })
            .collect();

        // Classify pipeline events into typed wire types for the commit reducer.
        let (combat_events, world_events) = classify_events(&result.events);

        // Marshal entity lifecycle transitions captured by Phase 8.
        let entity_state_updates: Vec<EntityStateUpdate> = result.entity_state_updates
            .iter()
            .map(|(eid, state)| EntityStateUpdate {
                entity_id: eid.0,
                new_state: convert_entity_state(*state),
            })
            .collect();

        // Marshal health deltas — entities whose hp changed this tick (includes hp=0 for deaths).
        let health_updates: Vec<HealthUpdate> = result.health_updates
            .iter()
            .map(|(eid, hp, max_hp)| HealthUpdate { entity_id: eid.0, hp: *hp, max_hp: *max_hp })
            .collect();

        // Commit results to SpacetimeDB.
        let commit_ok = match ctx.reducers.commit_tick_results(
            result.tick_id.0,
            transforms,
            health_updates,
            combat_events,
            world_events,
            consumed_ids,
            entity_state_updates,
            Vec::new(), // region_updates
        ) {
            Ok(_) => true,
            Err(e) => { error!("commit_tick_results failed tick={canonical_tick}: {e}"); false }
        };

        // Advance tick cursor only after a successful commit (Bug #13 fix).
        // A failed commit leaves last_processed_tick unchanged so the tick is not
        // silently dropped — the worker will halt at reconnect and resync cleanly.
        if commit_ok {
            guard.last_processed_tick = canonical_tick;
        } else {
            error!("tick={canonical_tick} commit failed — last_processed_tick not advanced; worker will resync on reconnect");
        }

        // Structured tick summary — one line when interesting or every 20 ticks.
        if canonical_tick % 20 == 0 || summary.damage_events > 0 || summary.deaths > 0
            || summary.despawns > 0 || summary.intents_processed > 0
        {
            info!(
                "tick={canonical_tick} intents={} contacts={} damage={} deaths={} despawns={} entities={} hitboxes={} commit={}",
                summary.intents_processed, summary.contacts, summary.damage_events,
                summary.deaths, summary.despawns, summary.active_entities, summary.active_hitboxes,
                if commit_ok { "ok" } else { "err" }
            );
        }

        // Periodically prune old combat/world event rows so the event tables don't
        // grow unbounded. Keep a 2-second window (40 ticks at 20 Hz) so clients
        // that are slightly behind still receive events before they are deleted.
        // Runs every 100 ticks (5 s) to keep the per-tick cost negligible.
        const EVENT_RETAIN_TICKS: u64 = 40;
        const EVENT_PRUNE_INTERVAL: u64 = 100;
        if commit_ok && canonical_tick % EVENT_PRUNE_INTERVAL == 0
            && canonical_tick > EVENT_RETAIN_TICKS
        {
            let before_tick = canonical_tick - EVENT_RETAIN_TICKS;
            if let Err(e) = ctx.reducers.clear_events(before_tick) {
                warn!("clear_events(before_tick={before_tick}) failed: {e}");
            }
        }
    });

    // entity.on_insert is the single spawn gate.
    //
    // SpacetimeDB fires on_insert for every row in the initial subscription batch
    // (after on_applied returns) AND for every row inserted during live operation.
    // The `contains()` guard makes this idempotent against both cases, with no
    // special-casing needed for fresh start vs. worker restart.
    conn.db.entity().on_insert(move |ctx, new_entity| {
        let eid = EntityId(new_entity.entity_id);

        // Guard: never spawn entities that have already passed their useful lifecycle.
        // On worker restart the subscription snapshot contains every row including
        // Removed and DespawnPending entities. Without this guard they would enter
        // SimState as Spawning, and Phase 8 would activate them the next tick —
        // resurrecting dead NPCs and players.
        match new_entity.state {
            EntityState::Removed | EntityState::DespawnPending => {
                debug!("Entity {} is {:?} in DB — skipping spawn", eid.0, new_entity.state);
                return;
            }
            _ => {}
        }

        // Look up companion rows before acquiring the state lock — these reads
        // are from the SDK cache (no contention) and must not be done under the
        // lock to avoid holding it across I/O.
        let kind = convert_entity_kind(new_entity.kind);
        let tick = TickId(new_entity.spawned_at_tick);

        let max_hp = ctx.db
            .entity_health()
            .entity_id()
            .find(&new_entity.entity_id)
            .map(|h| h.max_hp)
            .unwrap_or(100.0);

        let pos = ctx.db
            .entity_transform()
            .entity_id()
            .find(&new_entity.entity_id)
            .map(|t| game_protocol::types::Vec3f { x: t.pos_x, y: t.pos_y, z: t.pos_z })
            .unwrap_or(game_protocol::types::Vec3f { x: 0.0, y: 1.0, z: 0.0 });

        // Single lock acquisition — check and spawn under the same guard so no
        // other callback thread can slip in between (eliminates the previous
        // read-lock → release → write-lock race window).
        let mut guard = state_for_entity.lock().unwrap();
        if guard.pipeline.state.entities.contains(eid) {
            debug!("Entity {} already tracked — ignoring duplicate on_insert", eid.0);
            return;
        }
        guard.pipeline.spawn_entity_from_snapshot(eid, kind, tick, max_hp, pos);
        info!("Entity {} ({kind:?}) spawned into simulation at tick {:?} pos=({},{},{})",
            eid.0, tick, pos.x, pos.y, pos.z);
    });

    // Watch for external entity state changes (e.g. force-despawn from a server reducer).
    // The simulation is authoritative for Spawning→Active (Phase 8) and for normal
    // combat deaths, so we only act on transitions that the pipeline didn't initiate.
    let state_for_entity_update = Arc::clone(&state);
    conn.db.entity().on_update(move |_ctx, old_entity, new_entity| {
        let eid = EntityId(new_entity.entity_id);
        let mut guard = state_for_entity_update.lock().unwrap();
        match (old_entity.state, new_entity.state) {
            // External reducer set DespawnPending without going through combat death.
            (EntityState::Active, EntityState::DespawnPending) => {
                if guard.pipeline.state.is_active(eid) {
                    guard.pipeline.state.mark_despawn(eid);
                    info!("Entity {} externally marked DespawnPending — mirrored to simulation", eid.0);
                }
            }
            // Hard removal by a server-side reducer; skip if already cleaned up.
            (_, EntityState::Removed) => {
                if guard.pipeline.state.entities.lookup(eid).is_some() {
                    let removed = guard.pipeline.force_remove_entity(eid);
                    if removed {
                        info!("Entity {} externally set Removed — cleaned up from simulation", eid.0);
                    } else {
                        info!("Entity {} externally set Removed — no-op (not present)", eid.0);
                    }
                }
            }
            _ => {} // Spawning→Active handled by pipeline Phase 8; other transitions ignored.
        }
    });

    // Watch for hard row deletions (cleanup reducers, admin tools, etc.).
    // In the normal despawn flow the simulation already called remove_entity before the
    // DB row is deleted, so remove_entity returns false and this is a cheap no-op.
    let state_for_entity_delete = Arc::clone(&state);
    conn.db.entity().on_delete(move |_ctx, deleted_entity| {
        let eid = EntityId(deleted_entity.entity_id);
        let mut guard = state_for_entity_delete.lock().unwrap();
        let removed = guard.pipeline.force_remove_entity(eid);
        if removed {
            warn!("Entity {} row deleted while still in simulation — forced cleanup", eid.0);
        }
    });

    // Block on the connection thread — the callbacks above drive the simulation.
    conn.run_threaded().join().expect("Connection thread panicked");
}

/// Subscribe to the tables the coordinator needs to observe.
///
/// Ordering note (SDK docs): The SpacetimeDB SDK makes **no ordering guarantee**
/// between `on_insert` (called for each initial row) and `on_applied`. To avoid
/// replaying historical sim_tick rows, `sim_tick.on_insert` checks
/// `ctx.event == Event::SubscribeApplied` and skips those rows entirely.
/// `on_applied` seeds `last_processed_tick` as defense-in-depth.
///
/// Entity spawning (`entity.on_insert`) processes initial rows normally — the
/// `contains()` guard in that callback provides idempotency against both
/// initial-subscription and live-insert events.
fn subscribe_to_tables(ctx: &DbConnection, state: Arc<Mutex<CoordinatorState>>) {
    ctx.subscription_builder()
        .on_applied(move |ctx| {
            // Seed last_processed_tick to the highest committed tick visible in the
            // subscription cache. This prevents historical sim_tick.on_insert callbacks
            // from replaying already-processed ticks after a worker restart.
            let max_tick = ctx.db.sim_tick().iter().map(|t| t.tick_id).max().unwrap_or(0);
            {
                let mut guard = state.lock().unwrap();
                guard.last_processed_tick = max_tick;
                // Sync the pipeline's internal tick counter to the next live tick.
                // The coordinator will deliver canonical_tick = max_tick + 1 first via
                // sim_tick.on_insert.  The inner filter in run_tick compares
                // i.target_tick == self.current_tick, so they must agree or every
                // intent is silently dropped.
                guard.pipeline.set_current_tick(TickId(max_tick + 1));
            }
            info!(
                "Subscription applied — {} sim_tick rows, {} intent rows, {} entity rows; seeding last_processed_tick={} pipeline_start_tick={}",
                ctx.db.sim_tick().count(),
                ctx.db.player_intent().count(),
                ctx.db.entity().count(),
                max_tick,
                max_tick + 1,
            );
            // Entity rows are materialised by entity.on_insert (idempotent via contains() guard).
            // sim_tick rows are skipped by the Event::SubscribeApplied guard in that callback.
        })
        .on_error(|_ctx, err| {
            error!("Subscription error: {err}");
        })
        .subscribe([
            "SELECT * FROM sim_tick",
            "SELECT * FROM player_intent",
            "SELECT * FROM entity",
            "SELECT * FROM entity_health",
            "SELECT * FROM entity_transform",
        ]);
}

// ── Ability registry ───────────────────────────────────────────
//
// Hardcoded ability definitions for the initial vertical slice.
// All abilities use ability_id=1 by convention until a data loading
// pipeline (roadmap #4) replaces this with JSON/RON files.
//
// Timeline: 20 Hz ticks, so tick offsets map as follows:
//   offset 0  → SpawnHitbox (sensor created)
//   offset 0  → CooldownStart (20 ticks = 1 s cooldown)
//   offset 1  → ApplyDamageFrame (combat reads hitbox contacts)
//   offset 2  → RemoveHitbox (sensor freed)

/// On-disk serialization format for `data/abilities.ron`.
/// Both `AbilityData` and `AbilityTimeline` already derive `serde::Deserialize`.
#[derive(serde::Deserialize)]
struct AbilityFile {
    abilities: Vec<AbilityData>,
    timelines: Vec<AbilityTimeline>,
}

/// Load abilities from `data/abilities.ron`.
/// Falls back to the hardcoded registry if the file is missing or malformed.
fn load_abilities() -> AbilityRegistry {
    const PATH: &str = "data/abilities.ron";
    let result = std::fs::read_to_string(PATH)
        .map_err(|e| format!("read '{PATH}': {e}"))
        .and_then(|src| ron::from_str::<AbilityFile>(&src).map_err(|e| format!("parse '{PATH}': {e}")));
    match result {
        Ok(file) => {
            let count = file.abilities.len();
            let mut reg = AbilityRegistry::new();
            for a in file.abilities { reg.register(a); }
            for t in file.timelines { reg.register_timeline(t); }
            info!("Loaded {count} ability/abilities from {PATH}");
            reg
        }
        Err(e) => {
            warn!("Could not load {PATH} ({e}) — using hardcoded fallback");
            build_ability_registry()
        }
    }
}

fn build_ability_registry() -> AbilityRegistry {
    let mut reg = AbilityRegistry::new();

    // Ability 1: Slash — fast melee capsule sweep.
    reg.register(AbilityData {
        ability_id: 1,
        name: "Slash".to_string(),
        base_damage: 25.0,
        damage_type: game_schema::DamageType::Physical,
        shape: SkillShape::CapsuleSweep,
        threat_multiplier: 1.0,
    });
    reg.register_timeline(AbilityTimeline {
        ability_id: 1,
        actions: vec![
            ScheduledAbilityAction {
                tick_offset: 0,
                action: AbilityAction::SpawnHitbox {
                    shape: SkillShape::CapsuleSweep,
                    offset: game_schema::Vec3f { x: 0.0, y: 0.0, z: 0.0 },
                },
            },
            ScheduledAbilityAction {
                tick_offset: 0,
                action: AbilityAction::CooldownStart { duration_ticks: 20 },
            },
            ScheduledAbilityAction {
                tick_offset: 1,
                action: AbilityAction::ApplyDamageFrame,
            },
            ScheduledAbilityAction {
                tick_offset: 2,
                action: AbilityAction::RemoveHitbox,
            },
        ],
    });

    reg
}

// ── Binding conversions ─────────────────────────────────────────
//
// The generated bindings produce mirror types of game_schema types.
// We convert between them here since they are distinct Rust types.

/// Convert generated binding's IntentAction → game_protocol's IntentAction.
fn convert_intent_action(action: crate::module_bindings::IntentAction) -> game_protocol::intent::IntentAction {
    match action {
        crate::module_bindings::IntentAction::Move(d) => {
            game_protocol::intent::IntentAction::Move(game_schema::MoveDir {
                dir_x: d.dir_x,
                dir_y: d.dir_y,
                dir_z: d.dir_z,
            })
        }
        crate::module_bindings::IntentAction::UseAbility(u) => {
            game_protocol::intent::IntentAction::UseAbility(game_schema::UseAbilityData {
                ability_id: u.ability_id,
                target: convert_ability_target(u.target),
            })
        }
        crate::module_bindings::IntentAction::Stop => game_protocol::intent::IntentAction::Stop,
        crate::module_bindings::IntentAction::FaceTo(d) => {
            game_protocol::intent::IntentAction::FaceTo(game_schema::MoveDir {
                dir_x: d.dir_x,
                dir_y: d.dir_y,
                dir_z: d.dir_z,
            })
        }
        crate::module_bindings::IntentAction::Interact(id) => {
            game_protocol::intent::IntentAction::Interact(id)
        }
    }
}

fn convert_ability_target(target: crate::module_bindings::AbilityTarget) -> game_schema::AbilityTarget {
    match target {
        crate::module_bindings::AbilityTarget::None => game_schema::AbilityTarget::None,
        crate::module_bindings::AbilityTarget::Entity(id) => game_schema::AbilityTarget::Entity(id),
        crate::module_bindings::AbilityTarget::Position(v) => {
            game_schema::AbilityTarget::Position(game_schema::Vec3f { x: v.x, y: v.y, z: v.z })
        }
        crate::module_bindings::AbilityTarget::Direction(v) => {
            game_schema::AbilityTarget::Direction(game_schema::Vec3f { x: v.x, y: v.y, z: v.z })
        }
    }
}

/// Convert game_schema DamageType → generated binding's DamageType.
fn convert_damage_type(dt: game_schema::DamageType) -> DamageType {
    match dt {
        game_schema::DamageType::Physical => DamageType::Physical,
        game_schema::DamageType::Magical => DamageType::Magical,
        game_schema::DamageType::True => DamageType::True,
    }
}

/// Convert generated binding's EntityKind → game_schema's EntityKind.
fn convert_entity_kind(kind: crate::module_bindings::EntityKind) -> game_schema::EntityKind {
    match kind {
        crate::module_bindings::EntityKind::Player => game_schema::EntityKind::Player,
        crate::module_bindings::EntityKind::Npc => game_schema::EntityKind::Npc,
        crate::module_bindings::EntityKind::Projectile => game_schema::EntityKind::Projectile,
        crate::module_bindings::EntityKind::Hazard => game_schema::EntityKind::Hazard,
        crate::module_bindings::EntityKind::Boss => game_schema::EntityKind::Boss,
    }
}

/// Convert game_schema EntityState → generated binding's EntityState.
fn convert_entity_state(state: game_schema::EntityState) -> EntityState {
    match state {
        game_schema::EntityState::Spawning => EntityState::Spawning,
        game_schema::EntityState::Active => EntityState::Active,
        game_schema::EntityState::DespawnPending => EntityState::DespawnPending,
        game_schema::EntityState::Removed => EntityState::Removed,
    }
}

/// Convert pipeline `SimEvent`s into wire `CombatEventInput` and `WorldEventInput`.
///
/// Made `pub` so integration tests can exercise end-to-end preservation of
/// `event_sequence` during marshalling.
pub fn classify_events(events: &[game_protocol::event::SimEvent]) -> (Vec<CombatEventInput>, Vec<WorldEventInput>) {
    let mut combat_events: Vec<CombatEventInput> = Vec::new();
    let mut world_events: Vec<WorldEventInput> = Vec::new();

    for e in events {
        match &e.payload {
            EventPayload::Damage { source, amount, damage_type } => {
                combat_events.push(CombatEventInput {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CombatEventKind::Damage(DamageData {
                        amount: *amount,
                        damage_type: convert_damage_type(*damage_type),
                    }),
                });
            }
            EventPayload::SkillHit { skill_id, source } => {
                combat_events.push(CombatEventInput {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CombatEventKind::SkillHit(*skill_id),
                });
            }
            EventPayload::BuffApplied { buff_id, source, duration_ticks } => {
                combat_events.push(CombatEventInput {
                    source_entity: source.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CombatEventKind::BuffApplied(BuffAppliedData {
                        buff_id: *buff_id,
                        duration_ticks: *duration_ticks,
                    }),
                });
            }
            EventPayload::BuffExpired { buff_id } => {
                combat_events.push(CombatEventInput {
                    source_entity: e.entity_id.0,
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CombatEventKind::BuffExpired(*buff_id),
                });
            }
            EventPayload::EntityDied { killer } => {
                combat_events.push(CombatEventInput {
                    source_entity: killer.map(|k| k.0).unwrap_or(0),
                    target_entity: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: CombatEventKind::EntityDied(killer.map(|k| k.0)),
                });
            }
            EventPayload::EntityDespawned => {
                world_events.push(WorldEventInput {
                    entity_id: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: WorldEventKind::EntityDespawned,
                });
            }
            EventPayload::PickupCollected { item_id } => {
                world_events.push(WorldEventInput {
                    entity_id: e.entity_id.0,
                    event_sequence: e.event_sequence,
                    event_kind: WorldEventKind::PickupCollected(*item_id),
                });
            }
            // Internal pipeline events — not committed to DB.
            EventPayload::EntitySpawned
            | EventPayload::HitboxSpawned { .. }
            | EventPayload::DamageFrame { .. }
            | EventPayload::HitboxRemoved { .. }
            | EventPayload::CooldownReady { .. }
            | EventPayload::TickBoundary => {}
        }
    }

    (combat_events, world_events)
}
