//! SpacetimeDB-facing coordinator for the simulation worker.
//!
//! Owns connection/subscription callbacks, gathers intents for each canonical
//! tick, delegates simulation to `SimulationRunner`, and forwards commits via
//! reducers with retry/ack handling.

use std::path::Path;
use std::sync::{Arc, Mutex};

use log::{debug, error, info, warn};
use spacetimedb_sdk::{DbContext, Identity, Table, TableWithPrimaryKey};

use crate::commit_authority::FailureAction;
use crate::commit_builder::{
    self, CommitCombatEvent, CommitCombatEventKind, CommitEntityStateKind, CommitPackage,
    CommitWorldEventKind,
};
use crate::entity_sync::EntitySync;
use crate::module_bindings::*;
use crate::physics::rapier_world::PhysicsWorld;
use crate::simulation_runner::SimulationRunner;
use game_core::combat::skill::{
    AbilityAction, AbilityData, AbilityFile, AbilityRegistry, AbilityTimeline,
    ScheduledAbilityAction, SkillShape, TargetFilter,
};
use game_core::combat::status::BuffRegistry;
use game_protocol::entity_id::EntityId;
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
    sim: SimulationRunner,
    items: game_core::stats::ItemRegistry,
    dungeons: game_core::dungeon::DungeonRegistry,
    encounters: game_core::encounter::EncounterRegistry,
    /// Parsed `data/spawn_rules.ron` (Phase 7.5).  Consumed at on_applied to
    /// populate open-world director events, and by the `instance` subscription
    /// callbacks to register dungeon-scoped rules on instance creation.
    spawn_rules: game_core::spawn_rules::SpawnRulesRegistry,
    /// Voxel-terrain bindings, cached row→collider mapping, and the deferred
    /// edit queue. See `TerrainState` (§4.8b Phase 5).
    terrain: TerrainState,
}

/// Send (or re-send) a commit payload to SpacetimeDB.
///
/// On transient failure the function retries immediately from the ack
/// callback — no dependency on `sim_tick.on_insert`, so it cannot
/// deadlock with server-side backpressure.
///
/// On `FailureAction::Exhausted` (all retries spent) the process exits
/// so the supervisor can restart the worker with a clean reseed.
fn send_commit(reducers: &RemoteReducers, pkg: CommitPackage, state: Arc<Mutex<CoordinatorState>>) {
    let tick_id = pkg.tick_id;
    let transforms = wire_transforms(&pkg);
    let health_updates = wire_health_updates(&pkg);
    let combat_events = wire_combat_events(&pkg);
    let world_events = wire_world_events(&pkg);
    let consumed_intent_ids = pkg.consumed_intent_ids.clone();
    let entity_state_updates = wire_entity_state_updates(&pkg);
    let region_updates = wire_region_updates(&pkg);
    let buff_updates = wire_buff_updates(&pkg);
    let buff_cleared_entity_ids = pkg.buff_cleared_entity_ids.clone();
    let npc_state_updates = wire_npc_state_updates(&pkg);
    let director_spawns = wire_director_spawns(&pkg);
    let encounter_memberships = wire_encounter_memberships(&pkg);
    let interactable_updates = wire_interactable_updates(&pkg);
    let death_state_inserts = wire_death_state_inserts(&pkg);
    let sim_log_entries = wire_sim_log_entries(&pkg);
    let boss_phase_updates = pkg.boss_phase_updates.clone();
    let zone_counter_deltas = pkg.zone_counter_deltas.clone();

    let state_for_ack = Arc::clone(&state);

    let send_result = reducers.commit_tick_results_then(
        tick_id,
        transforms,
        health_updates,
        combat_events,
        world_events,
        consumed_intent_ids,
        entity_state_updates,
        region_updates,
        buff_updates,
        buff_cleared_entity_ids,
        npc_state_updates,
        director_spawns,
        encounter_memberships,
        interactable_updates,
        death_state_inserts,
        sim_log_entries,
        boss_phase_updates
            .into_iter()
            .map(|(boss_entity_id, phase, entered_at_tick)| {
                crate::module_bindings::BossPhaseUpdateInput {
                    boss_entity_id,
                    phase,
                    entered_at_tick,
                }
            })
            .collect(),
        zone_counter_deltas
            .into_iter()
            .map(|(layer, region_x, region_z, counter_name, delta)| {
                crate::module_bindings::ZoneCounterDeltaInput {
                    layer,
                    region_x,
                    region_z,
                    counter_name,
                    delta,
                }
            })
            .collect(),
        move |rctx, outcome| {
            let reason = match &outcome {
                Ok(Ok(())) => {
                    let mut guard = state_for_ack.lock().unwrap_or_else(|p| p.into_inner());
                    guard.sim.acknowledge_success(tick_id);
                    return;
                }
                Ok(Err(reducer_err)) => format!("reducer rejected: {reducer_err}"),
                Err(internal_err) => format!("internal error: {internal_err:?}"),
            };

            let action = {
                let mut guard = state_for_ack.lock().unwrap_or_else(|p| p.into_inner());
                guard.sim.acknowledge_failure(tick_id, &reason)
            };

            match action {
                FailureAction::Retry => {
                    send_commit(&rctx.reducers, pkg, state_for_ack);
                }
                FailureAction::Exhausted => {
                    error!(
                        "tick={tick_id} commit permanently failed — \
                         crashing for clean reseed"
                    );
                    std::process::exit(1);
                }
            }
        },
    );

    // Handle send failure (unable to enqueue the reducer call at all).
    if let Err(e) = send_result {
        let action = {
            let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
            guard
                .sim
                .acknowledge_failure(tick_id, &format!("send failed: {e}"))
        };

        match action {
            FailureAction::Retry => {
                warn!("tick={tick_id} send failed, retrying immediately");
                // Cannot retry send without a reducers reference (the
                // original borrow is from the on_insert ctx which we
                // can't capture here).  Crash for reseed — the send
                // path failing means the connection is likely broken.
                error!(
                    "tick={tick_id} send failed and cannot retry in-band — \
                     crashing for clean reseed"
                );
                std::process::exit(1);
            }
            FailureAction::Exhausted => {
                error!(
                    "tick={tick_id} send permanently failed — \
                     crashing for clean reseed"
                );
                std::process::exit(1);
            }
        }
    }
}

const TOKEN_FILE: &str = ".worker_token";

/// Load a previously-saved auth token from disk.
fn load_token() -> Option<String> {
    std::fs::read_to_string(TOKEN_FILE)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
    let mut physics = PhysicsWorld::new(tick_dt);
    let abilities = load_abilities();
    let items = load_items();
    let buffs = load_buffs();
    let dungeons = load_dungeons();
    let encounters = load_encounters();
    let spawn_rules = load_spawn_rules();

    // Materialise named static layers (open world, hubs, …) from
    // `data/layers.ron`. Replaces the previously-hardcoded layer-0
    // placeholder floor in `PhysicsWorld::new` and gives every static
    // layer the same compositional `geometry + Option<terrain_set>`
    // shape used by `DungeonTemplate`.
    let world_layers = load_world_layers();
    let mut initial_terrain_bindings: Vec<TerrainBinding> = Vec::new();
    for layer_def in &world_layers {
        materialize_layer(
            &mut physics,
            layer_def.layer_id,
            &layer_def.geometry,
            layer_def.terrain_set.as_deref(),
            layer_def.collision_policy,
        );
        if let Some(set_name) = layer_def.terrain_set.as_deref() {
            initial_terrain_bindings.push(TerrainBinding {
                layer: layer_def.layer_id,
                set_name: set_name.to_string(),
                set_id: None,
            });
        }
        info!(
            "Static layer {} ('{}'): materialised {} geometry shape(s) (terrain_set={:?})",
            layer_def.layer_id,
            layer_def.name,
            layer_def.geometry.len(),
            layer_def.terrain_set,
        );
    }

    let state = Arc::new(Mutex::new(CoordinatorState {
        sim: SimulationRunner::new(
            TickId(0),
            Box::new(physics),
            tick_dt,
            abilities,
            buffs,
            crate::lag_compensation::MAX_REWIND_TICKS,
        ),
        items,
        dungeons,
        encounters,
        spawn_rules,
        terrain: TerrainState::with_bindings(initial_terrain_bindings),
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
            info!(r#"  spacetime call tickforge register_worker '{{"__identity__":"0x{identity}"}}' -s local"#);

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

        // Gather intents up to the triggering canonical tick from the client cache.
        // If a later sim_tick callback arrives after a skipped insert, the runner will
        // process the next contiguous tick and ignore future-targeted intents for now.
        let pending_intents: Vec<(u64, game_protocol::intent::PlayerIntent)> = ctx
            .db
            .player_intent()
            .iter()
            .filter(|i| i.target_tick <= canonical_tick)
            .map(|row| {
                (
                    row.intent_id,
                    game_protocol::intent::PlayerIntent {
                        entity_id: EntityId(row.entity_id),
                        sequence_id: row.sequence_id,
                        target_tick: TickId(row.target_tick),
                        client_observed_tick: row.client_observed_tick,
                        action: convert_intent_action(row.action.clone()),
                    },
                )
            })
            .collect();
        let intents: Vec<_> = pending_intents
            .iter()
            .map(|(_, intent)| intent.clone())
            .collect();

        // ── Begin locked section ────────────────────────────────────────
        // Acquire the lock for pipeline mutation (run_tick + marshal).
        // The lock is dropped *before* the async reducer call so that commit
        // acknowledgement, entity callbacks, and other SDK events are not
        // blocked while the server processes the commit.
        let pkg = {
            let mut guard = match state_for_tick.lock() {
                Ok(g) => g,
                Err(poisoned) => {
                    error!("CoordinatorState lock poisoned — recovering");
                    poisoned.into_inner()
                }
            };

            // Drain any pending terrain edits BEFORE the tick step so a
            // chunk swap never lands mid-step. Idle workers pay a single
            // empty-vec branch (§4.8b Phase 5).
            {
                let CoordinatorState { sim, terrain, .. } = &mut *guard;
                terrain.drain_into(sim.physics_mut());
            }

            // Delegate tick orchestration to SimulationRunner.
            let result = match guard.sim.run_tick(canonical_tick, &intents) {
                Ok(r) => r,
                Err(_) => return, // already logged by TickDriver
            };

            // Marshal TickResult into SDK-free CommitPackage.
            let consumed_ids: Vec<u64> = pending_intents
                .iter()
                .filter(|(_, intent)| intent.target_tick <= result.tick_id)
                .map(|(intent_id, _)| *intent_id)
                .collect();

            commit_builder::build(result, consumed_ids)
        };
        // ── Lock released ───────────────────────────────────────────────

        // Send the commit via the retry-aware path.  On transient failure
        // the ack callback re-sends immediately; on exhaustion the process
        // exits for a clean supervisor reseed.
        send_commit(&ctx.reducers, pkg, Arc::clone(&state_for_tick));

        // ── Catch-up loop ───────────────────────────────────────────────
        // When backlogged, process additional ticks immediately instead of
        // waiting for the next SDK callback (which would keep the gap
        // constant forever).  Each iteration re-acquires the lock, runs
        // one tick, drops the lock, and sends the commit.
        loop {
            let pkg = {
                let mut guard = match state_for_tick.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if !guard.sim.can_catch_up(canonical_tick) {
                    break;
                }
                {
                    let CoordinatorState { sim, terrain, .. } = &mut *guard;
                    terrain.drain_into(sim.physics_mut());
                }
                let result = match guard.sim.run_tick(canonical_tick, &[]) {
                    Ok(r) => r,
                    Err(_) => break,
                };
                commit_builder::build(result, vec![])
            };
            send_commit(&ctx.reducers, pkg, Arc::clone(&state_for_tick));
        }
    });

    // entity.on_insert — thin adapter over EntitySync::sync_insert.
    // SDK cache reads happen before the lock; decision logic lives in EntitySync.
    conn.db.entity().on_insert(move |ctx, new_entity| {
        let eid = EntityId(new_entity.entity_id);
        let kind = convert_entity_kind(new_entity.kind);
        let state = convert_entity_state(new_entity.state);
        let tick = TickId(new_entity.spawned_at_tick);

        // Skip terminal entities early — on restart the subscription snapshot
        // includes Removed rows whose companion tables have already been cleaned
        // up.  Warn-and-default below would be noisy for these.
        if matches!(state, game_schema::EntityState::Removed | game_schema::EntityState::DespawnPending) {
            debug!("entity {} is {:?} in snapshot — skipping", new_entity.entity_id, state);
            return;
        }

        // Look up companion rows before acquiring the state lock — these reads
        // are from the SDK cache (no contention) and must not be done under the
        // lock to avoid holding it across I/O.
        let max_hp = ctx.db
            .entity_health()
            .entity_id()
            .find(&new_entity.entity_id)
            .map(|h| h.max_hp)
            .unwrap_or_else(|| {
                warn!("entity {} has no entity_health companion row at spawn — defaulting to 100 hp", new_entity.entity_id);
                100.0
            });

        let pos = ctx.db
            .entity_transform()
            .entity_id()
            .find(&new_entity.entity_id)
            .map(|t| game_protocol::types::Vec3f { x: t.pos_x, y: t.pos_y, z: t.pos_z })
            .unwrap_or_else(|| {
                warn!("entity {} has no entity_transform companion row at spawn — defaulting to origin", new_entity.entity_id);
                game_protocol::types::Vec3f { x: 0.0, y: 1.0, z: 0.0 }
            });

        let layer = ctx.db
            .entity_layer()
            .entity_id()
            .find(&new_entity.entity_id)
            .map(|r| r.layer)
            .unwrap_or(0);

        // Read per-instance buff state from the SDK cache.
        // Template data (modifiers, buff_kind, max_stacks) is reconstructed
        // from the BuffRegistry after acquiring the lock.
        struct BuffRow {
            buff_id: u32,
            source_entity: u64,
            entity_id: u64,
            stacks: u32,
            expires_at_tick: Option<u64>,
            ai_override_kind: Option<u8>,
            ai_override_target: Option<u64>,
            last_dot_tick: Option<u64>,
        }
        let buff_rows: Vec<BuffRow> = ctx.db
            .active_buff()
            .iter()
            .filter(|b| b.entity_id == new_entity.entity_id)
            .map(|b| BuffRow {
                buff_id: b.buff_id,
                source_entity: b.source_entity,
                entity_id: b.entity_id,
                stacks: b.stacks,
                expires_at_tick: b.expires_at_tick,
                ai_override_kind: b.mod_ai_override_kind,
                ai_override_target: b.mod_ai_override_target,
                last_dot_tick: b.last_dot_tick,
            })
            .collect();

        let npc_state: Option<(game_schema::NpcAiState, Option<EntityId>)> = ctx.db
            .npc_state()
            .entity_id()
            .find(&new_entity.entity_id)
            .map(|n| (convert_npc_ai_state(n.ai_state), n.target_entity.map(EntityId)));

        let npc_config_row = ctx
            .db
            .npc_config()
            .entity_id()
            .find(&new_entity.entity_id);

        let configured_encounter_key = npc_config_row
            .as_ref()
            .and_then(|c| c.encounter_name.clone())
            .filter(|key| !key.is_empty());

        // Resolve the authoritative physics body shape.
        // Character kinds read NpcConfig.body_shape (Players have no row →
        // PlayerCapsule default). Props read InteractableConfig.body_shape.
        let body_shape: Option<game_core::physics_backend::BodyShape> = match kind {
            game_schema::EntityKind::Player
            | game_schema::EntityKind::Npc
            | game_schema::EntityKind::Boss => npc_config_row
                .as_ref()
                .and_then(|c| c.body_shape)
                .and_then(game_core::physics_backend::BodyShape::from_u8),
            game_schema::EntityKind::Prop => ctx
                .db
                .interactable_config()
                .entity_id()
                .find(&new_entity.entity_id)
                .and_then(|c| game_core::physics_backend::BodyShape::from_u8(c.body_shape)),
            _ => None,
        };

        let npc_config: Option<crate::entity_sync::NpcSpawnConfig> = npc_config_row.map(|c| {
            let mut ability_ids: Vec<u32> = Vec::new();
            if let Some(id) = c.ability_id_1 {
                ability_ids.push(id);
            }
            if let Some(id) = c.ability_id_2 {
                ability_ids.push(id);
            }
            if let Some(id) = c.ability_id_3 {
                ability_ids.push(id);
            }
            if let Some(id) = c.ability_id_4 {
                ability_ids.push(id);
            }
            crate::entity_sync::NpcSpawnConfig {
                passive: c.passive,
                no_chase: c.no_chase,
                ability_ids,
                leash_radius: c.leash_radius,
                aggro_radius: c.aggro_radius,
            }
        });

        let mut guard = match state_for_entity.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                error!("CoordinatorState lock poisoned in entity.on_insert — recovering");
                poisoned.into_inner()
            }
        };

        // Reconstruct full ActiveBuff from per-instance DB fields + registry template.
        let buffs: Vec<game_core::combat::status::ActiveBuff> = buff_rows
            .into_iter()
            .filter_map(|b| {
                use game_core::combat::status::AiOverride;
                let ai_override = match b.ai_override_kind {
                    Some(0) => Some(AiOverride::ForceFlee),
                    Some(1) => Some(AiOverride::ForceIdle),
                    Some(2) => Some(AiOverride::ForceFocus {
                        target: EntityId(b.ai_override_target.unwrap_or(0)),
                    }),
                    _ => None,
                };
                if let Some(template) = guard.sim.buff_registry().get(b.buff_id) {
                    let mut modifiers = template.modifiers;
                    modifiers.ai_override = ai_override;
                    Some(game_core::combat::status::ActiveBuff {
                        buff_id: b.buff_id,
                        source: EntityId(b.source_entity),
                        target: EntityId(b.entity_id),
                        buff_kind: template.buff_kind,
                        stacks: b.stacks,
                        max_stacks: template.max_stacks,
                        expires_at: b.expires_at_tick.map(game_protocol::tick::TickId),
                        modifiers,
                        last_dot_tick: b.last_dot_tick.map(game_protocol::tick::TickId),
                    })
                } else {
                    warn!("Buff {} not found in registry during rehydration — skipping", b.buff_id);
                    None
                }
            })
            .collect();

        // Resolve Y before the mutable borrow in sync_insert.
        let spawn_pos = if let Some(shape) = body_shape {
            // Use capsule-specific spawn resolution when the body shape has capsule dimensions
            if let Some((capsule_half_height, capsule_radius)) = shape.capsule_dims() {
                guard.sim.resolve_spawn_position_with_capsule(pos, layer, capsule_half_height, capsule_radius)
            } else {
                // Fallback to standard resolution for non-capsule shapes
                if matches!(
                    kind,
                    game_schema::EntityKind::Player
                        | game_schema::EntityKind::Npc
                        | game_schema::EntityKind::Boss
                ) {
                    guard.sim.resolve_spawn_position(pos, layer)
                } else {
                    pos
                }
            }
        } else {
            // No body shape defined, use standard resolution for character types
            if matches!(
                kind,
                game_schema::EntityKind::Player
                    | game_schema::EntityKind::Npc
                    | game_schema::EntityKind::Boss
            ) {
                guard.sim.resolve_spawn_position(pos, layer)
            } else {
                pos
            }
        };

        EntitySync::sync_insert(
            &mut guard.sim,
            eid,
            kind,
            state,
            tick,
            max_hp,
            spawn_pos,
            layer,
            crate::entity_sync::RuntimeSnapshot {
                buffs,
                npc_state,
                npc_config,
                body_shape,
                ..Default::default()
            },
        );

        // Register encounter rules for Boss entities so the pipeline can
        // evaluate phase transitions each tick.
        if kind == game_schema::EntityKind::Boss {
            let configured_key = configured_encounter_key.as_deref();
            if let Some((resolved_key, rules, fell_back)) =
                resolve_boss_encounter_rules(&guard.encounters, configured_key)
            {
                if fell_back {
                    if let Some(missing_key) = configured_key {
                        warn!(
                            "Encounter key '{}' missing for boss entity {} — falling back to 'default'",
                            missing_key,
                            eid.0,
                        );
                    }
                }

                let enc_state =
                    game_core::encounter::EncounterState::new_dormant(eid, rules, tick);
                guard.sim.register_encounter(eid, enc_state);
                info!(
                    "Registered encounter rules '{}' for boss entity {}",
                    resolved_key,
                    eid.0,
                );
            } else if let Some(missing_key) = configured_key {
                warn!(
                    "Encounter key '{}' missing for boss entity {} and 'default' encounter is missing",
                    missing_key,
                    eid.0,
                );
            } else {
                warn!(
                    "No encounter rules registered for boss entity {} because 'default' encounter is missing",
                    eid.0,
                );
            }
        }
    });

    // entity.on_update — thin adapter over EntitySync::sync_update.
    // On Respawn result, reads companion rows and delegates to sync_insert.
    let state_for_entity_update = Arc::clone(&state);
    conn.db
        .entity()
        .on_update(move |ctx, old_entity, new_entity| {
            let eid = EntityId(new_entity.entity_id);
            let old_state = convert_entity_state(old_entity.state);
            let new_state = convert_entity_state(new_entity.state);

            // First pass: sync_update under the lock to get the result.
            let result = {
                let mut guard = match state_for_entity_update.lock() {
                    Ok(g) => g,
                    Err(poisoned) => {
                        error!("CoordinatorState lock poisoned in entity.on_update — recovering");
                        poisoned.into_inner()
                    }
                };
                EntitySync::sync_update(&mut guard.sim, eid, old_state, new_state)
            };

            if result == crate::entity_sync::SyncUpdateResult::Respawn {
                // Read companion rows from SDK cache *before* reacquiring the
                // lock — same pattern as on_insert to keep the critical section
                // minimal.
                let kind = convert_entity_kind(new_entity.kind);
                let tick = TickId(new_entity.spawned_at_tick);

                let max_hp = ctx
                    .db
                    .entity_health()
                    .entity_id()
                    .find(&new_entity.entity_id)
                    .map(|h| h.max_hp)
                    .unwrap_or(100.0);

                let pos = ctx
                    .db
                    .entity_transform()
                    .entity_id()
                    .find(&new_entity.entity_id)
                    .map(|t| game_protocol::types::Vec3f {
                        x: t.pos_x,
                        y: t.pos_y,
                        z: t.pos_z,
                    })
                    .unwrap_or(game_protocol::types::Vec3f {
                        x: 0.0,
                        y: 1.0,
                        z: 0.0,
                    });

                // Read the entity's layer from the DB — respawn_player sets this
                // to the death layer (which may be a dungeon instance, not 0).
                let layer = ctx
                    .db
                    .entity_layer()
                    .entity_id()
                    .find(&new_entity.entity_id)
                    .map(|r| r.layer)
                    .unwrap_or(0);

                let snapshot = crate::entity_sync::RuntimeSnapshot::default();

                // Reacquire lock only for the sync_insert mutation.
                let mut guard = match state_for_entity_update.lock() {
                    Ok(g) => g,
                    Err(poisoned) => {
                        error!("CoordinatorState lock poisoned in entity.on_update (respawn) — recovering");
                        poisoned.into_inner()
                    }
                };
                EntitySync::sync_insert(
                    &mut guard.sim,
                    eid,
                    kind,
                    new_state,
                    tick,
                    max_hp,
                    pos,
                    layer,
                    snapshot,
                );
                // Re-apply equipment modifiers — force_remove_entities cleared
                // them, but the DB rows still exist.
                recompute_equipment(&ctx.db, &mut guard, eid);
                info!(
                    "Entity {} respawned via sync_insert after on_update Respawn signal",
                    eid.0
                );
            }
        });

    // entity.on_delete — thin adapter over EntitySync::sync_delete.
    let state_for_entity_delete = Arc::clone(&state);
    conn.db.entity().on_delete(move |_ctx, deleted_entity| {
        let eid = EntityId(deleted_entity.entity_id);
        let mut guard = match state_for_entity_delete.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                error!("CoordinatorState lock poisoned in entity.on_delete — recovering");
                poisoned.into_inner()
            }
        };
        EntitySync::sync_delete(&mut guard.sim, eid);
    });

    // ── Instance lifecycle — spawn/despawn environment colliders ─────
    // When the server creates an instance, the worker spawns parentless
    // environment colliders (Pass 1) from the dungeon template. Prop
    // entities for interactables are already created server-side (Pass 2).
    let state_for_instance_insert = Arc::clone(&state);
    conn.db.instance().on_insert(move |ctx, inst| {
        // Skip initial subscription snapshot — existing instances are already
        // handled by on_applied. Only react to live inserts here.
        if matches!(ctx.event, spacetimedb_sdk::Event::SubscribeApplied) {
            return;
        }
        if inst.state != crate::module_bindings::InstanceState::Active
            && inst.state != crate::module_bindings::InstanceState::Pending
        {
            return;
        }
        let mut guard = match state_for_instance_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        materialize_active_instance(&mut guard, &ctx.db, inst, "live insert");
    });

    // On instance update → Expired: remove environment colliders for that layer.
    let state_for_instance_update = Arc::clone(&state);
    conn.db
        .instance()
        .on_update(move |_ctx, old_inst, new_inst| {
            // Only act when state transitions to Expired.
            if old_inst.state == new_inst.state {
                return;
            }
            if new_inst.state != crate::module_bindings::InstanceState::Expired {
                return;
            }
            let mut guard = match state_for_instance_update.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard
                .sim
                .physics_mut()
                .remove_environment_colliders_by_layer(new_inst.layer);
            guard.sim.physics_mut().remove_layer_policy(new_inst.layer);
            let cleared = guard.sim.clear_director_for_layer(new_inst.layer);
            // Drop any terrain binding on this layer so future edits to
            // the same set don't try to apply to a freed layer.
            guard.terrain.unbind_layer(new_inst.layer);
            info!(
                "Instance {} expired: removed environment colliders from layer {} \
                 (cleared {} director event(s))",
                new_inst.instance_id, new_inst.layer, cleared
            );
        });

    // ── Entity layer bridge ──────────────────────────────────────────
    // Mirror entity_layer rows (public projection of entity_region.layer)
    // into the sim's dense layer cache so physics and combat use the
    // correct layer for every entity (players, NPCs, props, bosses).

    let state_for_layer_insert = Arc::clone(&state);
    conn.db.entity_layer().on_insert(move |ctx, row| {
        if matches!(ctx.event, spacetimedb_sdk::Event::SubscribeApplied) {
            return; // Handled in on_applied bulk seed.
        }
        let eid = EntityId(row.entity_id);
        let mut guard = match state_for_layer_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.sim.set_entity_layer(eid, row.layer);
        //info!("entity_layer.on_insert: entity {} → layer {}", row.entity_id, row.layer);
    });

    let state_for_layer_update = Arc::clone(&state);
    conn.db.entity_layer().on_update(move |ctx, old, row| {
        let eid = EntityId(row.entity_id);
        let mut guard = match state_for_layer_update.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.sim.set_entity_layer(eid, row.layer);
        // Reconcile physics/SimState to the new layer's authoritative
        // spawn point. The reducer that triggered this layer change
        // (join_instance / debug_join_instance / leave_instance) wrote a
        // fresh entity_transform atomically; re-read it now, then
        // raycast-snap Y via `reconcile_entity_to_position` so the body
        // lands on the terrain that belongs to `new_layer`. Skip if the
        // layer value did not actually change (same-value updates fire
        // too under some subscription paths) or if the entity isn't in
        // the sim yet (insert handler will seed it instead).
        if old.layer == row.layer {
            return;
        }
        if !guard.sim.entity_exists(eid) {
            return;
        }
        if let Some(t) = ctx.db.entity_transform().entity_id().find(&row.entity_id) {
            let advisory = game_protocol::types::Vec3f {
                x: t.pos_x,
                y: t.pos_y,
                z: t.pos_z,
            };
            guard
                .sim
                .reconcile_entity_to_position(eid, advisory, row.layer);
        }
        info!(
            "entity_layer.on_update: entity {} → layer {}",
            row.entity_id, row.layer
        );
    });

    let state_for_layer_delete = Arc::clone(&state);
    conn.db.entity_layer().on_delete(move |_ctx, row| {
        let eid = EntityId(row.entity_id);
        let mut guard = match state_for_layer_delete.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.sim.set_entity_layer(eid, 0);
        //info!("entity_layer.on_delete: entity {} → layer 0", row.entity_id);
    });

    // ── Entity team bridge ──────────────────────────────────────────
    // Mirror entity_team rows into the sim's dense team cache so
    // apply_hit_damage can filter friendly/hostile targets in O(1).

    let state_for_team_insert = Arc::clone(&state);
    conn.db.entity_team().on_insert(move |ctx, row| {
        if matches!(ctx.event, spacetimedb_sdk::Event::SubscribeApplied) {
            return; // Handled in on_applied bulk seed below.
        }
        let eid = EntityId(row.entity_id);
        let mut guard = match state_for_team_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.sim.set_entity_team(eid, row.team_id);
        info!(
            "entity_team.on_insert: entity {} → team {}",
            row.entity_id, row.team_id
        );
    });

    let state_for_team_update = Arc::clone(&state);
    conn.db.entity_team().on_update(move |_ctx, _old, row| {
        let eid = EntityId(row.entity_id);
        let mut guard = match state_for_team_update.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.sim.set_entity_team(eid, row.team_id);
        info!(
            "entity_team.on_update: entity {} → team {}",
            row.entity_id, row.team_id
        );
    });

    let state_for_team_delete = Arc::clone(&state);
    conn.db.entity_team().on_delete(move |_ctx, row| {
        let eid = EntityId(row.entity_id);
        let mut guard = match state_for_team_delete.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.sim.set_entity_team(eid, 0);
        info!("entity_team.on_delete: entity {} → team 0", row.entity_id);
    });

    // ── Equipment bridge ────────────────────────────────────────────
    // Observe player_equipment changes to trigger stat recalculation.
    // These callbacks fire when a client calls equip_item / unequip_item
    // reducers. The coordinator queues a stat recalc on SimulationRunner,
    // which applies it before the next tick's Phase 1.

    // ── Interactable config bridge ──────────────────────────────────
    // Populate the sim-side interactable map from DB subscription so
    // handle_interact can branch by kind and toggle gate colliders.

    let state_for_interact_insert = Arc::clone(&state);
    conn.db.interactable_config().on_insert(move |_ctx, row| {
        let mut guard = match state_for_interact_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let info = convert_interactable_info(&row);
        guard.sim.set_interactable(EntityId(row.entity_id), info);
    });

    let state_for_interact_update = Arc::clone(&state);
    conn.db
        .interactable_config()
        .on_update(move |_ctx, _old, row| {
            let mut guard = match state_for_interact_update.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let info = convert_interactable_info(&row);
            guard.sim.set_interactable(EntityId(row.entity_id), info);
        });

    let state_for_interact_delete = Arc::clone(&state);
    conn.db.interactable_config().on_delete(move |_ctx, row| {
        let mut guard = match state_for_interact_delete.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.sim.remove_interactable(EntityId(row.entity_id));
    });

    // ── Voxel terrain (§4.8b Phase 5) ───────────────────────────────
    //
    // Three callbacks per table. The actual physics mutation is
    // **deferred** — every callback only enqueues a `TerrainEdit`, which
    // is drained from `pending_edits` at the start of the next tick
    // before `run_tick` (see `sim_tick.on_insert` below). This keeps BVH
    // rebuilds off the hot path: idle workers pay zero cost, and an
    // edit lands at the next tick boundary instead of mid-step.

    let state_for_terrain_set_insert = Arc::clone(&state);
    conn.db.terrain_set().on_insert(move |ctx, row| {
        let mut guard = match state_for_terrain_set_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let newly_bound = guard.terrain.resolve_set(&row.name, row.terrain_set_id);
        if newly_bound.is_empty() {
            return;
        }
        // For every layer that just became hydratable, queue every cached
        // terrain_chunk row matching this set. The drain at the next tick
        // applies them to physics. Note we iterate the SDK cache here, not
        // the queue — chunks may already be present (during initial
        // SubscribeApplied) or arrive later via terrain_chunk.on_insert.
        let chunks: Vec<_> = ctx
            .db
            .terrain_chunk()
            .iter()
            .filter(|c| c.terrain_set_id == row.terrain_set_id)
            .collect();
        let chunk_count = chunks.len();
        for c in chunks {
            guard.terrain.enqueue(TerrainEdit::Insert {
                terrain_set_id: c.terrain_set_id,
                row_id: c.row_id,
                vertices: c.vertices,
                indices: c.indices,
            });
        }
        info!(
            "terrain_set.on_insert: '{}' (id={}) → {} layer(s) bound, {} chunk(s) queued",
            row.name,
            row.terrain_set_id,
            newly_bound.len(),
            chunk_count,
        );
    });

    let state_for_terrain_chunk_insert = Arc::clone(&state);
    conn.db.terrain_chunk().on_insert(move |_ctx, row| {
        let mut guard = match state_for_terrain_chunk_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Always enqueue: the drain skips chunks whose set has no bound
        // layer yet, so an early arrival before terrain_set.on_insert
        // would otherwise be lost. Once terrain_set resolves, that
        // callback queues a fresh batch from the cache (which includes
        // this row), keeping behaviour correct.
        if guard.terrain.layers_for_set(row.terrain_set_id).is_empty() {
            return;
        }
        guard.terrain.enqueue(TerrainEdit::Insert {
            terrain_set_id: row.terrain_set_id,
            row_id: row.row_id,
            vertices: row.vertices.clone(),
            indices: row.indices.clone(),
        });
    });

    let state_for_terrain_chunk_update = Arc::clone(&state);
    conn.db.terrain_chunk().on_update(move |_ctx, _old, row| {
        let mut guard = match state_for_terrain_chunk_update.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.terrain.layers_for_set(row.terrain_set_id).is_empty() {
            return;
        }
        guard.terrain.enqueue(TerrainEdit::Update {
            terrain_set_id: row.terrain_set_id,
            row_id: row.row_id,
            vertices: row.vertices.clone(),
            indices: row.indices.clone(),
        });
    });

    let state_for_terrain_chunk_delete = Arc::clone(&state);
    conn.db.terrain_chunk().on_delete(move |_ctx, row| {
        let mut guard = match state_for_terrain_chunk_delete.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .terrain
            .enqueue(TerrainEdit::Delete { row_id: row.row_id });
    });

    // ── Encounter Add Membership projection ─────────────────────────
    // Mirror encounter_add rows into the pipeline's add_to_boss/entity_tags
    // maps so encounter rules like `OnEntityDied { tag }` fire in production.
    //
    // Replay ordering note: this fires independently of `entity.on_insert`.
    // `register_encounter_add_with_tags` only writes the two HashMaps and
    // does not consult any entity slot table, so out-of-order arrival is
    // safe.

    let state_for_add_insert = Arc::clone(&state);
    conn.db.encounter_add().on_insert(move |_ctx, row| {
        let mut guard = match state_for_add_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.sim.register_encounter_add_with_tags(
            EntityId(row.add_entity),
            EntityId(row.boss_entity),
            &row.tags,
        );
    });

    // Symmetric on_delete: clear mirrored worker state when the reducer
    // cascade-deletes membership rows (especially via `by_boss()` on boss
    // death, where the add entity may briefly outlive its membership row).
    let state_for_add_delete = Arc::clone(&state);
    conn.db.encounter_add().on_delete(move |_ctx, row| {
        let mut guard = match state_for_add_delete.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.sim.unregister_encounter_add(EntityId(row.add_entity));
    });

    // ── World Phase projection ──────────────────────────────────────
    // Project world_phase rows into the pipeline's world_phases map so
    // the director and encounter executor can react to zone progression.

    let state_for_wp_insert = Arc::clone(&state);
    conn.db.world_phase().on_insert(move |_ctx, row| {
        let mut guard = match state_for_wp_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        info!(
            "world_phase.on_insert: zone={} phase='{}'",
            row.zone_id, row.phase_name
        );
        guard
            .sim
            .set_world_phase(row.zone_id, row.phase_name.clone());
    });

    let state_for_wp_update = Arc::clone(&state);
    conn.db.world_phase().on_update(move |_ctx, _old, row| {
        let mut guard = match state_for_wp_update.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        info!(
            "world_phase.on_update: zone={} phase='{}'",
            row.zone_id, row.phase_name
        );
        guard
            .sim
            .set_world_phase(row.zone_id, row.phase_name.clone());
    });

    let state_for_wp_delete = Arc::clone(&state);
    conn.db.world_phase().on_delete(move |_ctx, row| {
        let mut guard = match state_for_wp_delete.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        debug!("world_phase.on_delete: zone={}", row.zone_id);
        guard.sim.remove_world_phase(row.zone_id);
    });

    // ── NPC Goal projection ─────────────────────────────────────────
    // Project npc_goal rows so Phase 7 AI can read goal directives.

    let state_for_goal_insert = Arc::clone(&state);
    conn.db.npc_goal().on_insert(move |_ctx, row| {
        let mut guard = match state_for_goal_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        debug!(
            "npc_goal.on_insert: entity={} kind='{}' priority={}",
            row.entity_id, row.goal_kind, row.priority
        );
        guard
            .sim
            .set_npc_goal(EntityId(row.entity_id), row.goal_kind.clone(), row.priority);
    });

    let state_for_goal_update = Arc::clone(&state);
    conn.db.npc_goal().on_update(move |_ctx, _old, row| {
        let mut guard = match state_for_goal_update.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        debug!(
            "npc_goal.on_update: entity={} kind='{}' priority={}",
            row.entity_id, row.goal_kind, row.priority
        );
        guard
            .sim
            .set_npc_goal(EntityId(row.entity_id), row.goal_kind.clone(), row.priority);
    });

    let state_for_goal_delete = Arc::clone(&state);
    conn.db.npc_goal().on_delete(move |_ctx, row| {
        let mut guard = match state_for_goal_delete.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        debug!("npc_goal.on_delete: entity={}", row.entity_id);
        guard.sim.remove_npc_goal(EntityId(row.entity_id));
    });

    let state_for_equip_insert = Arc::clone(&state);
    conn.db.player_equipment().on_insert(move |ctx, row| {
        if matches!(ctx.event, spacetimedb_sdk::Event::SubscribeApplied) {
            return; // Initial snapshot — stats will be seeded from full state
        }
        let eid = EntityId(row.owner_entity);
        info!(
            "player_equipment.on_insert: entity={} item={} slot={:?}",
            row.owner_entity, row.item_id, row.slot
        );
        let mut guard = match state_for_equip_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        recompute_equipment(&ctx.db, &mut guard, eid);
    });

    let state_for_equip_update = Arc::clone(&state);
    conn.db
        .player_equipment()
        .on_update(move |ctx, _old, new_row| {
            let eid = EntityId(new_row.owner_entity);
            info!(
                "player_equipment.on_update: entity={} item={} slot={:?}",
                new_row.owner_entity, new_row.item_id, new_row.slot
            );
            let mut guard = match state_for_equip_update.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            recompute_equipment(&ctx.db, &mut guard, eid);
        });

    let state_for_equip_delete = Arc::clone(&state);
    conn.db.player_equipment().on_delete(move |ctx, old_row| {
        let eid = EntityId(old_row.owner_entity);
        info!(
            "player_equipment.on_delete: entity={} item={} slot={:?}",
            old_row.owner_entity, old_row.item_id, old_row.slot
        );
        let mut guard = match state_for_equip_delete.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        recompute_equipment(&ctx.db, &mut guard, eid);
    });

    // ── Inventory bridge ───────────────────────────────────────────
    // Mirror item counts into SimState so interactable `required_item`
    // gates can be enforced inside Phase 2 without consulting the DB.

    let state_for_inventory_insert = Arc::clone(&state);
    conn.db.player_inventory().on_insert(move |ctx, row| {
        if matches!(ctx.event, spacetimedb_sdk::Event::SubscribeApplied) {
            return;
        }
        let mut guard = match state_for_inventory_insert.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .sim
            .add_inventory_item_count(EntityId(row.owner_entity), row.item_id, row.quantity);
    });

    let state_for_inventory_update = Arc::clone(&state);
    conn.db
        .player_inventory()
        .on_update(move |_ctx, old_row, new_row| {
            let mut guard = match state_for_inventory_update.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.sim.remove_inventory_item_count(
                EntityId(old_row.owner_entity),
                old_row.item_id,
                old_row.quantity,
            );
            guard.sim.add_inventory_item_count(
                EntityId(new_row.owner_entity),
                new_row.item_id,
                new_row.quantity,
            );
        });

    let state_for_inventory_delete = Arc::clone(&state);
    conn.db.player_inventory().on_delete(move |_ctx, row| {
        let mut guard = match state_for_inventory_delete.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.sim.remove_inventory_item_count(
            EntityId(row.owner_entity),
            row.item_id,
            row.quantity,
        );
    });

    // Block on the connection thread — the callbacks above drive the simulation.
    conn.run_threaded()
        .join()
        .expect("Connection thread panicked");
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
            let global_max_rewind_ticks = ctx.db.module_config().iter().next().map(|c| c.global_max_rewind_ticks).unwrap_or(crate::lag_compensation::MAX_REWIND_TICKS);
            {
                let mut guard = match state.lock() {
                    Ok(g) => g,
                    Err(poisoned) => {
                        error!("CoordinatorState lock poisoned in on_applied — recovering");
                        poisoned.into_inner()
                    }
                };
                guard.sim.seed(max_tick, global_max_rewind_ticks);

                // Recompute equipment modifiers for every entity that has
                // equipment rows.  The per-row on_insert callback skips
                // SubscribeApplied events, so this is the only path that
                // restores equipment state after a worker restart.
                let equipped_entities: std::collections::BTreeSet<_> = ctx
                    .db
                    .player_equipment()
                    .iter()
                    .map(|row| EntityId(row.owner_entity))
                    .collect();
                for eid in equipped_entities {
                    recompute_equipment(&ctx.db, &mut guard, eid);
                }

                guard.sim.clear_inventory_items();
                for row in ctx.db.player_inventory().iter() {
                    guard.sim.add_inventory_item_count(
                        EntityId(row.owner_entity),
                        row.item_id,
                        row.quantity,
                    );
                }

                // Seed entity layers from entity_layer rows (public
                // projection of entity_region.layer). Covers all entities
                // — players, dungeon NPCs, props, bosses.
                for row in ctx.db.entity_layer().iter() {
                    guard.sim.set_entity_layer(EntityId(row.entity_id), row.layer);
                }

                // Seed entity teams from entity_team rows.
                for row in ctx.db.entity_team().iter() {
                    guard.sim.set_entity_team(EntityId(row.entity_id), row.team_id);
                }

                // ── Register open-world director events from spawn rules ──
                //
                // Each (rx, rz, &SpawnRule) tuple yielded by the registry maps
                // to one `DynamicEvent` registered against layer 0.  Dungeon
                // rules are ignored here — they're registered per-instance by
                // `instance.on_insert`.
                let open_world: Vec<(i32, i32, game_core::director::DynamicEvent)> = guard
                    .spawn_rules
                    .for_open_world()
                    .into_iter()
                    .filter_map(|(rx, rz, rule)| {
                        game_core::spawn_rules::rule_to_dynamic_event(rule, rx, rz, /*layer=*/ 0)
                            .map(|ev| (rx, rz, ev))
                    })
                    .collect();
                let registered = open_world.len();
                for (_, _, ev) in open_world {
                    let _ = guard.sim.register_director_event(ev);
                }
                info!(
                    "Registered {registered} open-world director event(s) from spawn_rules.ron",
                );

                // Seed Active/Pending instance geometry after a worker restart.
                // `instance.on_insert` skips SubscribeApplied rows, so without
                // this path the DB can contain live instance entities/layers
                // while Rapier has no dungeon floor or walls for that layer.
                let active_instances: Vec<_> = ctx
                    .db
                    .instance()
                    .iter()
                    .filter(|inst| {
                        inst.state == crate::module_bindings::InstanceState::Active
                            || inst.state == crate::module_bindings::InstanceState::Pending
                    })
                    .collect();
                let mut materialized_instances = 0usize;
                for inst in active_instances {
                    if materialize_active_instance(&mut guard, &ctx.db, &inst, "subscription seed")
                    {
                        materialized_instances += 1;
                    }
                }
                if materialized_instances > 0 {
                    info!(
                        "Materialised {materialized_instances} active instance layer(s) from subscription snapshot",
                    );
                }
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
            "SELECT * FROM module_config",
            "SELECT * FROM sim_tick",
            "SELECT * FROM player_intent",
            "SELECT * FROM entity",
            "SELECT * FROM entity_health",
            "SELECT * FROM entity_transform",
            "SELECT * FROM active_buff",
            "SELECT * FROM npc_state",
            "SELECT * FROM player_equipment",
            "SELECT * FROM player_inventory",
            "SELECT * FROM instance",
            "SELECT * FROM interactable_config",
            "SELECT * FROM world_phase",
            "SELECT * FROM npc_goal",
            "SELECT * FROM npc_config",
            "SELECT * FROM entity_team",
            "SELECT * FROM entity_layer",
            "SELECT * FROM encounter_add",
            // Terrain: full snapshot once at SubscribeApplied, then per-row
            // deltas via on_insert/on_update/on_delete. A single chunk edit
            // costs one row delta + one TriMesh rebuild — not a reload.
            // Future: scope per-set (WHERE terrain_set_id IN (...)) when
            // one DB hosts many disjoint maps.
            "SELECT * FROM terrain_set",
            "SELECT * FROM terrain_chunk",
        ]);
}

fn materialize_active_instance(
    guard: &mut CoordinatorState,
    db: &RemoteTables,
    inst: &Instance,
    source: &str,
) -> bool {
    let Some(template) = guard.dungeons.get(&inst.template_id).cloned() else {
        warn!(
            "Instance {} references unknown template '{}' during {} — no geometry spawned",
            inst.instance_id, inst.template_id, source
        );
        return false;
    };

    let layer = inst.layer;
    materialize_layer(
        guard.sim.physics_mut(),
        layer,
        &template.geometry,
        template.terrain_set.as_deref(),
        template.collision_policy,
    );
    info!(
        "Instance {} (template={}): materialised {} environment collider(s) on layer {} (terrain_set={:?}, source={})",
        inst.instance_id,
        inst.template_id,
        template.geometry.len(),
        layer,
        template.terrain_set,
        source,
    );

    // Register terrain binding for this instance layer + enqueue every
    // currently cached chunk for the matching set. Drain happens at the
    // next tick boundary (§4.8b Phase 5).
    //
    // `register_binding` picks up an existing set_id if any sibling layer
    // already resolved this name; the explicit resolve below handles the
    // first-binder case and is a no-op for already-resolved siblings.
    if let Some(set_name) = template.terrain_set.as_deref() {
        guard.terrain.register_binding(layer, set_name.to_string());
        if let Some(set_row) = db.terrain_set().name().find(&set_name.to_string()) {
            let newly = guard.terrain.resolve_set(set_name, set_row.terrain_set_id);
            let chunks: Vec<_> = db
                .terrain_chunk()
                .iter()
                .filter(|c| c.terrain_set_id == set_row.terrain_set_id)
                .collect();
            let chunk_count = chunks.len();
            for c in chunks {
                guard.terrain.enqueue(TerrainEdit::Insert {
                    terrain_set_id: c.terrain_set_id,
                    row_id: c.row_id,
                    vertices: c.vertices,
                    indices: c.indices,
                });
            }
            info!(
                "Instance {}: terrain_set '{}' resolved to id {} ({} new layer(s) bound, {} chunk(s) queued)",
                inst.instance_id,
                set_name,
                set_row.terrain_set_id,
                newly.len(),
                chunk_count,
            );
        } else {
            info!(
                "Instance {}: terrain_set '{}' not yet observed — chunks will arrive via live terrain_set/terrain_chunk callbacks",
                inst.instance_id, set_name,
            );
        }
    }

    // Re-materialization is idempotent for geometry; make director
    // registrations idempotent too so a subscription reseed cannot double-fire
    // dungeon-scoped spawn rules on the same layer.
    let cleared = guard.sim.clear_director_for_layer(layer);
    if cleared > 0 {
        info!(
            "Instance {} (template={}): cleared {} stale director event(s) on layer {} before registering rules",
            inst.instance_id, inst.template_id, cleared, layer,
        );
    }

    let dungeon_events: Vec<game_core::director::DynamicEvent> = guard
        .spawn_rules
        .for_dungeon(&inst.template_id)
        .filter_map(|rule| {
            // Dungeon-scoped rules use the instance-normalised zone key
            // (layer, 0, 0) — see `rule_to_dynamic_event`.
            game_core::spawn_rules::rule_to_dynamic_event(rule, 0, 0, layer)
        })
        .collect();
    let dungeon_event_count = dungeon_events.len();
    for ev in dungeon_events {
        let _ = guard.sim.register_director_event(ev);
    }
    if dungeon_event_count > 0 {
        info!(
            "Instance {} (template={}): registered {} director event(s) on layer {}",
            inst.instance_id, inst.template_id, dungeon_event_count, layer
        );
    }

    true
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

/// Load abilities from `data/abilities.ron`.
/// Falls back to the hardcoded registry if the file is missing or malformed.
fn load_abilities() -> AbilityRegistry {
    const PATH: &str = "data/abilities.ron";
    let result = std::fs::read_to_string(PATH)
        .map_err(|e| format!("read '{PATH}': {e}"))
        .and_then(|src| {
            ron::from_str::<AbilityFile>(&src).map_err(|e| format!("parse '{PATH}': {e}"))
        });
    match result {
        Ok(file) => {
            let count = file.abilities.len();
            let mut reg = AbilityRegistry::new();
            for a in file.abilities {
                reg.register(a);
            }
            for t in file.timelines {
                reg.register_timeline(t);
            }
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
        on_hit_buffs: vec![],
        knockback_force: 0.0,
        allow_reentry: false,
        charge_tiers: None,
        charge_roots_while_charging: true,
        damage_interval_ticks: 0,
        pierce: false,
        knockdown_ticks: 0,
        stun_ticks: 0,
        pull_force: 0.0,
        launch_lift: 0.0,
        launch_recovery_ticks: 0,
        usable_while_cc: false,
        require_grounded: true,

        fear_ticks: 0,
        silence_ticks: 0,
        sleep_ticks: 0,
        max_range: Option::None,
        projectile_speed: Option::None,
        targeting_mode: game_core::combat::skill::TargetingMode::DirectionTarget,
        cast_facing_policy: game_core::combat::skill::CastFacingPolicy::FaceAimDirection,
        lock_on_timeout_ticks: None,
        heal_amount: 0.0,
        max_rewind_ticks: None,
        target_filter: TargetFilter::Hostile,
    });
    reg.register_timeline(AbilityTimeline {
        ability_id: 1,
        actions: vec![
            ScheduledAbilityAction {
                tick_offset: 0,
                action: AbilityAction::SpawnHitbox {
                    shape: SkillShape::CapsuleSweep,
                    offset: game_schema::Vec3f {
                        x: 0.0,
                        y: 0.0,
                        z: 0.0,
                    },
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

// ── Item registry ───────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct ItemFile {
    items: Vec<game_core::stats::ItemData>,
}

/// Load item definitions from `data/items.ron`.
/// Falls back to the hardcoded registry if the file is missing or malformed.
fn load_items() -> game_core::stats::ItemRegistry {
    const PATH: &str = "data/items.ron";
    let result = std::fs::read_to_string(PATH)
        .map_err(|e| format!("read '{PATH}': {e}"))
        .and_then(|src| {
            ron::from_str::<ItemFile>(&src).map_err(|e| format!("parse '{PATH}': {e}"))
        });
    match result {
        Ok(file) => {
            let count = file.items.len();
            let mut reg = game_core::stats::ItemRegistry::new();
            for item in file.items {
                reg.register(item);
            }
            info!("Loaded {count} item(s) from {PATH}");
            reg
        }
        Err(e) => {
            warn!("Could not load {PATH} ({e}) — using hardcoded fallback");
            build_item_registry()
        }
    }
}

fn build_item_registry() -> game_core::stats::ItemRegistry {
    use game_core::stats::{EquipmentModifiers, ItemData, ItemRegistry};
    let mut reg = ItemRegistry::new();
    reg.register(ItemData {
        item_id: 1,
        name: "Rusty Sword".into(),
        modifiers: EquipmentModifiers {
            attack_power: 0.2,
            ..EquipmentModifiers::default()
        },
    });
    reg.register(ItemData {
        item_id: 2,
        name: "Leather Vest".into(),
        modifiers: EquipmentModifiers {
            max_hp: 20.0,
            damage_in: -0.05,
            ..EquipmentModifiers::default()
        },
    });
    reg
}

// ── Buff registry ───────────────────────────────────────────────

/// Load buff definitions from `data/buffs.ron`.
/// Falls back to an empty registry if the file is missing or malformed.
fn load_buffs() -> BuffRegistry {
    use game_core::combat::status::BuffFile;
    const PATH: &str = "data/buffs.ron";
    let result = std::fs::read_to_string(PATH)
        .map_err(|e| format!("read '{PATH}': {e}"))
        .and_then(|src| {
            ron::from_str::<BuffFile>(&src).map_err(|e| format!("parse '{PATH}': {e}"))
        });
    match result {
        Ok(file) => {
            let count = file.buffs.len();
            let mut reg = BuffRegistry::new();
            for b in file.buffs {
                reg.register(b);
            }
            info!("Loaded {count} buff(s) from {PATH}");
            reg
        }
        Err(e) => {
            warn!("Could not load {PATH} ({e}) — using empty buff registry");
            BuffRegistry::new()
        }
    }
}

// ── Dungeon registry ────────────────────────────────────────────

/// Load dungeon templates from `data/dungeons.ron`.
/// Falls back to an empty registry if the file is missing or malformed.
fn load_dungeons() -> game_core::dungeon::DungeonRegistry {
    use game_core::dungeon::{DungeonFile, DungeonRegistry};
    const PATH: &str = "data/dungeons.ron";
    let result = std::fs::read_to_string(PATH)
        .map_err(|e| format!("read '{PATH}': {e}"))
        .and_then(|src| {
            ron::from_str::<DungeonFile>(&src).map_err(|e| format!("parse '{PATH}': {e}"))
        });
    match result {
        Ok(file) => {
            let count = file.templates.len();
            let mut reg = DungeonRegistry::new();
            for t in file.templates {
                reg.register(t);
            }
            info!("Loaded {count} dungeon template(s) from {PATH}");
            reg
        }
        Err(e) => {
            warn!("Could not load {PATH} ({e}) — using empty dungeon registry");
            DungeonRegistry::new()
        }
    }
}

// ── Static-layer registry ───────────────────────────────────────

/// Load named static layers (open world, hubs, persistent test rooms)
/// from `data/layers.ron`. Falls back to an empty list if the file is
/// missing — the worker will still run, but no static-layer floors
/// will exist and any layer-0 entity will fall through forever (loud
/// failure mode by design; see `PhysicsWorld::new`).
fn load_world_layers() -> Vec<game_schema::dungeon::WorldLayerDef> {
    use game_schema::dungeon::WorldLayersFile;
    const PATH: &str = "data/layers.ron";
    let result = std::fs::read_to_string(PATH)
        .map_err(|e| format!("read '{PATH}': {e}"))
        .and_then(|src| {
            ron::from_str::<WorldLayersFile>(&src).map_err(|e| format!("parse '{PATH}': {e}"))
        });
    match result {
        Ok(file) => {
            let mut layers = file.layers;
            // Reserved-range guard: dungeon instances allocate from 100+;
            // a static layer with `layer_id >= 100` would collide with a
            // future instance allocation. Drop it loudly rather than mix.
            let before = layers.len();
            layers.retain(|l| {
                if l.layer_id >= 100 {
                    error!(
                        "data/layers.ron: dropping layer '{}' (layer_id={} >= 100 \
                         conflicts with dynamic instance allocator range)",
                        l.name, l.layer_id
                    );
                    false
                } else {
                    true
                }
            });
            info!(
                "Loaded {} world layer(s) from {PATH} ({} dropped as reserved-range conflicts)",
                layers.len(),
                before - layers.len(),
            );
            layers
        }
        Err(e) => {
            warn!(
                "Could not load {PATH} ({e}) — no static layers will be materialised; \
                 expect entities on un-authored layers to fall forever"
            );
            Vec::new()
        }
    }
}

// ── Voxel terrain state (§4.8b Phase 5) ─────────────────────────────
//
// The worker treats baked `terrain_chunk` rows as immutable-during-tick
// environment colliders. Live edits from the offline editor (rare) are
// captured by SDK `on_insert` / `on_update` / `on_delete` callbacks and
// queued in `pending_edits`; the queue is drained at the start of each
// tick before `run_tick`, so a chunk swap never lands mid-step.
//
// Steady-state cost is zero: the queue is empty in production and the
// drain branch is predicted away. Edit cost is bounded — one BVH
// rebuild per edited chunk, batched if multiple edits arrive between
// ticks.

/// One pending live terrain edit, waiting to be applied at the next
/// tick boundary.
#[derive(Debug, Clone)]
enum TerrainEdit {
    /// New chunk row appeared (initial subscription snapshot OR live
    /// `terrain_chunk_upsert` of a previously-absent chunk).
    Insert {
        terrain_set_id: u32,
        row_id: u64,
        vertices: Vec<f32>,
        indices: Vec<u32>,
    },
    /// Existing row was rebaked. Old colliders for this `row_id` must
    /// be removed first.
    Update {
        terrain_set_id: u32,
        row_id: u64,
        vertices: Vec<f32>,
        indices: Vec<u32>,
    },
    /// Editor removed a chunk. Old colliders for this `row_id` are
    /// dropped on every layer that was bound to its set.
    Delete { row_id: u64 },
}

/// Single (layer ↔ terrain_set name) binding. `set_id` is filled in
/// once the `terrain_set` row with that name is observed; before then
/// the chunk callbacks have no layer to route edits to and the queue
/// drain skips them.
#[derive(Debug, Clone)]
struct TerrainBinding {
    layer: u32,
    set_name: String,
    set_id: Option<u32>,
}

/// Worker-side voxel-terrain state.
#[derive(Debug, Default)]
struct TerrainState {
    bindings: Vec<TerrainBinding>,
    /// `row_id` → list of `(layer, opaque_handle)` for every layer this
    /// chunk has been applied to. A single chunk may be applied to
    /// multiple layers when multiple bindings share a `terrain_set_id`.
    chunk_handles: std::collections::HashMap<u64, Vec<(u32, u64)>>,
    pending_edits: Vec<TerrainEdit>,
}

impl TerrainState {
    fn with_bindings(bindings: Vec<TerrainBinding>) -> Self {
        Self {
            bindings,
            ..Self::default()
        }
    }

    /// Register a new (layer, set_name) binding. Called from
    /// `materialize_layer` callers when `terrain_set` is `Some`. If a
    /// `terrain_set` with that name has already been observed, the
    /// `set_id` is filled in immediately.
    fn register_binding(&mut self, layer: u32, set_name: String) {
        let set_id = self
            .bindings
            .iter()
            .find(|b| b.set_name == set_name && b.set_id.is_some())
            .and_then(|b| b.set_id);
        // Drop any prior binding for this layer (defensive; layer reuse
        // across instance recreation must not double-route).
        self.bindings.retain(|b| b.layer != layer);
        self.bindings.push(TerrainBinding {
            layer,
            set_name,
            set_id,
        });
    }

    /// Mark every binding with `set_name` as resolved to `set_id`.
    /// Returns the layers that newly became hydratable.
    fn resolve_set(&mut self, set_name: &str, set_id: u32) -> Vec<u32> {
        let mut newly = Vec::new();
        for b in &mut self.bindings {
            if b.set_name == set_name && b.set_id != Some(set_id) {
                b.set_id = Some(set_id);
                newly.push(b.layer);
            }
        }
        newly
    }

    /// Drop a binding (instance expiry path). Bulk
    /// `remove_environment_colliders_by_layer(layer)` already cleared
    /// the colliders themselves; this just cleans the tracking map.
    fn unbind_layer(&mut self, layer: u32) {
        self.bindings.retain(|b| b.layer != layer);
        for handles in self.chunk_handles.values_mut() {
            handles.retain(|(l, _)| *l != layer);
        }
        self.chunk_handles.retain(|_, h| !h.is_empty());
    }

    fn layers_for_set(&self, set_id: u32) -> Vec<u32> {
        self.bindings
            .iter()
            .filter(move |b| b.set_id == Some(set_id))
            .map(|b| b.layer)
            .collect()
    }

    fn enqueue(&mut self, edit: TerrainEdit) {
        self.pending_edits.push(edit);
    }

    /// Apply every queued edit to `physics`. Each `Insert` / `Update` /
    /// `Delete` becomes at most N collider operations where N is the
    /// number of layers bound to the chunk's set. Idempotent — clearing
    /// the queue costs zero when no edits arrived.
    fn drain_into(&mut self, physics: &mut dyn game_core::physics_backend::PhysicsBackend) {
        if self.pending_edits.is_empty() {
            return;
        }
        let edits = std::mem::take(&mut self.pending_edits);
        let mut applied_inserts = 0usize;
        let mut applied_updates = 0usize;
        let mut applied_deletes = 0usize;
        for edit in edits {
            match edit {
                TerrainEdit::Insert {
                    terrain_set_id,
                    row_id,
                    vertices,
                    indices,
                } => {
                    if self.apply_chunk(physics, terrain_set_id, row_id, vertices, indices) {
                        applied_inserts += 1;
                    }
                }
                TerrainEdit::Update {
                    terrain_set_id,
                    row_id,
                    vertices,
                    indices,
                } => {
                    self.drop_chunk_colliders(physics, row_id);
                    if self.apply_chunk(physics, terrain_set_id, row_id, vertices, indices) {
                        applied_updates += 1;
                    }
                }
                TerrainEdit::Delete { row_id } => {
                    if self.chunk_handles.contains_key(&row_id) {
                        self.drop_chunk_colliders(physics, row_id);
                        applied_deletes += 1;
                    }
                }
            }
        }
        if applied_inserts + applied_updates + applied_deletes > 0 {
            info!(
                "TerrainState: applied {applied_inserts} insert(s), {applied_updates} update(s), \
                 {applied_deletes} delete(s) at tick boundary"
            );
        }
    }

    /// Returns `true` if the chunk was applied to at least one layer.
    fn apply_chunk(
        &mut self,
        physics: &mut dyn game_core::physics_backend::PhysicsBackend,
        terrain_set_id: u32,
        row_id: u64,
        vertices: Vec<f32>,
        indices: Vec<u32>,
    ) -> bool {
        let layers = self.layers_for_set(terrain_set_id);
        if layers.is_empty() {
            return false;
        }
        // `TerrainEdit::Update` already cleared old handles; for `Insert`
        // there should be none. Defensive cleanup keeps the map tidy if
        // the editor double-inserts the same row.
        self.drop_chunk_colliders(physics, row_id);

        let mut new_handles = Vec::with_capacity(layers.len());
        for layer in layers {
            let shape = game_core::physics_backend::EnvironmentShape::TriMesh {
                vertices: vertices.clone(),
                indices: indices.clone(),
            };
            let opaque = physics.add_environment_collider_on_layer(
                shape,
                game_protocol::types::Vec3f::new(0.0, 0.0, 0.0),
                layer,
            );
            new_handles.push((layer, opaque));
        }
        self.chunk_handles.insert(row_id, new_handles);
        true
    }

    fn drop_chunk_colliders(
        &mut self,
        physics: &mut dyn game_core::physics_backend::PhysicsBackend,
        row_id: u64,
    ) {
        if let Some(handles) = self.chunk_handles.remove(&row_id) {
            for (_layer, opaque) in handles {
                physics.remove_environment_collider(opaque);
            }
        }
    }
}

/// Materialise environment colliders + collision policy for a layer.
///
/// Used by both worker startup (for each `WorldLayerDef`) and instance
/// creation (for each `DungeonTemplate`). Composes:
///
/// 1. Any pre-existing environment colliders on `layer` are removed
///    first so this call is idempotent. In particular, the layer-0
///    placeholder floor stamped by `PhysicsWorld::new` is replaced by
///    the authored `WorldLayerDef::geometry` when one exists.
/// 2. Hand-authored `geometry` — every `GeometryDef` becomes one
///    parentless environment collider stamped with `layer`.
/// 3. **Baked terrain binding** — when `terrain_set` is `Some`, the layer
///    is registered with `terrain` so that:
///      - any chunks already cached locally are hydrated immediately
///        (live path: instance creation after subscription is up), and
///      - any chunks arriving later are routed to this layer via the
///        `terrain_chunk.on_*` callbacks (covers initial subscription
///        and live editor edits both).
///    The collider rebuild itself happens through the deferred edit
///    queue (see `TerrainState::pending_edits`), draining at the start
///    of each tick. This keeps mid-tick BVH rebuilds out of the hot
///    path.
/// 4. The layer's `LayerCollisionPolicy`, registered last so policy
///    queries during the materialisation itself never observe a
///    half-built layer.
/// 4. The layer's `LayerCollisionPolicy`, registered last so policy
///    queries during the materialisation itself never observe a
///    half-built layer.
fn materialize_layer(
    physics: &mut dyn game_core::physics_backend::PhysicsBackend,
    layer: u32,
    geometry: &[game_schema::dungeon::GeometryDef],
    terrain_set: Option<&str>,
    policy: game_schema::dungeon::LayerCollisionPolicy,
) {
    physics.remove_environment_colliders_by_layer(layer);
    for geo in geometry {
        let shape = game_core::dungeon::shape_def_to_environment(&geo.shape);
        let pos = game_protocol::types::Vec3f {
            x: geo.position[0],
            y: geo.position[1],
            z: geo.position[2],
        };
        physics.add_environment_collider_on_layer(shape, pos, layer);
    }
    // Terrain hydration is owned by the caller via `TerrainState`: the
    // (layer, terrain_set name) binding is recorded there and chunks are
    // applied through the deferred edit queue (see §4.8b Phase 5). We
    // keep the parameter so this signature is uniform across both call
    // sites (`run()` startup loop and `Instance::on_insert`) and so the
    // intent is visible at the call site.
    let _ = terrain_set;
    physics.set_layer_policy(layer, policy);
}

/// Load encounter definitions from `data/encounters.ron`.
fn load_encounters() -> game_core::encounter::EncounterRegistry {
    use game_core::encounter::{EncounterFile, EncounterRegistry};
    const PATH: &str = "data/encounters.ron";
    let result = std::fs::read_to_string(PATH)
        .map_err(|e| format!("read '{PATH}': {e}"))
        .and_then(|src| {
            ron::from_str::<EncounterFile>(&src).map_err(|e| format!("parse '{PATH}': {e}"))
        });
    match result {
        Ok(file) => {
            let count = file.encounters.len();
            let mut reg = EncounterRegistry::new();
            for def in file.encounters {
                reg.register(def.name, def.rules);
            }
            info!("Loaded {count} encounter definition(s) from {PATH}");
            reg
        }
        Err(e) => {
            warn!("Could not load {PATH} ({e}) — using empty encounter registry");
            EncounterRegistry::new()
        }
    }
}

fn resolve_boss_encounter_rules(
    encounters: &game_core::encounter::EncounterRegistry,
    configured_key: Option<&str>,
) -> Option<(String, Vec<game_core::encounter::Rule>, bool)> {
    if let Some(key) = configured_key.filter(|k| !k.is_empty()) {
        if let Some(rules) = encounters.rules_for(key) {
            return Some((key.to_string(), rules, false));
        }
        return encounters
            .rules_for("default")
            .map(|rules| ("default".to_string(), rules, true));
    }

    encounters
        .rules_for("default")
        .map(|rules| ("default".to_string(), rules, false))
}

// ── Spawn-rules registry ────────────────────────────────────────

/// Load `data/spawn_rules.ron` (Phase 7.5).  Falls back to empty.
fn load_spawn_rules() -> game_core::spawn_rules::SpawnRulesRegistry {
    use game_core::spawn_rules::SpawnRulesRegistry;
    const PATH: &str = "data/spawn_rules.ron";
    let result = std::fs::read_to_string(PATH)
        .map_err(|e| format!("read '{PATH}': {e}"))
        .and_then(|src| {
            SpawnRulesRegistry::from_ron(&src).map_err(|e| format!("parse '{PATH}': {e}"))
        });
    match result {
        Ok(reg) => {
            info!("Loaded {} spawn rule(s) from {PATH}", reg.len());
            reg
        }
        Err(e) => {
            warn!("Could not load {PATH} ({e}) — using empty spawn-rules registry");
            SpawnRulesRegistry::new()
        }
    }
}

/// Aggregate equipment modifiers for `entity_id` from the SDK cache and push
/// the result to the simulation runner.
fn recompute_equipment(
    db: &crate::module_bindings::RemoteTables,
    state: &mut CoordinatorState,
    entity_id: EntityId,
) {
    let modifiers = game_core::stats::EquipmentModifiers::aggregate(
        db.player_equipment()
            .iter()
            .filter(|row| row.owner_entity == entity_id.0)
            .map(|row| state.items.modifiers(row.item_id)),
    );
    state.sim.update_equipment(entity_id, modifiers);
}

// ── Binding conversions ─────────────────────────────────────────
//
// The generated bindings produce mirror types of game_schema types.
// We convert between them here since they are distinct Rust types.

/// Convert generated binding's IntentAction → game_protocol's IntentAction.
fn convert_intent_action(
    action: crate::module_bindings::IntentAction,
) -> game_protocol::intent::IntentAction {
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
                target_hint: u.target_hint,
            })
        }
        crate::module_bindings::IntentAction::ReleaseAbility(id) => {
            game_protocol::intent::IntentAction::ReleaseAbility(id)
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
        crate::module_bindings::IntentAction::Block(d) => {
            game_protocol::intent::IntentAction::Block(game_schema::BlockData {
                look_dir: game_schema::MoveDir {
                    dir_x: d.look_dir.dir_x,
                    dir_y: d.look_dir.dir_y,
                    dir_z: d.look_dir.dir_z,
                },
            })
        }
        crate::module_bindings::IntentAction::Jump => game_protocol::intent::IntentAction::Jump,
        crate::module_bindings::IntentAction::WeaponSwap => {
            game_protocol::intent::IntentAction::WeaponSwap
        }
        crate::module_bindings::IntentAction::TagTarget(id) => {
            game_protocol::intent::IntentAction::TagTarget(id)
        }
    }
}

fn convert_ability_target(
    target: crate::module_bindings::AbilityTarget,
) -> game_schema::AbilityTarget {
    match target {
        crate::module_bindings::AbilityTarget::None => game_schema::AbilityTarget::None,
        crate::module_bindings::AbilityTarget::Entity(id) => game_schema::AbilityTarget::Entity(id),
        crate::module_bindings::AbilityTarget::Position(v) => {
            game_schema::AbilityTarget::Position(game_schema::Vec3f {
                x: v.x,
                y: v.y,
                z: v.z,
            })
        }
        crate::module_bindings::AbilityTarget::Direction(v) => {
            game_schema::AbilityTarget::Direction(game_schema::Vec3f {
                x: v.x,
                y: v.y,
                z: v.z,
            })
        }
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
        crate::module_bindings::EntityKind::Prop => game_schema::EntityKind::Prop,
    }
}

fn convert_entity_state(state: crate::module_bindings::EntityState) -> game_schema::EntityState {
    match state {
        crate::module_bindings::EntityState::Spawning => game_schema::EntityState::Spawning,
        crate::module_bindings::EntityState::Active => game_schema::EntityState::Active,
        crate::module_bindings::EntityState::DespawnPending => {
            game_schema::EntityState::DespawnPending
        }
        crate::module_bindings::EntityState::Removed => game_schema::EntityState::Removed,
    }
}

fn convert_npc_ai_state(state: crate::module_bindings::NpcAiState) -> game_schema::NpcAiState {
    match state {
        crate::module_bindings::NpcAiState::Idle => game_schema::NpcAiState::Idle,
        crate::module_bindings::NpcAiState::Patrol => game_schema::NpcAiState::Patrol,
        crate::module_bindings::NpcAiState::Combat => game_schema::NpcAiState::Combat,
        crate::module_bindings::NpcAiState::Flee => game_schema::NpcAiState::Flee,
        crate::module_bindings::NpcAiState::Scripted => game_schema::NpcAiState::Scripted,
        crate::module_bindings::NpcAiState::Evade => game_schema::NpcAiState::Evade,
    }
}

fn convert_interactable_info(
    row: &crate::module_bindings::InteractableConfig,
) -> game_core::sim_state::InteractableInfo {
    use game_core::sim_state::{InteractableInfo, SimInteractKind, SimInteractState};
    let kind = match row.interact_kind {
        crate::module_bindings::InteractKind::Switch => SimInteractKind::Switch,
        crate::module_bindings::InteractKind::Gate => SimInteractKind::Gate,
        crate::module_bindings::InteractKind::Grab => SimInteractKind::Grab,
        crate::module_bindings::InteractKind::Chest => SimInteractKind::Chest,
    };
    let state = match row.state {
        crate::module_bindings::InteractState::Idle => SimInteractState::Idle,
        crate::module_bindings::InteractState::Active => SimInteractState::Active,
        crate::module_bindings::InteractState::Cooldown => SimInteractState::Cooldown,
    };
    InteractableInfo {
        kind,
        linked_entity: row.linked_entity.map(EntityId),
        required_buff: row.required_buff,
        required_item: row.required_item,
        interact_range: row.interact_range,
        script_id: row.script_id.clone(),
        tags: row.tags.clone(),
        puzzle_group: row.puzzle_group.clone(),
        puzzle_required_count: row.puzzle_required_count,
        puzzle_window_ticks: row.puzzle_window_ticks,
        state,
    }
}

// ── CommitPackage → SDK wire type conversions ───────────────────
//
// Each function does a 1:1 structural mapping from the SDK-free
// intermediate types in `commit_builder` to the generated SDK bindings.

fn wire_transforms(pkg: &CommitPackage) -> Vec<TransformUpdate> {
    pkg.transforms
        .iter()
        .map(|t| TransformUpdate {
            entity_id: t.entity_id,
            pos_x: t.pos_x,
            pos_y: t.pos_y,
            pos_z: t.pos_z,
            rot_x: t.rot_x,
            rot_y: t.rot_y,
            rot_z: t.rot_z,
            rot_w: t.rot_w,
            vel_x: t.vel_x,
            vel_y: t.vel_y,
            vel_z: t.vel_z,
            angvel_x: t.angvel_x,
            angvel_y: t.angvel_y,
            angvel_z: t.angvel_z,
        })
        .collect()
}

fn wire_health_updates(pkg: &CommitPackage) -> Vec<HealthUpdate> {
    pkg.health_updates
        .iter()
        .map(|h| HealthUpdate {
            entity_id: h.entity_id,
            hp: h.hp,
            max_hp: h.max_hp,
        })
        .collect()
}

fn wire_combat_events(pkg: &CommitPackage) -> Vec<CombatEventInput> {
    pkg.combat_events.iter().map(wire_combat_event).collect()
}

fn wire_combat_event(e: &CommitCombatEvent) -> CombatEventInput {
    CombatEventInput {
        source_entity: e.source_entity,
        target_entity: e.target_entity,
        event_sequence: e.event_sequence,
        event_kind: match &e.event_kind {
            CommitCombatEventKind::CastStart {
                ability_id,
                cast_duration_ticks,
            } => CombatEventKind::CastStart(CastStartData {
                ability_id: *ability_id,
                cast_duration_ticks: *cast_duration_ticks,
            }),
            CommitCombatEventKind::ChargeStart {
                ability_id,
                max_ticks,
            } => CombatEventKind::ChargeStart(ChargeStartData {
                ability_id: *ability_id,
                max_ticks: *max_ticks,
            }),
            CommitCombatEventKind::ChargeTierReached { ability_id, tier } => {
                CombatEventKind::ChargeTierReached(ChargeTierReachedData {
                    ability_id: *ability_id,
                    tier: *tier,
                })
            }
            CommitCombatEventKind::BlockStart => CombatEventKind::BlockStart,
            CommitCombatEventKind::BlockEnd => CombatEventKind::BlockEnd,
            CommitCombatEventKind::Damage(d) => CombatEventKind::Damage(DamageData {
                amount: d.amount,
                damage_type: wire_damage_type(d.damage_type),
            }),
            CommitCombatEventKind::SkillHit(id) => CombatEventKind::SkillHit(*id),
            CommitCombatEventKind::BuffApplied(b) => {
                CombatEventKind::BuffApplied(BuffAppliedData {
                    buff_id: b.buff_id,
                    duration_ticks: b.duration_ticks,
                })
            }
            CommitCombatEventKind::BuffExpired(id) => CombatEventKind::BuffExpired(*id),
            CommitCombatEventKind::EntityDied(k) => CombatEventKind::EntityDied(*k),
            CommitCombatEventKind::Dodged(ability_id) => CombatEventKind::Dodged(*ability_id),
            CommitCombatEventKind::Blocked {
                ability_id,
                damage_taken,
                perfect,
            } => CombatEventKind::Blocked(BlockedData {
                ability_id: *ability_id,
                damage_taken: *damage_taken,
                perfect: *perfect,
            }),
            CommitCombatEventKind::Covered {
                blocker,
                ability_id,
                damage_taken,
            } => CombatEventKind::Covered(CoveredData {
                blocker: *blocker,
                ability_id: *ability_id,
                damage_taken: *damage_taken,
            }),
            CommitCombatEventKind::TelegraphWarning {
                target,
                impact_tick,
            } => CombatEventKind::TelegraphWarning(TelegraphWarningData {
                target: *target,
                impact_tick: *impact_tick,
            }),
            CommitCombatEventKind::AreaTelegraph {
                ability_id,
                pos_x,
                pos_y,
                pos_z,
                radius,
                shape,
                impact_tick,
            } => CombatEventKind::AreaTelegraph(AreaTelegraphData {
                ability_id: *ability_id,
                pos_x: *pos_x,
                pos_y: *pos_y,
                pos_z: *pos_z,
                radius: *radius,
                shape: shape.clone(),
                impact_tick: *impact_tick,
            }),
            CommitCombatEventKind::EncounterCue {
                cue_id,
                anchor_entity,
                pos_x,
                pos_y,
                pos_z,
                shape,
                inner_radius,
                outer_radius,
                half_height,
                starts_at_tick,
                expires_at_tick,
            } => CombatEventKind::EncounterCue(EncounterCueData {
                cue_id: cue_id.clone(),
                anchor_entity: *anchor_entity,
                pos_x: *pos_x,
                pos_y: *pos_y,
                pos_z: *pos_z,
                shape: shape.clone(),
                inner_radius: *inner_radius,
                outer_radius: *outer_radius,
                half_height: *half_height,
                starts_at_tick: *starts_at_tick,
                expires_at_tick: *expires_at_tick,
            }),
            CommitCombatEventKind::LockOnAcquired => CombatEventKind::LockOnAcquired,
            CommitCombatEventKind::LockOnSessionStarted { ability_id } => {
                CombatEventKind::LockOnSessionStarted(LockOnSessionStartedData {
                    ability_id: *ability_id,
                })
            }
            CommitCombatEventKind::LockOnCanceled { target } => {
                CombatEventKind::LockOnCanceled(LockOnCanceledData { target: *target })
            }
            CommitCombatEventKind::LockOnFired { targets } => {
                CombatEventKind::LockOnFired(LockOnFiredData {
                    targets: targets.clone(),
                })
            }
            CommitCombatEventKind::ProjectileLaunched {
                execution_id,
                ability_id,
                origin_x,
                origin_y,
                origin_z,
                direction_x,
                direction_y,
                direction_z,
                speed,
                max_range,
            } => CombatEventKind::ProjectileLaunched(ProjectileLaunchedData {
                execution_id: *execution_id,
                ability_id: *ability_id,
                origin_x: *origin_x,
                origin_y: *origin_y,
                origin_z: *origin_z,
                direction_x: *direction_x,
                direction_y: *direction_y,
                direction_z: *direction_z,
                speed: *speed,
                max_range: *max_range,
            }),
            CommitCombatEventKind::HazardSpawned {
                execution_id,
                ability_id,
                pos_x,
                pos_y,
                pos_z,
                radius,
            } => CombatEventKind::HazardSpawned(HazardSpawnedData {
                execution_id: *execution_id,
                ability_id: *ability_id,
                pos_x: *pos_x,
                pos_y: *pos_y,
                pos_z: *pos_z,
                radius: *radius,
            }),
            CommitCombatEventKind::ContactHitboxSpawned {
                execution_id,
                parent_execution_id,
                ability_id,
                pos_x,
                pos_y,
                pos_z,
                radius,
                duration_ticks,
            } => CombatEventKind::ContactHitboxSpawned(ContactHitboxSpawnedData {
                execution_id: *execution_id,
                parent_execution_id: *parent_execution_id,
                ability_id: *ability_id,
                pos_x: *pos_x,
                pos_y: *pos_y,
                pos_z: *pos_z,
                radius: *radius,
                duration_ticks: *duration_ticks,
            }),
            CommitCombatEventKind::SkillObjectRemoved { execution_id } => {
                CombatEventKind::SkillObjectRemoved(*execution_id)
            }
            CommitCombatEventKind::Teleported {
                from_x,
                from_y,
                from_z,
                to_x,
                to_y,
                to_z,
            } => CombatEventKind::Teleported(TeleportedData {
                from_x: *from_x,
                from_y: *from_y,
                from_z: *from_z,
                to_x: *to_x,
                to_y: *to_y,
                to_z: *to_z,
            }),
            CommitCombatEventKind::Knockback { force } => {
                CombatEventKind::Knockback(KnockbackData { force: *force })
            }
            CommitCombatEventKind::Launched => CombatEventKind::Launched,
            CommitCombatEventKind::Stunned { duration_ticks } => {
                CombatEventKind::Stunned(StunnedData {
                    duration_ticks: *duration_ticks,
                })
            }
            CommitCombatEventKind::KnockedDown { duration_ticks } => {
                CombatEventKind::KnockedDown(KnockedDownData {
                    duration_ticks: *duration_ticks,
                })
            }
            CommitCombatEventKind::Pulled => CombatEventKind::Pulled,
            CommitCombatEventKind::Slept { duration_ticks } => CombatEventKind::Slept(SleptData {
                duration_ticks: *duration_ticks,
            }),
            CommitCombatEventKind::Silenced { duration_ticks } => {
                CombatEventKind::Silenced(SilencedData {
                    duration_ticks: *duration_ticks,
                })
            }
            CommitCombatEventKind::Feared { duration_ticks } => {
                CombatEventKind::Feared(FearedData {
                    duration_ticks: *duration_ticks,
                })
            }
            CommitCombatEventKind::StabilityConsumed { buff_id } => {
                CombatEventKind::StabilityConsumed(*buff_id)
            }
            CommitCombatEventKind::WeaponSwapped { new_set } => {
                CombatEventKind::WeaponSwapped(*new_set)
            }
            CommitCombatEventKind::CCCleared { cc_effect, source } => {
                CombatEventKind::CcCleared(CcClearedData {
                    cc_effect: wire_cc_effect(*cc_effect),
                    source: *source,
                })
            }
            CommitCombatEventKind::Cleansed { count, source } => {
                CombatEventKind::Cleansed(CleansedData {
                    count: *count,
                    source: *source,
                })
            }
            CommitCombatEventKind::Stunbreak => CombatEventKind::Stunbreak,
            CommitCombatEventKind::CCImmune { cc_effect, source } => {
                CombatEventKind::CcImmune(CcImmuneData {
                    cc_effect: wire_cc_effect(*cc_effect),
                    source: *source,
                })
            }
            CommitCombatEventKind::Healed { amount } => {
                CombatEventKind::Healed(HealedData { amount: *amount })
            }
        },
    }
}

fn wire_world_events(pkg: &CommitPackage) -> Vec<WorldEventInput> {
    pkg.world_events
        .iter()
        .map(|e| WorldEventInput {
            entity_id: e.entity_id,
            event_sequence: e.event_sequence,
            event_kind: match &e.event_kind {
                CommitWorldEventKind::EntityDespawned => WorldEventKind::EntityDespawned,
                CommitWorldEventKind::PickupCollected(id) => WorldEventKind::PickupCollected(*id),
                CommitWorldEventKind::InteractTriggered(target) => {
                    WorldEventKind::InteractTriggered(*target)
                }
            },
        })
        .collect()
}

fn wire_entity_state_updates(pkg: &CommitPackage) -> Vec<EntityStateUpdate> {
    pkg.entity_state_updates
        .iter()
        .map(|u| EntityStateUpdate {
            entity_id: u.entity_id,
            new_state: match u.new_state {
                CommitEntityStateKind::Spawning => EntityState::Spawning,
                CommitEntityStateKind::Active => EntityState::Active,
                CommitEntityStateKind::DespawnPending => EntityState::DespawnPending,
                CommitEntityStateKind::Removed => EntityState::Removed,
            },
        })
        .collect()
}

fn wire_region_updates(pkg: &CommitPackage) -> Vec<RegionUpdate> {
    pkg.region_updates
        .iter()
        .map(|r| RegionUpdate {
            entity_id: r.entity_id,
            region_x: r.region_x,
            region_z: r.region_z,
            layer: r.layer,
        })
        .collect()
}

fn wire_buff_updates(pkg: &CommitPackage) -> Vec<BuffUpdate> {
    pkg.buff_updates
        .iter()
        .map(|b| BuffUpdate {
            entity_id: b.entity_id,
            buff_id: b.buff_id,
            source_entity: b.source_entity,
            stacks: b.stacks,
            expires_at_tick: b.expires_at_tick,
            mod_ai_override_kind: b.mod_ai_override_kind,
            mod_ai_override_target: b.mod_ai_override_target,
            mod_stealth: b.mod_stealth,
            last_dot_tick: b.last_dot_tick,
        })
        .collect()
}

fn wire_npc_state_updates(pkg: &CommitPackage) -> Vec<NpcStateUpdate> {
    pkg.npc_state_updates
        .iter()
        .map(|n| NpcStateUpdate {
            entity_id: n.entity_id,
            ai_state: wire_npc_ai_state(n.ai_state),
            target_entity: n.target_entity,
        })
        .collect()
}

fn wire_director_spawns(pkg: &CommitPackage) -> Vec<DirectorSpawnInput> {
    pkg.director_spawns
        .iter()
        .map(|s| DirectorSpawnInput {
            kind: convert_entity_kind_to_wire(s.kind),
            max_hp: s.max_hp,
            pos_x: s.pos_x,
            pos_y: s.pos_y,
            pos_z: s.pos_z,
            layer: s.layer,
        })
        .collect()
}

fn wire_encounter_memberships(pkg: &CommitPackage) -> Vec<EncounterAddMembershipInput> {
    pkg.encounter_memberships
        .iter()
        .map(|m| EncounterAddMembershipInput {
            spawn_index: m.spawn_index,
            boss_entity: m.boss_entity,
            archetype: m.archetype.clone(),
            tags: m.tags.clone(),
        })
        .collect()
}

fn convert_entity_kind_to_wire(
    kind: game_schema::EntityKind,
) -> crate::module_bindings::EntityKind {
    match kind {
        game_schema::EntityKind::Player => crate::module_bindings::EntityKind::Player,
        game_schema::EntityKind::Npc => crate::module_bindings::EntityKind::Npc,
        game_schema::EntityKind::Projectile => crate::module_bindings::EntityKind::Projectile,
        game_schema::EntityKind::Hazard => crate::module_bindings::EntityKind::Hazard,
        game_schema::EntityKind::Boss => crate::module_bindings::EntityKind::Boss,
        game_schema::EntityKind::Prop => crate::module_bindings::EntityKind::Prop,
    }
}

fn wire_death_state_inserts(pkg: &CommitPackage) -> Vec<DeathStateInsertInput> {
    pkg.death_state_inserts
        .iter()
        .map(|d| DeathStateInsertInput {
            entity_id: d.entity_id,
            killer_entity: d.killer_entity,
            layer: d.layer,
            death_pos_x: d.death_pos_x,
            death_pos_y: d.death_pos_y,
            death_pos_z: d.death_pos_z,
        })
        .collect()
}

/// Convert pipeline sim_warnings into `SimLogInput` wire entries (level=Warn).
fn wire_sim_log_entries(pkg: &CommitPackage) -> Vec<SimLogInput> {
    pkg.sim_logs
        .iter()
        .map(|msg| SimLogInput {
            level: 2, // Warn
            message: msg.clone(),
        })
        .collect()
}

fn wire_interactable_updates(pkg: &CommitPackage) -> Vec<InteractableUpdate> {
    pkg.interactable_updates
        .iter()
        .map(|u| {
            let new_state = match u.new_state {
                game_core::sim_state::SimInteractState::Idle => {
                    crate::module_bindings::InteractState::Idle
                }
                game_core::sim_state::SimInteractState::Active => {
                    crate::module_bindings::InteractState::Active
                }
                game_core::sim_state::SimInteractState::Cooldown => {
                    crate::module_bindings::InteractState::Cooldown
                }
            };
            InteractableUpdate {
                entity_id: u.entity_id,
                state: new_state,
            }
        })
        .collect()
}

fn wire_npc_ai_state(state: game_schema::NpcAiState) -> NpcAiState {
    match state {
        game_schema::NpcAiState::Idle => NpcAiState::Idle,
        game_schema::NpcAiState::Patrol => NpcAiState::Patrol,
        game_schema::NpcAiState::Combat => NpcAiState::Combat,
        game_schema::NpcAiState::Flee => NpcAiState::Flee,
        game_schema::NpcAiState::Scripted => NpcAiState::Scripted,
        game_schema::NpcAiState::Evade => NpcAiState::Evade,
    }
}

fn wire_damage_type(dt: game_schema::DamageType) -> DamageType {
    match dt {
        game_schema::DamageType::Physical => DamageType::Physical,
        game_schema::DamageType::Magical => DamageType::Magical,
        game_schema::DamageType::True => DamageType::True,
    }
}

fn wire_cc_effect(cc: game_schema::CCEffect) -> CcEffect {
    match cc {
        game_schema::CCEffect::Stun => CcEffect::Stun,
        game_schema::CCEffect::Knockdown => CcEffect::Knockdown,
        game_schema::CCEffect::Sleep => CcEffect::Sleep,
        game_schema::CCEffect::Silence => CcEffect::Silence,
        game_schema::CCEffect::Fear => CcEffect::Fear,
        game_schema::CCEffect::Knockback => CcEffect::Knockback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrain_state_register_resolve_unbind_roundtrip() {
        let mut t = TerrainState::default();
        t.register_binding(100, "alpha".into());
        t.register_binding(101, "alpha".into());
        t.register_binding(102, "beta".into());
        // Before resolve, no set_id is bound.
        assert!(t.layers_for_set(7).is_empty());

        let newly = t.resolve_set("alpha", 7);
        assert_eq!(newly.len(), 2);
        let mut layers = t.layers_for_set(7);
        layers.sort();
        assert_eq!(layers, vec![100, 101]);
        assert!(t.layers_for_set(8).is_empty());

        // Re-resolving with the same id is a no-op (no double-counting).
        let newly2 = t.resolve_set("alpha", 7);
        assert!(newly2.is_empty());

        // A subsequent register_binding on a resolved set name picks up
        // the existing set_id immediately.
        t.register_binding(103, "alpha".into());
        let mut layers = t.layers_for_set(7);
        layers.sort();
        assert_eq!(layers, vec![100, 101, 103]);

        t.unbind_layer(101);
        let mut layers = t.layers_for_set(7);
        layers.sort();
        assert_eq!(layers, vec![100, 103]);
    }

    #[test]
    fn terrain_state_drain_empty_is_noop() {
        let mut t = TerrainState::default();
        let mut physics = crate::physics::rapier_world::PhysicsWorld::new(0.05);
        // Should not touch physics or panic.
        t.drain_into(&mut physics);
        assert!(t.pending_edits.is_empty());
        assert!(t.chunk_handles.is_empty());
    }

    #[test]
    fn terrain_state_drain_skips_unbound_set() {
        let mut t = TerrainState::default();
        let mut physics = crate::physics::rapier_world::PhysicsWorld::new(0.05);
        t.enqueue(TerrainEdit::Insert {
            terrain_set_id: 99,
            row_id: 1,
            vertices: vec![0.0; 9],
            indices: vec![0, 1, 2],
        });
        t.drain_into(&mut physics);
        // No binding -> chunk is dropped without recording a handle.
        assert!(t.chunk_handles.is_empty());
        assert!(t.pending_edits.is_empty());
    }

    #[test]
    fn terrain_state_apply_then_delete_swaps_chunk_per_layer() {
        use game_core::physics_backend::PhysicsBackend;

        let mut t = TerrainState::default();
        t.register_binding(200, "x".into());
        t.register_binding(201, "x".into());
        t.resolve_set("x", 42);

        let mut physics = crate::physics::rapier_world::PhysicsWorld::new(0.05);
        // Single triangle in the XZ plane.
        let verts = vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];
        let idx = vec![0u32, 1, 2];

        t.enqueue(TerrainEdit::Insert {
            terrain_set_id: 42,
            row_id: 7,
            vertices: verts.clone(),
            indices: idx.clone(),
        });
        t.drain_into(&mut physics);
        assert_eq!(t.chunk_handles.get(&7).map(|v| v.len()), Some(2));

        // Update replaces handles for the same row.
        t.enqueue(TerrainEdit::Update {
            terrain_set_id: 42,
            row_id: 7,
            vertices: verts,
            indices: idx,
        });
        t.drain_into(&mut physics);
        assert_eq!(t.chunk_handles.get(&7).map(|v| v.len()), Some(2));

        // Delete drops the row entirely.
        t.enqueue(TerrainEdit::Delete { row_id: 7 });
        t.drain_into(&mut physics);
        assert!(!t.chunk_handles.contains_key(&7));

        // Layer unbind drops the binding and clears the (now empty) map.
        t.unbind_layer(200);
        t.unbind_layer(201);
        // Removing a layer should also clear environment colliders via
        // the worker's bulk path; here we just check our tracking.
        assert!(t.bindings.is_empty());
        // Sanity: physics isn't broken.
        let _ = physics.as_any();
    }

    #[test]
    fn convert_intent_action_preserves_target_hint() {
        let action = crate::module_bindings::IntentAction::UseAbility(
            crate::module_bindings::UseAbilityData {
                ability_id: 7,
                target: crate::module_bindings::AbilityTarget::Direction(
                    crate::module_bindings::Vec3F {
                        x: 1.0,
                        y: 0.0,
                        z: 0.0,
                    },
                ),
                target_hint: Some(42),
            },
        );

        let converted = convert_intent_action(action);
        match converted {
            game_protocol::intent::IntentAction::UseAbility(data) => {
                assert_eq!(data.target_hint, Some(42));
                match data.target {
                    game_schema::AbilityTarget::Direction(dir) => {
                        assert_eq!(
                            dir,
                            game_schema::Vec3f {
                                x: 1.0,
                                y: 0.0,
                                z: 0.0
                            }
                        );
                    }
                    other => panic!("expected direction target, got {other:?}"),
                }
            }
            other => panic!("expected UseAbility action, got {other:?}"),
        }
    }

    #[test]
    fn load_abilities_parses_fireball_as_aim_assist() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("abilities.ron");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let file: AbilityFile = ron::from_str(&source)
            .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
        let fireball = file
            .abilities
            .into_iter()
            .find(|ability| ability.ability_id == 2)
            .expect("Fireball ability should be present in data/abilities.ron");

        assert_eq!(
            fireball.targeting_mode,
            game_core::combat::skill::TargetingMode::AimAssist
        );
    }

    #[test]
    fn load_abilities_parses_backstab_as_raycast_strict() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("data")
            .join("abilities.ron");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let file: AbilityFile = ron::from_str(&source)
            .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
        let backstab = file
            .abilities
            .into_iter()
            .find(|ability| ability.ability_id == 21)
            .expect("Backstab ability should be present in data/abilities.ron");

        assert_eq!(
            backstab.targeting_mode,
            game_core::combat::skill::TargetingMode::RaycastStrict
        );
    }

    #[test]
    fn materialize_layer_replaces_existing_geometry() {
        use game_core::physics_backend::PhysicsBackend;
        use game_schema::dungeon::{GeometryDef, LayerCollisionPolicy, ShapeDef};

        // PhysicsWorld::new stamps a placeholder cuboid floor on layer 0
        // (top at y≈0.1). Materialising an authored layer 0 must REPLACE
        // the placeholder, not stack on top of it.
        //
        // Verification via public API: author a floor at y=-20 (below the
        // placeholder). A downward ray from y=50 must hit y≈-20 (top of
        // authored floor). If the placeholder were still present, the ray
        // would hit y≈0.1 first.
        let mut world = PhysicsWorld::new(1.0 / 60.0);
        let geometry = vec![GeometryDef {
            shape: ShapeDef::Cuboid {
                half_x: 50.0,
                half_y: 0.5,
                half_z: 50.0,
            },
            position: [0.0, -20.0, 0.0],
        }];
        materialize_layer(
            &mut world,
            0,
            &geometry,
            None,
            LayerCollisionPolicy::default(),
        );
        world.step();

        let hit = world
            .raycast_surface(
                game_protocol::types::Vec3f::new(0.0, 50.0, 0.0),
                game_protocol::types::Vec3f::new(0.0, -1.0, 0.0),
                200.0,
                0,
            )
            .expect("authored layer-0 floor must be hit");
        // Top of authored cuboid is y = -20 + 0.5 = -19.5.
        assert!(
            (hit.y - (-19.5)).abs() < 0.2,
            "expected y≈-19.5 (authored floor), got y={} (placeholder not replaced?)",
            hit.y
        );
    }

    #[test]
    fn materialize_layer_combines_authored_geometry_and_terrain_set_request() {
        use game_core::physics_backend::PhysicsBackend;
        use game_schema::dungeon::{GeometryDef, LayerCollisionPolicy, ShapeDef};

        // Hand-authored geometry alongside a terrain_set reference.
        // The terrain_set hookup is a no-op until §4.8b Phase 1+5 lands,
        // but the authored geometry must still materialise on the layer.
        let mut world = PhysicsWorld::new(1.0 / 60.0);
        let geometry = vec![GeometryDef {
            shape: ShapeDef::Cuboid {
                half_x: 5.0,
                half_y: 0.5,
                half_z: 5.0,
            },
            position: [0.0, 5.0, 0.0],
        }];
        materialize_layer(
            &mut world,
            42,
            &geometry,
            Some("future_terrain_set"),
            LayerCollisionPolicy::default(),
        );
        world.step();

        // Layer-42 caster sees the authored floor (top at y=5.5).
        let hit = world
            .raycast_surface(
                game_protocol::types::Vec3f::new(0.0, 20.0, 0.0),
                game_protocol::types::Vec3f::new(0.0, -1.0, 0.0),
                100.0,
                42,
            )
            .expect("authored geometry must materialise even when terrain_set is requested");
        assert!(
            (hit.y - 5.5).abs() < 0.2,
            "expected authored floor at y≈5.5, got {}",
            hit.y
        );
    }

    #[test]
    fn shipped_layers_ron_parses() {
        use game_schema::dungeon::WorldLayersFile;
        let src = include_str!("../../../data/layers.ron");
        let file: WorldLayersFile = ron::from_str(src).expect("data/layers.ron must be valid RON");
        assert!(
            !file.layers.is_empty(),
            "shipped layers.ron should declare at least one static layer"
        );
        for l in &file.layers {
            assert!(
                l.layer_id < 100,
                "static layer '{}' has reserved-range conflict (layer_id={} >= 100)",
                l.name,
                l.layer_id
            );
            assert!(!l.name.is_empty(), "layer name must not be empty");
        }
    }

    fn one_rule(
        phase: game_core::encounter::BossPhase,
    ) -> Vec<game_core::encounter::EncounterRule> {
        vec![game_core::encounter::EncounterRule {
            trigger: game_core::encounter::EncounterTrigger::OnHpBelowOnce { percent: 0.5 },
            action: game_core::encounter::EncounterAction::ChangePhase { phase },
            fired: false,
        }]
    }

    #[test]
    fn resolve_boss_encounter_rules_prefers_configured_key() {
        let mut reg = game_core::encounter::EncounterRegistry::new();
        reg.register(
            "default".to_string(),
            one_rule(game_core::encounter::BossPhase::Phase2),
        );
        reg.register(
            "state_enter_demo".to_string(),
            one_rule(game_core::encounter::BossPhase::Phase3),
        );

        let (key, rules, fell_back) =
            resolve_boss_encounter_rules(&reg, Some("state_enter_demo")).expect("rules");
        assert_eq!(key, "state_enter_demo");
        assert!(!fell_back);
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn resolve_boss_encounter_rules_falls_back_to_default() {
        let mut reg = game_core::encounter::EncounterRegistry::new();
        reg.register(
            "default".to_string(),
            one_rule(game_core::encounter::BossPhase::Phase2),
        );

        let (key, rules, fell_back) =
            resolve_boss_encounter_rules(&reg, Some("missing_key")).expect("fallback rules");
        assert_eq!(key, "default");
        assert!(fell_back);
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn resolve_boss_encounter_rules_uses_default_when_unconfigured() {
        let mut reg = game_core::encounter::EncounterRegistry::new();
        reg.register(
            "default".to_string(),
            one_rule(game_core::encounter::BossPhase::Phase2),
        );

        let (key, rules, fell_back) =
            resolve_boss_encounter_rules(&reg, None).expect("default rules");
        assert_eq!(key, "default");
        assert!(!fell_back);
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn resolve_boss_encounter_rules_none_when_no_matches() {
        let reg = game_core::encounter::EncounterRegistry::new();
        assert!(resolve_boss_encounter_rules(&reg, Some("missing_key")).is_none());
        assert!(resolve_boss_encounter_rules(&reg, None).is_none());
    }
}
