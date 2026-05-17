use spacetimedb::{reducer, ReducerContext, Table, TimeDuration};
use crate::tables::*;

// ── Lifecycle ───────────────────────────────────────────────────────

#[reducer(init)]
pub fn init(ctx: &ReducerContext) {
    log::info!("Module initializing — seeding tick 0");

    // Store the publishing identity as admin for owner-only reducers.
    ctx.db.module_config().insert(ModuleConfig {
        key: 0,
        admin: ctx.sender(),
        last_committed_tick: 0,
    });

    // Seed tick 0
    ctx.db.sim_tick().insert(SimTick {
        tick_id: 0,
        timestamp_us: ctx.timestamp.to_micros_since_unix_epoch(),
    });

    // Start the simulation tick loop at 20Hz (50ms interval).
    // Per spec: scheduled reducers are best-effort, may be delayed.
    let interval = TimeDuration::from_micros(50_000);
    ctx.db.tick_schedule().insert(TickSchedule {
        scheduled_id: 0,
        scheduled_at: interval.into(),
    });

    log::info!("Tick scheduler started at 20Hz");

    // Start the world clock at 30s interval for Tier 2 aggregate processing.
    let world_clock_interval = TimeDuration::from_micros(30_000_000);
    ctx.db.world_clock_schedule().insert(WorldClockSchedule {
        scheduled_id: 0,
        scheduled_at: world_clock_interval.into(),
    });
    log::info!("World clock started at 30s interval");
}

#[reducer(client_connected)]
pub fn client_connected(ctx: &ReducerContext) {
    let caller = ctx.sender();
    log::info!("Client connected: {:?}", caller);
}

#[reducer(client_disconnected)]
pub fn client_disconnected(ctx: &ReducerContext) {
    let caller = ctx.sender();
    log::info!("Client disconnected: {:?}", caller);
}

// ── Tick Trigger ────────────────────────────────────────────────────
// Scheduler-only reducer that appends the next canonical tick row.

/// Maximum number of unacknowledged sim_tick rows before tick_trigger pauses.
/// At 20 Hz a backlog of 5 means the worker is >250 ms behind — pause and let it catch up.
const BACKLOG_LIMIT: u64 = 5;

/// Number of sim_tick rows to retain for coordinator restart seeding (6 seconds at 20 Hz).
/// Older rows are pruned each tick to prevent unbounded table growth.
const SIM_TICK_RETAIN: u64 = 120;

/// Default respawn delay in ticks (5 seconds at 20 Hz).
const RESPAWN_DELAY_TICKS: u64 = 100;

/// Party invite expiry in microseconds (60 seconds).
const INVITE_EXPIRE_MICROS: i64 = 60_000_000;

#[reducer]
pub fn tick_trigger(ctx: &ReducerContext, _schedule: TickSchedule) -> Result<(), String> {
    if ctx.sender() != ctx.identity() {
        return Err("tick_trigger may only be invoked by the scheduler".into());
    }

    // Read current max tick from sim_tick.
    let mut current_max = ctx.db.sim_tick().iter()
        .max_by_key(|t| t.tick_id)
        .ok_or("No tick found")?
        .tick_id;

    // Backpressure guard: skip inserting a new tick if the simulation worker
    // has not committed recent ticks. This prevents sim_tick from accumulating
    // faster than the worker can consume when it is slow or disconnected.
    //
    // Guard is only active once a worker has committed at least one tick
    // (last_committed_tick > 0).  Before that — clean deploy, hot-reload after a
    // schema migration that zero-initialises the field, or a fresh server with no
    // worker yet — we let ticks flow freely.  The sim_tick table is already bounded
    // by the SIM_TICK_RETAIN pruning below, so unbounded growth is not a concern.
    let last_committed = ctx.db.module_config().key().find(0)
        .map(|c| c.last_committed_tick)
        .unwrap_or(0);

    if last_committed > 0 {
        let backlog = current_max.saturating_sub(last_committed);
        if backlog > BACKLOG_LIMIT {
            // Self-healing: the worker that created these sim_tick rows crashed
            // before committing results. Delete the orphaned rows and reset
            // last_committed_tick to 0 (cold-start mode). The next worker that
            // connects will seed from MAX(sim_tick) normally, and its first
            // commit will bootstrap the sequence via the cold-start bypass in
            // commit_tick_results.
            let to_delete: Vec<u64> = ctx.db.sim_tick().iter()
                .filter(|t| t.tick_id > last_committed)
                .map(|t| t.tick_id)
                .collect();
            let count = to_delete.len();
            for id in to_delete {
                ctx.db.sim_tick().tick_id().delete(&id);
            }
            if let Some(mut cfg) = ctx.db.module_config().key().find(0) {
                cfg.last_committed_tick = 0;
                ctx.db.module_config().key().update(cfg);
            }
            current_max = last_committed;
            log::warn!(
                "tick_trigger: cleaned {count} orphaned sim_tick rows (last_committed was {last_committed}) — entering cold-start recovery"
            );
            // Fall through to insert the next tick normally.
        }
    }

    let next_tick_id = current_max + 1;

    ctx.db.sim_tick().insert(SimTick {
        tick_id: next_tick_id,
        timestamp_us: ctx.timestamp.to_micros_since_unix_epoch(),
    });

    // Prune old sim_tick rows to cap table size.
    // Keeps the last SIM_TICK_RETAIN rows so a restarting coordinator can still
    // seed last_processed_tick = MAX(sim_tick) without loading the full history.
    if next_tick_id > SIM_TICK_RETAIN {
        let cutoff = next_tick_id - SIM_TICK_RETAIN;
        let to_delete: Vec<u64> = ctx.db.sim_tick()
            .iter()
            .filter(|t| t.tick_id < cutoff)
            .map(|t| t.tick_id)
            .collect();
        for id in to_delete {
            ctx.db.sim_tick().tick_id().delete(&id);
        }
    }

    Ok(())
}

// ── World Clock ─────────────────────────────────────────────────────
// Tier 2 scheduled reducer (30s interval). Evaluates zone_counter
// thresholds, transitions world_phase, writes npc_goal directives.

#[reducer]
pub fn world_clock(ctx: &ReducerContext, _schedule: WorldClockSchedule) -> Result<(), String> {
    if ctx.sender() != ctx.identity() {
        return Err("world_clock may only be invoked by the scheduler".into());
    }

    // Tier 2 evaluation logic will be added as zone_counter and world_phase
    // content is populated. For now, this is a no-op placeholder that
    // demonstrates the scheduled reducer pattern.
    log::trace!("world_clock: tick");

    Ok(())
}

// ── Player Input ────────────────────────────────────────────────────
// Per spec: reducers validate + insert intent. Clients send intent only.
// Server validates all gameplay. Input must be sequenced.

#[reducer]
pub fn submit_intent(
    ctx: &ReducerContext,
    entity_id: u64,
    sequence_id: u64,
    action: IntentAction,
    client_observed_tick: u64,
) -> Result<(), String> {
    let caller = ctx.sender();

    // Verify client owns this entity
    let seq = ctx.db.client_sequence().client_identity().find(&caller)
        .ok_or("Client not registered")?;

    if seq.entity_id != entity_id {
        return Err("Client does not own this entity".into());
    }

    // Sequence validation: reject out-of-order / duplicate
    if sequence_id <= seq.last_processed_sequence {
        return Err(format!(
            "Stale sequence: got {}, last processed {}",
            sequence_id, seq.last_processed_sequence
        ));
    }

    // Update last processed sequence
    ctx.db.client_sequence().client_identity().update(ClientSequence {
        client_identity: caller,
        last_processed_sequence: sequence_id,
        entity_id: seq.entity_id,
    });

    // Rate limit: cap the number of pending (unprocessed) intents per entity.
    // At 20 Hz a queue depth of 5 = 250 ms of buffered input — enough for normal
    // gameplay but prevents a buggy or malicious client from flooding DB writes.
    const MAX_QUEUED_INTENTS: usize = 5;
    let queued = ctx.db.player_intent()
        .entity_id().filter(&entity_id)
        .count();
    if queued >= MAX_QUEUED_INTENTS {
        return Err("Intent queue full: reduce submission rate".into());
    }

    // Intents are always scheduled for the next simulation tick.
    let current_tick = ctx.db.sim_tick().iter()
        .max_by_key(|t| t.tick_id)
        .map(|t| t.tick_id)
        .unwrap_or(0);
    let target_tick = current_tick + 1;

    ctx.db.player_intent().insert(PlayerIntent {
        intent_id: 0, // auto_inc
        client_identity: caller,
        entity_id,
        sequence_id,
        target_tick,
        client_observed_tick,
        action,
    });

    Ok(())
}

// ── Entity Spawning ─────────────────────────────────────────────────
// Register a player entity for the connected client.

#[reducer]
pub fn spawn_player(ctx: &ReducerContext) -> Result<(), String> {
    let caller = ctx.sender();

    // A client can own only one spawned player entity.
    if ctx.db.client_sequence().client_identity().find(&caller).is_some() {
        return Err("Player already spawned".into());
    }

    let current_tick = ctx.db.sim_tick().iter()
        .max_by_key(|t| t.tick_id)
        .map(|t| t.tick_id)
        .unwrap_or(0);

    let entity = ctx.db.entity().insert(Entity {
        entity_id: 0, // auto_inc
        kind: EntityKind::Player,
        state: EntityState::Spawning,
        spawned_at_tick: current_tick,
        owner_identity: Some(caller),
    });

    let eid = entity.entity_id;

    ctx.db.entity_transform().insert(EntityTransform {
        entity_id: eid,
        pos_x: 0.0, pos_y: 1.0, pos_z: 0.0,
        rot_x: 0.0, rot_y: 0.0, rot_z: 0.0, rot_w: 1.0,
        vel_x: 0.0, vel_y: 0.0, vel_z: 0.0,
        angvel_x: 0.0, angvel_y: 0.0, angvel_z: 0.0,
        last_tick: current_tick,
    });

    ctx.db.entity_health().insert(EntityHealth {
        entity_id: eid,
        hp: 1000.0,
        max_hp: 1000.0,
    });

    ctx.db.entity_region().insert(EntityRegion {
        entity_id: eid,
        region_x: 0,
        region_z: 0,
        layer: 0,
    });

    ctx.db.client_sequence().insert(ClientSequence {
        client_identity: caller,
        last_processed_sequence: 0,
        entity_id: eid,
    });

    log::info!("Player spawned: entity_id={}, identity={:?}", eid, caller);
    Ok(())
}
// ── NPC Spawning ────────────────────────────────────────────────
// Spawn a test NPC at an explicit position. Admin or trusted-worker only.
// Used for smoke-testing the combat path (player Slash → NPC damage → death).

/// Internal helper: create an NPC entity with optional config.
/// Returns the entity_id of the spawned NPC.
fn spawn_npc_internal(
    ctx: &ReducerContext,
    kind: EntityKind,
    pos_x: f32,
    pos_y: f32,
    pos_z: f32,
    max_hp: f32,
    config: Option<NpcConfig>,
) -> u64 {
    let current_tick = ctx.db.sim_tick().iter()
        .max_by_key(|t| t.tick_id)
        .map(|t| t.tick_id)
        .unwrap_or(0);

    let entity = ctx.db.entity().insert(Entity {
        entity_id: 0,
        kind,
        state: EntityState::Spawning,
        spawned_at_tick: current_tick,
        owner_identity: None,
    });
    let eid = entity.entity_id;

    ctx.db.entity_transform().insert(EntityTransform {
        entity_id: eid,
        pos_x, pos_y, pos_z,
        rot_x: 0.0, rot_y: 0.0, rot_z: 0.0, rot_w: 1.0,
        vel_x: 0.0, vel_y: 0.0, vel_z: 0.0,
        angvel_x: 0.0, angvel_y: 0.0, angvel_z: 0.0,
        last_tick: current_tick,
    });

    ctx.db.entity_health().insert(EntityHealth {
        entity_id: eid,
        hp: max_hp,
        max_hp,
    });

    ctx.db.entity_region().insert(EntityRegion {
        entity_id: eid,
        region_x: (pos_x / 50.0).floor() as i32,
        region_z: (pos_z / 50.0).floor() as i32,
        layer: 0,
    });

    if let Some(mut cfg) = config {
        cfg.entity_id = eid;
        ctx.db.npc_config().insert(cfg);
    }

    eid
}

#[reducer]
pub fn spawn_npc(
    ctx: &ReducerContext,
    pos_x: f32,
    pos_y: f32,
    pos_z: f32,
    max_hp: f32,
) -> Result<(), String> {
    if !is_trusted_caller(ctx) && !is_module_admin(ctx) && !is_debug_caller(ctx) {
        return Err("spawn_npc may only be called by admin or a trusted worker".into());
    }

    let eid = spawn_npc_internal(ctx, EntityKind::Npc, pos_x, pos_y, pos_z, max_hp, None);
    log::info!("NPC spawned: entity_id={} pos=({},{},{}) max_hp={}", eid, pos_x, pos_y, pos_z, max_hp);
    Ok(())
}
// ── Commit Tick Results ─────────────────────────────────────────────
// Trusted-worker reducer: applies one authoritative simulation tick to DB rows.

/// Check if the caller is the module admin (the identity that published the module).
fn is_module_admin(ctx: &ReducerContext) -> bool {
    if ctx.sender() == ctx.identity() {
        return true;
    }
    ctx.db.module_config().key().find(0)
        .is_some_and(|cfg| ctx.sender() == cfg.admin)
}

/// Check if the caller is the module itself or a registered simulation worker.
fn is_trusted_caller(ctx: &ReducerContext) -> bool {
    if ctx.sender() == ctx.identity() {
        return true;
    }
    ctx.db.trusted_worker().worker_identity().find(&ctx.sender()).is_some()
}

/// When the `debug` feature is enabled, any authenticated caller is allowed
/// to invoke admin/debug reducers. In production builds this always returns false.
#[cfg(feature = "debug")]
fn is_debug_caller(_ctx: &ReducerContext) -> bool { true }
#[cfg(not(feature = "debug"))]
fn is_debug_caller(_ctx: &ReducerContext) -> bool { false }

#[reducer]
pub fn commit_tick_results(
    ctx: &ReducerContext,
    tick_id: u64,
    transforms: Vec<TransformUpdate>,
    health_updates: Vec<HealthUpdate>,
    combat_events: Vec<CombatEventInput>,
    world_events: Vec<WorldEventInput>,
    consumed_intent_ids: Vec<u64>,
    entity_state_updates: Vec<EntityStateUpdate>,
    region_updates: Vec<RegionUpdate>,
    buff_updates: Vec<BuffUpdate>,
    buff_cleared_entity_ids: Vec<u64>,
    threat_updates: Vec<ThreatUpdate>,
    threat_cleared_entity_ids: Vec<u64>,
    npc_state_updates: Vec<NpcStateUpdate>,
    director_spawns: Vec<DirectorSpawnInput>,
) -> Result<(), String> {
    // Accept only trusted worker identities (or module identity in internal calls).
    if !is_trusted_caller(ctx) {
        return Err("commit_tick_results may only be invoked by a trusted worker".into());
    }

    // Apply transform updates
    for t in transforms {
        if ctx.db.entity_transform().entity_id().find(&t.entity_id).is_none() {
            log::warn!(
                "commit_tick_results tick={}: no entity_transform row for entity_id={} — update skipped",
                tick_id, t.entity_id
            );
            continue;
        }
        ctx.db.entity_transform().entity_id().update(EntityTransform {
            entity_id: t.entity_id,
            pos_x: t.pos_x, pos_y: t.pos_y, pos_z: t.pos_z,
            rot_x: t.rot_x, rot_y: t.rot_y, rot_z: t.rot_z, rot_w: t.rot_w,
            vel_x: t.vel_x, vel_y: t.vel_y, vel_z: t.vel_z,
            angvel_x: t.angvel_x, angvel_y: t.angvel_y, angvel_z: t.angvel_z,
            last_tick: tick_id,
        });
    }

    // Apply health updates
    for h in health_updates {
        if ctx.db.entity_health().entity_id().find(&h.entity_id).is_none() {
            log::warn!(
                "commit_tick_results tick={}: no entity_health row for entity_id={} — update skipped",
                tick_id, h.entity_id
            );
            continue;
        }
        ctx.db.entity_health().entity_id().update(EntityHealth {
            entity_id: h.entity_id,
            hp: h.hp,
            max_hp: h.max_hp,
        });
    }

    // Insert combat events (use worker-provided event_sequence to preserve total ordering)
    for e in combat_events {
        ctx.db.combat_event().insert(CombatEvent {
            event_id: 0,
            tick_id,
            event_sequence: e.event_sequence,
            source_entity: e.source_entity,
            target_entity: e.target_entity,
            event_kind: e.event_kind,
        });
    }

    // Insert world events (preserve worker-provided event_sequence)
    for e in world_events {
        ctx.db.world_event().insert(WorldEvent {
            event_id: 0,
            tick_id,
            event_sequence: e.event_sequence,
            entity_id: e.entity_id,
            event_kind: e.event_kind,
        });
    }

    // Delete consumed intents
    for id in consumed_intent_ids {
        ctx.db.player_intent().intent_id().delete(&id);
    }

    // Apply entity state updates
    for u in entity_state_updates {
        if let Some(existing) = ctx.db.entity().entity_id().find(&u.entity_id) {
            ctx.db.entity().entity_id().update(Entity {
                entity_id: u.entity_id,
                kind: existing.kind,
                state: u.new_state,
                spawned_at_tick: existing.spawned_at_tick,
                owner_identity: existing.owner_identity,
            });

            // Create death state for player entities transitioning to DespawnPending.
            // This records the death position and starts the respawn timer.
            if u.new_state == EntityState::DespawnPending && existing.kind == EntityKind::Player {
                if ctx.db.death_state().entity_id().find(&u.entity_id).is_none() {
                    let (dx, dy, dz) = ctx.db.entity_transform()
                        .entity_id().find(&u.entity_id)
                        .map(|t| (t.pos_x, t.pos_y, t.pos_z))
                        .unwrap_or((0.0, 1.0, 0.0));
                    let layer = ctx.db.entity_region()
                        .entity_id().find(&u.entity_id)
                        .map(|r| r.layer)
                        .unwrap_or(0);
                    ctx.db.death_state().insert(DeathState {
                        entity_id: u.entity_id,
                        died_at_tick: tick_id,
                        respawn_at_tick: tick_id + RESPAWN_DELAY_TICKS,
                        killer_entity: None,
                        layer,
                        death_pos_x: dx,
                        death_pos_y: dy,
                        death_pos_z: dz,
                    });
                }
            }

            // Clean up companion rows for entities reaching terminal Removed state
            // so they disappear from nearby_transforms and other views.
            if u.new_state == EntityState::Removed {
                ctx.db.entity_transform().entity_id().delete(&u.entity_id);
                ctx.db.entity_region().entity_id().delete(&u.entity_id);
                ctx.db.entity_health().entity_id().delete(&u.entity_id);
                ctx.db.npc_state().entity_id().delete(&u.entity_id);
                ctx.db.npc_config().entity_id().delete(&u.entity_id);
                ctx.db.stealthed_entity().entity_id().delete(&u.entity_id);
                ctx.db.entity_team().entity_id().delete(&u.entity_id);
                ctx.db.boss_phase().boss_entity_id().delete(&u.entity_id);
                ctx.db.npc_goal().entity_id().delete(&u.entity_id);
                // Note: death_state is NOT deleted here — players need it for respawn.
                // Buffs and threat rows for Removed entities are already cleaned
                // up by the buff_cleared / threat_cleared sections below — the
                // worker explicitly adds Removed entity IDs to those lists.
                log::info!(
                    "commit_tick_results tick={}: entity {} removed — companion rows deleted",
                    tick_id, u.entity_id
                );
            }
        } else {
            log::warn!(
                "commit_tick_results tick={}: no entity row for entity_id={} — state update skipped",
                tick_id, u.entity_id
            );
        }
    }

    // Apply region updates
    for r in region_updates {
        ctx.db.entity_region().entity_id().update(EntityRegion {
            entity_id: r.entity_id,
            region_x: r.region_x,
            region_z: r.region_z,
            layer: r.layer,
        });
    }

    // Persist buffs: delete all rows for entities present in buff_cleared_entity_ids, then
    // re-insert. buff_cleared_entity_ids always includes every non-Removed entity (with an
    // empty vec for entities that currently have no buffs), so stale rows are
    // reliably cleared when all buffs on an entity expire in the same tick.
    {
        // buff_cleared_entity_ids drives deletes; buff_updates drives inserts.
        // Separating them ensures entities with zero buffs still clear stale DB rows.
        for entity_id in &buff_cleared_entity_ids {
            ctx.db.active_buff().entity_id().delete(entity_id);
        }
        for b in buff_updates {
            ctx.db.active_buff().insert(ActiveBuff {
                buff_instance_id: 0, // auto_inc
                entity_id: b.entity_id,
                buff_id: b.buff_id,
                source_entity: b.source_entity,
                stacks: b.stacks,
                expires_at_tick: b.expires_at_tick,
                mod_damage_out_pct: b.mod_damage_out_pct,
                mod_damage_in_pct: b.mod_damage_in_pct,
                mod_cooldown_reduce_pct: b.mod_cooldown_reduce_pct,
                mod_speed_pct: b.mod_speed_pct,
                mod_ai_override_kind: b.mod_ai_override_kind,
                mod_ai_override_target: b.mod_ai_override_target,
                mod_root: b.mod_root,
                mod_stealth: b.mod_stealth,
            });
        }

        // Derive stealthed_entity from committed buffs.
        // For each entity whose buffs were refreshed this tick, check if any
        // buff has mod_stealth=true. If so, look up the entity's team and
        // upsert a stealthed_entity row; otherwise delete any existing row.
        for &entity_id in &buff_cleared_entity_ids {
            let is_stealthed = ctx.db.active_buff().entity_id().filter(&entity_id)
                .any(|b| b.mod_stealth == Some(true));
            if is_stealthed {
                let team_id = ctx.db.entity_team().entity_id().find(&entity_id)
                    .map(|t| t.team_id)
                    .unwrap_or(0);
                if ctx.db.stealthed_entity().entity_id().find(&entity_id).is_some() {
                    ctx.db.stealthed_entity().entity_id().update(StealthedEntity {
                        entity_id,
                        team_id,
                    });
                } else {
                    ctx.db.stealthed_entity().insert(StealthedEntity {
                        entity_id,
                        team_id,
                    });
                }
            } else {
                ctx.db.stealthed_entity().entity_id().delete(&entity_id);
            }
        }
    }

    // Persist threat: delete all rows for NPCs present in threat_cleared_entity_ids, then re-insert.
    // threat_cleared_entity_ids always includes every non-Removed NPC/Boss entity so stale rows
    // are reliably cleared when all threat decays to zero in a single tick.
    {
        // threat_cleared_entity_ids drives deletes; threat_updates drives inserts.
        for npc_entity in &threat_cleared_entity_ids {
            ctx.db.threat_entry().npc_entity().delete(npc_entity);
        }
        for t in threat_updates {
            ctx.db.threat_entry().insert(ThreatEntry {
                threat_id: 0, // auto_inc
                npc_entity: t.npc_entity,
                source_entity: t.source_entity,
                threat: t.threat,
            });
        }
    }

    // Persist NPC state: upsert by entity PK (1:1 per NPC entity).
    for n in npc_state_updates {
        if ctx.db.npc_state().entity_id().find(&n.entity_id).is_some() {
            ctx.db.npc_state().entity_id().update(NpcState {
                entity_id: n.entity_id,
                ai_state: n.ai_state,
                target_entity: n.target_entity,
            });
        } else {
            ctx.db.npc_state().insert(NpcState {
                entity_id: n.entity_id,
                ai_state: n.ai_state,
                target_entity: n.target_entity,
            });
        }
    }

    // Persist Director spawns: create entity + companion rows for each spawn.
    // Uses auto_inc (entity_id: 0) just like player/NPC spawns — the DB
    // assigns unique IDs. Entity kind is tracked via EntityKind, not ID range.
    for s in director_spawns {
        let entity = ctx.db.entity().insert(Entity {
            entity_id: 0, // auto_inc
            kind: s.kind,
            state: EntityState::Spawning,
            spawned_at_tick: tick_id,
            owner_identity: None,
        });
        let eid = entity.entity_id;

        ctx.db.entity_transform().insert(EntityTransform {
            entity_id: eid,
            pos_x: s.pos_x, pos_y: s.pos_y, pos_z: s.pos_z,
            rot_x: 0.0, rot_y: 0.0, rot_z: 0.0, rot_w: 1.0,
            vel_x: 0.0, vel_y: 0.0, vel_z: 0.0,
            angvel_x: 0.0, angvel_y: 0.0, angvel_z: 0.0,
            last_tick: tick_id,
        });

        ctx.db.entity_health().insert(EntityHealth {
            entity_id: eid,
            hp: s.max_hp,
            max_hp: s.max_hp,
        });

        ctx.db.entity_region().insert(EntityRegion {
            entity_id: eid,
            region_x: (s.pos_x / 50.0).floor() as i32,
            region_z: (s.pos_z / 50.0).floor() as i32,
            layer: 0,
        });
    }

    // Advance the last_committed_tick cursor used by tick_trigger's backpressure guard.
    // Strict sequence enforcement: the worker must commit ticks exactly in order.
    // If a tick was lost or skipped, the worker should have retried or crashed;
    // accepting a gap here would silently drop DB effects for the missing tick.
    //
    // Cold-start rule: when last_committed_tick == 0, no worker has committed yet
    // (or the module was freshly deployed). Accept any tick_id to bootstrap the
    // sequence. This mirrors tick_trigger's cold-start bypass of backpressure.
    if let Some(mut cfg) = ctx.db.module_config().key().find(0) {
        if cfg.last_committed_tick == 0 {
            // Cold start — accept whatever tick the worker sends to bootstrap.
            cfg.last_committed_tick = tick_id;
            ctx.db.module_config().key().update(cfg);
        } else {
            let expected = cfg.last_committed_tick + 1;
            if tick_id == expected {
                cfg.last_committed_tick = tick_id;
                ctx.db.module_config().key().update(cfg);
            } else if tick_id <= cfg.last_committed_tick {
                // Duplicate / replay — harmless, ignore.
                log::warn!(
                    "commit_tick_results: tick_id={} already committed (last={}), ignoring cursor advance",
                    tick_id, cfg.last_committed_tick
                );
            } else {
                // Gap detected — reject so the worker sees a reducer error and retries.
                return Err(format!(
                    "commit_tick_results: tick_id={} skips expected {} — gap rejected",
                    tick_id, expected
                ));
            }
        }
    }

    Ok(())
}

// ── Commit reducer input types ──────────────────────────────────────
// These are the wire types the simulation worker sends when calling
// commit_tick_results. They use SpacetimeType so they are serializable
// across the SpacetimeDB boundary.

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct TransformUpdate {
    pub entity_id: u64,
    pub pos_x: f32, pub pos_y: f32, pub pos_z: f32,
    pub rot_x: f32, pub rot_y: f32, pub rot_z: f32, pub rot_w: f32,
    pub vel_x: f32, pub vel_y: f32, pub vel_z: f32,
    pub angvel_x: f32, pub angvel_y: f32, pub angvel_z: f32,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct HealthUpdate {
    pub entity_id: u64,
    pub hp: f32,
    pub max_hp: f32,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct CombatEventInput {
    pub source_entity: u64,
    pub target_entity: u64,
    pub event_sequence: u32,
    pub event_kind: CombatEventKind,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct WorldEventInput {
    pub entity_id: u64,
    pub event_sequence: u32,
    pub event_kind: WorldEventKind,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct EntityStateUpdate {
    pub entity_id: u64,
    pub new_state: EntityState,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct RegionUpdate {
    pub entity_id: u64,
    pub region_x: i32,
    pub region_z: i32,
    pub layer: u32,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct BuffUpdate {
    pub entity_id: u64,
    pub buff_id: u32,
    pub source_entity: u64,
    pub stacks: u32,
    pub expires_at_tick: Option<u64>,
    pub mod_damage_out_pct: Option<f32>,
    pub mod_damage_in_pct: Option<f32>,
    pub mod_cooldown_reduce_pct: Option<f32>,
    pub mod_speed_pct: Option<f32>,
    pub mod_ai_override_kind: Option<u8>,
    pub mod_ai_override_target: Option<u64>,
    pub mod_root: Option<bool>,
    pub mod_stealth: Option<bool>,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct ThreatUpdate {
    pub npc_entity: u64,
    pub source_entity: u64,
    pub threat: f32,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct NpcStateUpdate {
    pub entity_id: u64,
    pub ai_state: NpcAiState,
    pub target_entity: Option<u64>,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct DirectorSpawnInput {
    pub kind: EntityKind,
    pub max_hp: f32,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
}

// ── Worker Registration ─────────────────────────────────────────────
// Register a trusted worker identity that may call commit/cleanup reducers.
// Authorization is module-admin based (`is_module_admin`), not module-identity-only.

#[reducer]
pub fn register_worker(ctx: &ReducerContext, worker_identity: spacetimedb::Identity) -> Result<(), String> {
    if !is_module_admin(ctx) {
        return Err("register_worker may only be invoked by the module admin".into());
    }
    if ctx.db.trusted_worker().worker_identity().find(&worker_identity).is_some() {
        return Err("Worker already registered".into());
    }
    ctx.db.trusted_worker().insert(TrustedWorker { worker_identity });
    log::info!("Registered trusted worker: {:?}", worker_identity);
    Ok(())
}

// ── Clear Events ────────────────────────────────────────────────────
// Delete old event rows. Called by the coordinator after clients
// have had time to receive them. Per spec: event tables hold transient
// rows that are broadcast then deleted.

#[reducer]
pub fn clear_events(ctx: &ReducerContext, before_tick: u64) -> Result<(), String> {
    // Per spec: only the simulation worker / scheduler may delete event rows.
    if !is_trusted_caller(ctx) {
        return Err("clear_events may only be invoked by a trusted worker".into());
    }

    // Delete combat events from old ticks (range delete via btree index)
    ctx.db.combat_event().tick_id().delete(..before_tick);

    // Delete world events from old ticks (range delete via btree index)
    ctx.db.world_event().tick_id().delete(..before_tick);

    Ok(())
}

// ── Economy Reducers ────────────────────────────────────────────────
// These operate outside the tick pipeline. Clients call them directly
// for instant-feel inventory management. The simulation worker never
// calls these — it only observes player_equipment changes via
// subscription callbacks to trigger stat recalculation.
//
// All economy reducers validate caller ownership via client_sequence
// before mutating any rows.

/// Equip an item from inventory into an equipment slot.
///
/// If the equipment slot is already occupied, the existing item is
/// moved back to the inventory slot the new item came from (swap).
#[reducer]
pub fn equip_item(
    ctx: &ReducerContext,
    entity_id: u64,
    inventory_slot: u32,
    target_slot: EquipmentSlot,
) -> Result<(), String> {
    let caller = ctx.sender();

    // Verify ownership
    let seq = ctx.db.client_sequence().client_identity().find(&caller)
        .ok_or("Client not registered")?;
    if seq.entity_id != entity_id {
        return Err("Client does not own this entity".into());
    }

    // Find the inventory item
    let inv_item = ctx.db.player_inventory().owner_entity().filter(&entity_id)
        .find(|r| r.slot_index == inventory_slot)
        .ok_or("No item in that inventory slot")?;

    let equipping_item_id = inv_item.item_id;
    let inv_row_id = inv_item.row_id;

    // Check if equipment slot is already occupied
    let existing_equip = ctx.db.player_equipment().owner_entity().filter(&entity_id)
        .find(|r| r.slot == target_slot);

    if let Some(old_equip) = existing_equip {
        // Swap: move old equipment into the inventory slot being vacated
        let old_row_id = old_equip.row_id;
        let old_item_id = old_equip.item_id;

        // Update inventory slot with the old equipment item
        ctx.db.player_inventory().row_id().update(PlayerInventory {
            row_id: inv_row_id,
            owner_entity: entity_id,
            slot_index: inventory_slot,
            item_id: old_item_id,
            quantity: 1,
        });

        // Update equipment slot with the new item
        ctx.db.player_equipment().row_id().update(PlayerEquipment {
            row_id: old_row_id,
            owner_entity: entity_id,
            slot: target_slot,
            item_id: equipping_item_id,
        });
    } else {
        // No existing equipment — remove from inventory, insert into equipment
        ctx.db.player_inventory().row_id().delete(&inv_row_id);
        ctx.db.player_equipment().insert(PlayerEquipment {
            row_id: 0, // auto_inc
            owner_entity: entity_id,
            slot: target_slot,
            item_id: equipping_item_id,
        });
    }

    log::info!(
        "equip_item: entity={} item={} slot={:?}",
        entity_id, equipping_item_id, target_slot
    );
    Ok(())
}

/// Unequip an item from an equipment slot into a specific inventory slot.
#[reducer]
pub fn unequip_item(
    ctx: &ReducerContext,
    entity_id: u64,
    equipment_slot: EquipmentSlot,
    target_inventory_slot: u32,
) -> Result<(), String> {
    let caller = ctx.sender();

    let seq = ctx.db.client_sequence().client_identity().find(&caller)
        .ok_or("Client not registered")?;
    if seq.entity_id != entity_id {
        return Err("Client does not own this entity".into());
    }

    // Find the equipped item
    let equip = ctx.db.player_equipment().owner_entity().filter(&entity_id)
        .find(|r| r.slot == equipment_slot)
        .ok_or("Nothing equipped in that slot")?;

    let item_id = equip.item_id;
    let equip_row_id = equip.row_id;

    // Verify target inventory slot is empty
    let slot_occupied = ctx.db.player_inventory().owner_entity().filter(&entity_id)
        .any(|r| r.slot_index == target_inventory_slot);
    if slot_occupied {
        return Err("Target inventory slot is occupied".into());
    }

    // Remove equipment, insert into inventory
    ctx.db.player_equipment().row_id().delete(&equip_row_id);
    ctx.db.player_inventory().insert(PlayerInventory {
        row_id: 0,
        owner_entity: entity_id,
        slot_index: target_inventory_slot,
        item_id,
        quantity: 1,
    });

    log::info!(
        "unequip_item: entity={} item={} slot={:?} -> inv_slot={}",
        entity_id, item_id, equipment_slot, target_inventory_slot
    );
    Ok(())
}

/// Swap items between two inventory slots (or move if one is empty).
#[reducer]
pub fn swap_item(
    ctx: &ReducerContext,
    entity_id: u64,
    slot_a: u32,
    slot_b: u32,
) -> Result<(), String> {
    let caller = ctx.sender();

    let seq = ctx.db.client_sequence().client_identity().find(&caller)
        .ok_or("Client not registered")?;
    if seq.entity_id != entity_id {
        return Err("Client does not own this entity".into());
    }

    if slot_a == slot_b {
        return Ok(()); // No-op
    }

    let item_a = ctx.db.player_inventory().owner_entity().filter(&entity_id)
        .find(|r| r.slot_index == slot_a);
    let item_b = ctx.db.player_inventory().owner_entity().filter(&entity_id)
        .find(|r| r.slot_index == slot_b);

    match (item_a, item_b) {
        (Some(a), Some(b)) => {
            // Swap both entries
            let (a_id, a_row, a_item, a_qty) = (a.row_id, a.slot_index, a.item_id, a.quantity);
            let (b_id, b_row, b_item, b_qty) = (b.row_id, b.slot_index, b.item_id, b.quantity);
            ctx.db.player_inventory().row_id().update(PlayerInventory {
                row_id: a_id, owner_entity: entity_id,
                slot_index: a_row, item_id: b_item, quantity: b_qty,
            });
            ctx.db.player_inventory().row_id().update(PlayerInventory {
                row_id: b_id, owner_entity: entity_id,
                slot_index: b_row, item_id: a_item, quantity: a_qty,
            });
        }
        (Some(a), None) => {
            // Move A to slot B
            ctx.db.player_inventory().row_id().update(PlayerInventory {
                row_id: a.row_id, owner_entity: entity_id,
                slot_index: slot_b, item_id: a.item_id, quantity: a.quantity,
            });
        }
        (None, Some(b)) => {
            // Move B to slot A
            ctx.db.player_inventory().row_id().update(PlayerInventory {
                row_id: b.row_id, owner_entity: entity_id,
                slot_index: slot_a, item_id: b.item_id, quantity: b.quantity,
            });
        }
        (None, None) => {
            // Both empty — no-op
        }
    }

    Ok(())
}

/// Grant an item to a player's inventory. Trusted-worker only.
///
/// Called by the simulation worker when a player loots a pickup or
/// receives a quest reward. The simulation determines *what* drops;
/// this reducer persists the result.
#[reducer]
pub fn loot_item(
    ctx: &ReducerContext,
    entity_id: u64,
    target_slot: u32,
    item_id: u32,
    quantity: u32,
) -> Result<(), String> {
    if !is_trusted_caller(ctx) {
        return Err("loot_item may only be invoked by a trusted worker".into());
    }

    // Verify entity exists
    if ctx.db.entity().entity_id().find(&entity_id).is_none() {
        return Err("Entity does not exist".into());
    }

    // Verify slot is empty
    let slot_occupied = ctx.db.player_inventory().owner_entity().filter(&entity_id)
        .any(|r| r.slot_index == target_slot);
    if slot_occupied {
        return Err("Target inventory slot is occupied".into());
    }

    ctx.db.player_inventory().insert(PlayerInventory {
        row_id: 0,
        owner_entity: entity_id,
        slot_index: target_slot,
        item_id,
        quantity,
    });

    log::info!("loot_item: entity={} item={} qty={} slot={}", entity_id, item_id, quantity, target_slot);
    Ok(())
}

/// Accept a pending trade. Stub — full trade system not yet implemented.
#[reducer]
pub fn trade_accept(ctx: &ReducerContext, _trade_id: u64) -> Result<(), String> {
    let _caller = ctx.sender();
    Err("Trading system not yet implemented".into())
}

// ── Respawn ─────────────────────────────────────────────────────────
// Re-spawn a dead player at the nearest respawn point after a delay.
// Death state is created automatically by commit_tick_results when
// a player entity transitions to DespawnPending.

/// Find the nearest respawn point on the given layer. Falls back to origin.
fn find_nearest_respawn_point(
    ctx: &ReducerContext,
    layer: u32,
    death_pos: Option<(f32, f32, f32)>,
) -> (f32, f32, f32) {
    let points: Vec<_> = ctx.db.respawn_point().layer().filter(&layer).collect();
    if points.is_empty() {
        return (0.0, 1.0, 0.0);
    }
    if let Some((dx, _, dz)) = death_pos {
        points.iter()
            .min_by(|a, b| {
                let da = (a.pos_x - dx).powi(2) + (a.pos_z - dz).powi(2);
                let db = (b.pos_x - dx).powi(2) + (b.pos_z - dz).powi(2);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|p| (p.pos_x, p.pos_y, p.pos_z))
            .unwrap_or((0.0, 1.0, 0.0))
    } else {
        let p = &points[0];
        (p.pos_x, p.pos_y, p.pos_z)
    }
}

#[reducer]
pub fn respawn_player(ctx: &ReducerContext) -> Result<(), String> {
    let caller = ctx.sender();

    let seq = ctx.db.client_sequence().client_identity().find(&caller)
        .ok_or("Not registered — call spawn_player first")?;

    let entity_id = seq.entity_id;

    let entity = ctx.db.entity().entity_id().find(&entity_id)
        .ok_or("Entity row not found")?;

    // Only allow respawn from terminal states.
    if !matches!(entity.state, EntityState::DespawnPending | EntityState::Removed) {
        return Err(format!("Entity is {:?} — not dead", entity.state));
    }

    let current_tick = ctx.db.sim_tick().iter()
        .max_by_key(|t| t.tick_id)
        .map(|t| t.tick_id)
        .unwrap_or(0);

    // Check respawn timer via death_state (created by commit_tick_results).
    let death = ctx.db.death_state().entity_id().find(&entity_id);
    if let Some(ref ds) = death {
        if current_tick < ds.respawn_at_tick {
            let remaining = ds.respawn_at_tick - current_tick;
            return Err(format!("Cannot respawn yet — {} ticks remaining", remaining));
        }
    }

    // Find nearest respawn point on the player's death layer.
    let layer = death.as_ref().map(|d| d.layer).unwrap_or(0);
    let death_pos = death.as_ref().map(|d| (d.death_pos_x, d.death_pos_y, d.death_pos_z));
    let respawn_pos = find_nearest_respawn_point(ctx, layer, death_pos);

    // Restore entity state.
    ctx.db.entity().entity_id().update(Entity {
        entity_id,
        kind: entity.kind,
        state: EntityState::Spawning,
        spawned_at_tick: current_tick,
        owner_identity: entity.owner_identity,
    });

    // Re-create companion rows (deleted on Removed transition).
    if ctx.db.entity_transform().entity_id().find(&entity_id).is_none() {
        ctx.db.entity_transform().insert(EntityTransform {
            entity_id,
            pos_x: respawn_pos.0, pos_y: respawn_pos.1, pos_z: respawn_pos.2,
            rot_x: 0.0, rot_y: 0.0, rot_z: 0.0, rot_w: 1.0,
            vel_x: 0.0, vel_y: 0.0, vel_z: 0.0,
            angvel_x: 0.0, angvel_y: 0.0, angvel_z: 0.0,
            last_tick: current_tick,
        });
    }

    if ctx.db.entity_health().entity_id().find(&entity_id).is_none() {
        ctx.db.entity_health().insert(EntityHealth {
            entity_id,
            hp: 1000.0,
            max_hp: 1000.0,
        });
    } else {
        ctx.db.entity_health().entity_id().update(EntityHealth {
            entity_id,
            hp: 1000.0,
            max_hp: 1000.0,
        });
    }

    let region_x = (respawn_pos.0 / 50.0).floor() as i32;
    let region_z = (respawn_pos.2 / 50.0).floor() as i32;
    if ctx.db.entity_region().entity_id().find(&entity_id).is_none() {
        ctx.db.entity_region().insert(EntityRegion {
            entity_id,
            region_x,
            region_z,
            layer,
        });
    }

    // Clean up death state.
    ctx.db.death_state().entity_id().delete(&entity_id);

    log::info!(
        "Player respawned: entity_id={}, identity={:?}, pos=({},{},{})",
        entity_id, caller, respawn_pos.0, respawn_pos.1, respawn_pos.2
    );
    Ok(())
}

// ── Party System ────────────────────────────────────────────────────
// CRUD reducers for party management. Worker subscribes for team
// awareness. Required for dungeon entry (Phase B).

#[reducer]
pub fn create_party(ctx: &ReducerContext) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx.db.client_sequence().client_identity().find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    if ctx.db.party_member().entity_id().filter(&entity_id).next().is_some() {
        return Err("Already in a party".into());
    }

    let party = ctx.db.party().insert(Party {
        party_id: 0,
        leader_entity: entity_id,
        max_members: 5,
        created_at: ctx.timestamp.to_micros_since_unix_epoch(),
    });

    ctx.db.party_member().insert(PartyMember {
        member_id: 0,
        party_id: party.party_id,
        entity_id,
    });

    log::info!("Party created: party_id={}, leader={}", party.party_id, entity_id);
    Ok(())
}

#[reducer]
pub fn invite_to_party(ctx: &ReducerContext, target_entity_id: u64) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx.db.client_sequence().client_identity().find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    let membership = ctx.db.party_member().entity_id().filter(&entity_id)
        .next()
        .ok_or("Not in a party")?;
    let party = ctx.db.party().party_id().find(&membership.party_id)
        .ok_or("Party not found")?;
    if party.leader_entity != entity_id {
        return Err("Only the leader can invite".into());
    }

    if ctx.db.entity().entity_id().find(&target_entity_id).is_none() {
        return Err("Target entity does not exist".into());
    }
    if ctx.db.party_member().entity_id().filter(&target_entity_id).next().is_some() {
        return Err("Target is already in a party".into());
    }
    if ctx.db.party_invite().invitee_entity().filter(&target_entity_id)
        .any(|i| i.party_id == party.party_id)
    {
        return Err("Invite already pending".into());
    }

    ctx.db.party_invite().insert(PartyInvite {
        invite_id: 0,
        party_id: party.party_id,
        inviter_entity: entity_id,
        invitee_entity: target_entity_id,
        expires_at: ctx.timestamp.to_micros_since_unix_epoch() + INVITE_EXPIRE_MICROS,
    });

    log::info!("Party invite: {} invited {} to party {}", entity_id, target_entity_id, party.party_id);
    Ok(())
}

#[reducer]
pub fn accept_party_invite(ctx: &ReducerContext, invite_id: u64) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx.db.client_sequence().client_identity().find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    let invite = ctx.db.party_invite().invite_id().find(&invite_id)
        .ok_or("Invite not found")?;
    if invite.invitee_entity != entity_id {
        return Err("This invite is not for you".into());
    }
    if invite.expires_at < ctx.timestamp.to_micros_since_unix_epoch() {
        ctx.db.party_invite().invite_id().delete(&invite_id);
        return Err("Invite has expired".into());
    }
    if ctx.db.party_member().entity_id().filter(&entity_id).next().is_some() {
        ctx.db.party_invite().invite_id().delete(&invite_id);
        return Err("Already in a party".into());
    }

    let party = ctx.db.party().party_id().find(&invite.party_id)
        .ok_or("Party no longer exists")?;
    let member_count = ctx.db.party_member().party_id().filter(&invite.party_id).count() as u32;
    if member_count >= party.max_members {
        ctx.db.party_invite().invite_id().delete(&invite_id);
        return Err("Party is full".into());
    }

    ctx.db.party_invite().invite_id().delete(&invite_id);
    ctx.db.party_member().insert(PartyMember {
        member_id: 0,
        party_id: party.party_id,
        entity_id,
    });

    log::info!("Party join: entity {} joined party {}", entity_id, party.party_id);
    Ok(())
}

#[reducer]
pub fn decline_party_invite(ctx: &ReducerContext, invite_id: u64) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx.db.client_sequence().client_identity().find(&caller)
        .ok_or("Not registered")?;

    let invite = ctx.db.party_invite().invite_id().find(&invite_id)
        .ok_or("Invite not found")?;
    if invite.invitee_entity != seq.entity_id {
        return Err("This invite is not for you".into());
    }

    ctx.db.party_invite().invite_id().delete(&invite_id);
    Ok(())
}

#[reducer]
pub fn leave_party(ctx: &ReducerContext) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx.db.client_sequence().client_identity().find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    let membership = ctx.db.party_member().entity_id().filter(&entity_id)
        .next()
        .ok_or("Not in a party")?;
    let party_id = membership.party_id;

    ctx.db.party_member().entity_id().delete(&entity_id);

    let remaining: Vec<_> = ctx.db.party_member().party_id().filter(&party_id).collect();
    if remaining.is_empty() {
        ctx.db.party().party_id().delete(&party_id);
        ctx.db.party_invite().party_id().delete(&party_id);
    } else if let Some(party) = ctx.db.party().party_id().find(&party_id) {
        if party.leader_entity == entity_id {
            let new_leader = remaining[0].entity_id;
            ctx.db.party().party_id().update(Party {
                party_id,
                leader_entity: new_leader,
                max_members: party.max_members,
                created_at: party.created_at,
            });
            log::info!("Party {}: leader left, promoted entity {}", party_id, new_leader);
        }
    }

    log::info!("Party leave: entity {} left party {}", entity_id, party_id);
    Ok(())
}

#[reducer]
pub fn kick_from_party(ctx: &ReducerContext, target_entity_id: u64) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx.db.client_sequence().client_identity().find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    let membership = ctx.db.party_member().entity_id().filter(&entity_id)
        .next()
        .ok_or("Not in a party")?;
    let party = ctx.db.party().party_id().find(&membership.party_id)
        .ok_or("Party not found")?;
    if party.leader_entity != entity_id {
        return Err("Only the leader can kick".into());
    }
    if target_entity_id == entity_id {
        return Err("Cannot kick yourself — use leave_party".into());
    }

    let target = ctx.db.party_member().entity_id().filter(&target_entity_id)
        .find(|m| m.party_id == party.party_id)
        .ok_or("Target is not in your party")?;

    ctx.db.party_member().member_id().delete(&target.member_id);

    log::info!("Party kick: {} kicked {} from party {}", entity_id, target_entity_id, party.party_id);
    Ok(())
}

#[reducer]
pub fn disband_party(ctx: &ReducerContext) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx.db.client_sequence().client_identity().find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    let membership = ctx.db.party_member().entity_id().filter(&entity_id)
        .next()
        .ok_or("Not in a party")?;
    let party = ctx.db.party().party_id().find(&membership.party_id)
        .ok_or("Party not found")?;
    if party.leader_entity != entity_id {
        return Err("Only the leader can disband".into());
    }

    let party_id = party.party_id;
    ctx.db.party_member().party_id().delete(&party_id);
    ctx.db.party_invite().party_id().delete(&party_id);
    ctx.db.party().party_id().delete(&party_id);

    log::info!("Party disbanded: party_id={} by leader={}", party_id, entity_id);
    Ok(())
}

// ── World Management ────────────────────────────────────────────────
// Admin reducers for respawn points and trusted-worker reducers for
// boss phase / zone counter updates (ADR-0002 Tier 1).

#[reducer]
pub fn add_respawn_point(
    ctx: &ReducerContext,
    layer: u32,
    pos_x: f32,
    pos_y: f32,
    pos_z: f32,
    name: String,
) -> Result<(), String> {
    if !is_module_admin(ctx) && !is_debug_caller(ctx) {
        return Err("add_respawn_point: admin only".into());
    }
    let point = ctx.db.respawn_point().insert(RespawnPoint {
        point_id: 0,
        layer,
        region_x: (pos_x / 50.0).floor() as i32,
        region_z: (pos_z / 50.0).floor() as i32,
        pos_x,
        pos_y,
        pos_z,
        name: name.clone(),
    });
    log::info!(
        "Respawn point added: id={} name={} layer={} pos=({},{},{})",
        point.point_id, name, layer, pos_x, pos_y, pos_z
    );
    Ok(())
}

#[reducer]
pub fn remove_respawn_point(ctx: &ReducerContext, point_id: u64) -> Result<(), String> {
    if !is_module_admin(ctx) && !is_debug_caller(ctx) {
        return Err("remove_respawn_point: admin only".into());
    }
    if ctx.db.respawn_point().point_id().find(&point_id).is_none() {
        return Err("Respawn point not found".into());
    }
    ctx.db.respawn_point().point_id().delete(&point_id);
    log::info!("Respawn point removed: id={}", point_id);
    Ok(())
}

/// Update or create a boss phase entry. Trusted-worker only.
#[reducer]
pub fn commit_boss_phase(
    ctx: &ReducerContext,
    boss_entity_id: u64,
    phase: u32,
    entered_at_tick: u64,
) -> Result<(), String> {
    if !is_trusted_caller(ctx) {
        return Err("commit_boss_phase: trusted worker only".into());
    }
    if ctx.db.boss_phase().boss_entity_id().find(&boss_entity_id).is_some() {
        ctx.db.boss_phase().boss_entity_id().update(BossPhase {
            boss_entity_id,
            phase,
            entered_at_tick,
        });
    } else {
        ctx.db.boss_phase().insert(BossPhase {
            boss_entity_id,
            phase,
            entered_at_tick,
        });
    }
    log::info!("Boss phase: entity={} phase={} tick={}", boss_entity_id, phase, entered_at_tick);
    Ok(())
}

/// Increment a zone counter by delta. Creates the counter if it doesn't exist.
/// Trusted-worker only.
#[reducer]
pub fn increment_zone_counter(
    ctx: &ReducerContext,
    layer: u32,
    region_x: i32,
    region_z: i32,
    counter_name: String,
    delta: f64,
) -> Result<(), String> {
    if !is_trusted_caller(ctx) {
        return Err("increment_zone_counter: trusted worker only".into());
    }

    let existing = ctx.db.zone_counter().by_zone()
        .filter((layer, region_x, region_z..=region_z))
        .find(|c| c.counter_name == counter_name);

    if let Some(counter) = existing {
        let new_value = counter.value + delta;
        ctx.db.zone_counter().counter_id().update(ZoneCounter {
            counter_id: counter.counter_id,
            layer,
            region_x,
            region_z,
            counter_name,
            value: new_value,
        });
    } else {
        ctx.db.zone_counter().insert(ZoneCounter {
            counter_id: 0,
            layer,
            region_x,
            region_z,
            counter_name,
            value: delta,
        });
    }

    Ok(())
}

// ── Debug Reducers ──────────────────────────────────────────────────
// Admin-only test utilities for skill/combat development.
// Gated behind the `debug` compile-time feature.
// When building for production, omit the feature to strip these reducers
// from the published WASM module entirely.

#[cfg(feature = "debug")]
mod debug_reducers {
    use super::*;

    /// Set an entity's HP and max_hp directly. Useful for testing death
    /// thresholds, heal-over-time buffs, and low-HP AI flee behavior.
    #[reducer]
    pub fn debug_set_hp(
        ctx: &ReducerContext,
        entity_id: u64,
        hp: f32,
        max_hp: f32,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_set_hp: admin only".into());
        }
        if ctx.db.entity_health().entity_id().find(&entity_id).is_none() {
            return Err(format!("No entity_health row for entity {entity_id}"));
        }
        ctx.db.entity_health().entity_id().update(EntityHealth {
            entity_id,
            hp,
            max_hp,
        });
        log::info!("debug_set_hp: entity={entity_id} hp={hp} max_hp={max_hp}");
        Ok(())
    }

    /// Apply a buff directly to an entity, bypassing the ability pipeline.
    /// Specify buff_id, stacks, and optional expires_at_tick. Modifier
    /// fields default to None (no effect) — for testing stat effects,
    /// set the relevant modifier.
    #[reducer]
    pub fn debug_apply_buff(
        ctx: &ReducerContext,
        entity_id: u64,
        buff_id: u32,
        stacks: u32,
        duration_ticks: u32,
        mod_damage_out_pct: Option<f32>,
        mod_damage_in_pct: Option<f32>,
        mod_speed_pct: Option<f32>,
        mod_root: Option<bool>,
        mod_stealth: Option<bool>,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_apply_buff: admin only".into());
        }
        if ctx.db.entity().entity_id().find(&entity_id).is_none() {
            return Err(format!("Entity {entity_id} does not exist"));
        }

        let current_tick = ctx.db.sim_tick().iter()
            .max_by_key(|t| t.tick_id)
            .map(|t| t.tick_id)
            .unwrap_or(0);

        let expires_at_tick = if duration_ticks > 0 {
            Some(current_tick + duration_ticks as u64)
        } else {
            None // permanent
        };

        ctx.db.active_buff().insert(ActiveBuff {
            buff_instance_id: 0, // auto_inc
            entity_id,
            buff_id,
            source_entity: entity_id, // self-applied
            stacks,
            expires_at_tick,
            mod_damage_out_pct,
            mod_damage_in_pct,
            mod_cooldown_reduce_pct: None,
            mod_speed_pct,
            mod_ai_override_kind: None,
            mod_ai_override_target: None,
            mod_root,
            mod_stealth,
        });

        // Derive stealthed_entity so the view filter works immediately.
        if mod_stealth == Some(true) {
            let team_id = ctx.db.entity_team().entity_id().find(&entity_id)
                .map(|t| t.team_id)
                .unwrap_or(0);
            if ctx.db.stealthed_entity().entity_id().find(&entity_id).is_some() {
                ctx.db.stealthed_entity().entity_id().update(StealthedEntity {
                    entity_id,
                    team_id,
                });
            } else {
                ctx.db.stealthed_entity().insert(StealthedEntity {
                    entity_id,
                    team_id,
                });
            }
        }

        log::info!(
            "debug_apply_buff: entity={entity_id} buff={buff_id} stacks={stacks} expires={expires_at_tick:?} stealth={mod_stealth:?}"
        );
        Ok(())
    }

    /// Teleport an entity to an absolute world position. Useful for testing
    /// AOI region transitions, skill range checks, and positioning scenarios.
    #[reducer]
    pub fn debug_teleport(
        ctx: &ReducerContext,
        entity_id: u64,
        x: f32,
        y: f32,
        z: f32,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_teleport: admin only".into());
        }
        let tf = ctx.db.entity_transform().entity_id().find(&entity_id)
            .ok_or(format!("No transform row for entity {entity_id}"))?;

        ctx.db.entity_transform().entity_id().update(EntityTransform {
            entity_id,
            pos_x: x, pos_y: y, pos_z: z,
            rot_x: tf.rot_x, rot_y: tf.rot_y, rot_z: tf.rot_z, rot_w: tf.rot_w,
            vel_x: 0.0, vel_y: 0.0, vel_z: 0.0,
            angvel_x: 0.0, angvel_y: 0.0, angvel_z: 0.0,
            last_tick: tf.last_tick,
        });

        // Update region assignment.
        let new_rx = (x / 50.0).floor() as i32;
        let new_rz = (z / 50.0).floor() as i32;
        if let Some(er) = ctx.db.entity_region().entity_id().find(&entity_id) {
            ctx.db.entity_region().entity_id().update(EntityRegion {
                entity_id,
                region_x: new_rx,
                region_z: new_rz,
                layer: er.layer,
            });
        }

        log::info!("debug_teleport: entity={entity_id} -> ({x}, {y}, {z}) region=({new_rx}, {new_rz})");
        Ok(())
    }

    /// Grant an item to any entity's inventory. Unlike loot_item (worker-only),
    /// this is admin-callable for testing equipment and stat pipelines.
    #[reducer]
    pub fn debug_grant_item(
        ctx: &ReducerContext,
        entity_id: u64,
        item_id: u32,
        quantity: u32,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_grant_item: admin only".into());
        }
        if ctx.db.entity().entity_id().find(&entity_id).is_none() {
            return Err(format!("Entity {entity_id} does not exist"));
        }

        // Find first empty slot (scan existing slots, pick next).
        let max_slot = ctx.db.player_inventory().owner_entity().filter(&entity_id)
            .map(|r| r.slot_index)
            .max()
            .map(|s| s + 1)
            .unwrap_or(0);

        ctx.db.player_inventory().insert(PlayerInventory {
            row_id: 0,
            owner_entity: entity_id,
            slot_index: max_slot,
            item_id,
            quantity,
        });

        log::info!("debug_grant_item: entity={entity_id} item={item_id} qty={quantity} slot={max_slot}");
        Ok(())
    }

    /// Force-remove an entity. Deletes companion rows (transform, health,
    /// region, npc_state, buffs, threat). Use to clean up stuck or unwanted
    /// entities during testing.
    #[reducer]
    pub fn debug_remove_entity(
        ctx: &ReducerContext,
        entity_id: u64,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_remove_entity: admin only".into());
        }
        let entity = ctx.db.entity().entity_id().find(&entity_id)
            .ok_or(format!("Entity {entity_id} does not exist"))?;

        // Transition to Removed.
        ctx.db.entity().entity_id().update(Entity {
            entity_id,
            kind: entity.kind,
            state: EntityState::Removed,
            spawned_at_tick: entity.spawned_at_tick,
            owner_identity: entity.owner_identity,
        });

        // Clean companion rows.
        ctx.db.entity_transform().entity_id().delete(&entity_id);
        ctx.db.entity_region().entity_id().delete(&entity_id);
        ctx.db.entity_health().entity_id().delete(&entity_id);
        ctx.db.npc_state().entity_id().delete(&entity_id);
        ctx.db.npc_config().entity_id().delete(&entity_id);

        // Stealth + team.
        ctx.db.stealthed_entity().entity_id().delete(&entity_id);
        ctx.db.entity_team().entity_id().delete(&entity_id);

        // Buffs.
        ctx.db.active_buff().entity_id().delete(&entity_id);

        // Threat (as NPC or as source).
        ctx.db.threat_entry().npc_entity().delete(&entity_id);
        // Also remove threat rows where this entity is listed as a source.
        let source_threat_ids: Vec<u64> = ctx.db.threat_entry()
            .iter()
            .filter(|t| t.source_entity == entity_id)
            .map(|t| t.threat_id)
            .collect();
        for id in source_threat_ids {
            ctx.db.threat_entry().threat_id().delete(&id);
        }

        // Party, boss phase, death state, NPC goals.
        ctx.db.party_member().entity_id().delete(&entity_id);
        ctx.db.party_invite().invitee_entity().delete(&entity_id);
        ctx.db.boss_phase().boss_entity_id().delete(&entity_id);
        ctx.db.death_state().entity_id().delete(&entity_id);
        ctx.db.npc_goal().entity_id().delete(&entity_id);

        log::info!("debug_remove_entity: entity={entity_id} force-removed");
        Ok(())
    }

    /// Spawn a boss at specified coordinates. Bosses use larger meshes
    /// and different AI parameters. Admin only.
    #[reducer]
    pub fn debug_spawn_boss(
        ctx: &ReducerContext,
        pos_x: f32,
        pos_y: f32,
        pos_z: f32,
        max_hp: f32,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_spawn_boss: admin only".into());
        }

        let eid = spawn_npc_internal(ctx, EntityKind::Boss, pos_x, pos_y, pos_z, max_hp, None);
        log::info!("debug_spawn_boss: entity_id={eid} pos=({pos_x},{pos_y},{pos_z}) max_hp={max_hp}");
        Ok(())
    }

    /// Spawn a predefined test layout. Available scenarios:
    /// - `"combat"`: training dummy + reactive NPC + lock-on NPC (no chase)
    /// - `"stress"`: 1000 NPCs in a grid for performance testing
    #[reducer]
    pub fn debug_spawn_scenario(
        ctx: &ReducerContext,
        scenario: String,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_spawn_scenario: admin only".into());
        }

        match scenario.as_str() {
            "combat" => {
                // Training dummy: passive, high HP, no abilities, at (5, 1, 0)
                let e1 = spawn_npc_internal(ctx, EntityKind::Npc, 5.0, 1.0, 0.0, 10000.0, Some(NpcConfig {
                    entity_id: 0,
                    passive: true,
                    no_chase: false,
                    ability_id_1: None,
                    ability_id_2: None,
                    ability_id_3: None,
                    ability_id_4: None,
                }));
                log::info!("combat scenario: training dummy entity_id={e1}");

                // Reactive melee NPC: fights back with Slash, no chase, at (10, 1, 0)
                let e2 = spawn_npc_internal(ctx, EntityKind::Npc, 10.0, 1.0, 0.0, 500.0, Some(NpcConfig {
                    entity_id: 0,
                    passive: false,
                    no_chase: true,
                    ability_id_1: Some(1),
                    ability_id_2: None,
                    ability_id_3: None,
                    ability_id_4: None,
                }));
                log::info!("combat scenario: melee NPC entity_id={e2}");

                // Ranged NPC: fights back with Fireball (2), no chase, at (15, 1, 0)
                let e3 = spawn_npc_internal(ctx, EntityKind::Npc, 15.0, 1.0, 0.0, 500.0, Some(NpcConfig {
                    entity_id: 0,
                    passive: false,
                    no_chase: true,
                    ability_id_1: Some(2),
                    ability_id_2: None,
                    ability_id_3: None,
                    ability_id_4: None,
                }));
                log::info!("combat scenario: ranged NPC entity_id={e3}");

                // Full combat NPC: chases, multi-ability, at (20, 1, 0)
                let e4 = spawn_npc_internal(ctx, EntityKind::Npc, 20.0, 1.0, 0.0, 300.0, Some(NpcConfig {
                    entity_id: 0,
                    passive: false,
                    no_chase: false,
                    ability_id_1: Some(1),
                    ability_id_2: Some(2),
                    ability_id_3: None,
                    ability_id_4: None,
                }));
                log::info!("combat scenario: full-combat NPC entity_id={e4}");

                log::info!("combat scenario spawned: dummy={e1}, melee={e2}, ranged={e3}, full={e4}");
                Ok(())
            }
            "stress" => {
                // 1000 NPCs in a 32×32 grid (spacing=3 units), centered near origin.
                let grid_size = 32u32;
                let spacing = 3.0f32;
                let offset = (grid_size as f32 * spacing) / 2.0;
                let mut count = 0u32;
                for row in 0..grid_size {
                    for col in 0..grid_size {
                        if count >= 1000 { break; }
                        let x = col as f32 * spacing - offset;
                        let z = row as f32 * spacing - offset;
                        spawn_npc_internal(ctx, EntityKind::Npc, x, 1.0, z, 100.0, Some(NpcConfig {
                            entity_id: 0,
                            passive: true,
                            no_chase: false,
                            ability_id_1: None,
                            ability_id_2: None,
                            ability_id_3: None,
                            ability_id_4: None,
                        }));
                        count += 1;
                    }
                }
                log::info!("stress scenario spawned: {count} passive NPCs in {grid_size}x{grid_size} grid");
                Ok(())
            }
            _ => Err(format!("Unknown scenario: {scenario}. Available: combat, stress, props")),
        }
    }

    /// Spawn many NPCs for performance testing. All NPCs are passive.
    #[reducer]
    pub fn debug_spawn_many(
        ctx: &ReducerContext,
        count: u32,
        spacing: f32,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_spawn_many: admin only".into());
        }
        if count > 10000 {
            return Err("Maximum 10000 NPCs per call".into());
        }
        let grid_side = (count as f32).sqrt().ceil() as u32;
        let offset = (grid_side as f32 * spacing) / 2.0;
        let mut spawned = 0u32;
        for row in 0..grid_side {
            for col in 0..grid_side {
                if spawned >= count { break; }
                let x = col as f32 * spacing - offset;
                let z = row as f32 * spacing - offset;
                spawn_npc_internal(ctx, EntityKind::Npc, x, 1.0, z, 100.0, Some(NpcConfig {
                    entity_id: 0,
                    passive: true,
                    no_chase: false,
                    ability_id_1: None,
                    ability_id_2: None,
                    ability_id_3: None,
                    ability_id_4: None,
                }));
                spawned += 1;
            }
        }
        log::info!("debug_spawn_many: spawned {spawned} passive NPCs");
        Ok(())
    }

    /// Spawn fighting NPC pairs for combat stress testing. Admin only.
    ///
    /// Each pair consists of two non-passive NPCs 2 units apart, both armed
    /// with Slash (ability 1). `NpcState` rows with mutual targeting are
    /// inserted so the worker seeds aggro automatically on entity sync —
    /// NPCs enter Combat immediately and start casting.
    ///
    /// - `pair_count` — number of fighting pairs (max 5000 → 10 000 entities).
    /// - `pair_spacing` — distance between pair centres on the grid.
    /// - `no_chase` — if true, NPCs stay in place (stationary turrets); if
    ///   false they chase each other.
    #[reducer]
    pub fn debug_spawn_combat(
        ctx: &ReducerContext,
        pair_count: u32,
        pair_spacing: f32,
        no_chase: bool,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_spawn_combat: admin only".into());
        }
        if pair_count > 5000 {
            return Err("Maximum 5000 pairs (10 000 entities) per call".into());
        }

        let grid_side = (pair_count as f32).sqrt().ceil() as u32;
        let offset = (grid_side as f32 * pair_spacing) / 2.0;
        let mut spawned = 0u32;

        for row in 0..grid_side {
            for col in 0..grid_side {
                if spawned >= pair_count { break; }
                let cx = col as f32 * pair_spacing - offset;
                let cz = row as f32 * pair_spacing - offset;

                // Spawn pair 1 unit apart on each side of the centre.
                let id_a = spawn_npc_internal(ctx, EntityKind::Npc, cx - 1.0, 1.0, cz, 1000.0, Some(NpcConfig {
                    entity_id: 0,
                    passive: false,
                    no_chase,
                    ability_id_1: Some(1), // Slash
                    ability_id_2: None,
                    ability_id_3: None,
                    ability_id_4: None,
                }));
                let id_b = spawn_npc_internal(ctx, EntityKind::Npc, cx + 1.0, 1.0, cz, 1000.0, Some(NpcConfig {
                    entity_id: 0,
                    passive: false,
                    no_chase,
                    ability_id_1: Some(1), // Slash
                    ability_id_2: None,
                    ability_id_3: None,
                    ability_id_4: None,
                }));

                // Seed mutual aggro via npc_state — the worker restores
                // threat from target_entity on entity sync.
                ctx.db.npc_state().insert(NpcState {
                    entity_id: id_a,
                    ai_state: NpcAiState::Combat,
                    target_entity: Some(id_b),
                });
                ctx.db.npc_state().insert(NpcState {
                    entity_id: id_b,
                    ai_state: NpcAiState::Combat,
                    target_entity: Some(id_a),
                });

                spawned += 1;
            }
        }

        log::info!("debug_spawn_combat: spawned {spawned} fighting pairs ({} NPCs)", spawned * 2);
        Ok(())
    }

    /// Spawn a dynamic prop (box) at the given position. Admin only.
    /// Props are physics-driven objects that players can push around.
    #[reducer]
    pub fn debug_spawn_prop(
        ctx: &ReducerContext,
        pos_x: f32,
        pos_y: f32,
        pos_z: f32,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_spawn_prop: admin only".into());
        }

        let current_tick = ctx.db.sim_tick().iter()
            .max_by_key(|t| t.tick_id)
            .map(|t| t.tick_id)
            .unwrap_or(0);

        let entity = ctx.db.entity().insert(Entity {
            entity_id: 0,
            kind: EntityKind::Prop,
            state: EntityState::Spawning,
            spawned_at_tick: current_tick,
            owner_identity: None,
        });
        let eid = entity.entity_id;

        ctx.db.entity_transform().insert(EntityTransform {
            entity_id: eid,
            pos_x, pos_y, pos_z,
            rot_x: 0.0, rot_y: 0.0, rot_z: 0.0, rot_w: 1.0,
            vel_x: 0.0, vel_y: 0.0, vel_z: 0.0,
            angvel_x: 0.0, angvel_y: 0.0, angvel_z: 0.0,
            last_tick: current_tick,
        });

        ctx.db.entity_health().insert(EntityHealth {
            entity_id: eid,
            hp: 1.0,
            max_hp: 1.0,
        });

        ctx.db.entity_region().insert(EntityRegion {
            entity_id: eid,
            region_x: (pos_x / 50.0).floor() as i32,
            region_z: (pos_z / 50.0).floor() as i32,
            layer: 0,
        });

        log::info!("debug_spawn_prop: entity_id={eid} pos=({pos_x},{pos_y},{pos_z})");
        Ok(())
    }

    /// Move an entity to a different visibility layer. Useful for testing
    /// layer isolation in the `nearby_transforms` view (dungeon instances,
    /// phasing, stealth).
    #[reducer]
    pub fn debug_set_layer(
        ctx: &ReducerContext,
        entity_id: u64,
        layer: u32,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_set_layer: admin only".into());
        }
        let er = ctx.db.entity_region().entity_id().find(&entity_id)
            .ok_or(format!("No entity_region row for entity {entity_id}"))?;
        ctx.db.entity_region().entity_id().update(EntityRegion {
            entity_id,
            region_x: er.region_x,
            region_z: er.region_z,
            layer,
        });
        log::info!("debug_set_layer: entity={entity_id} layer={layer}");
        Ok(())
    }

    /// Assign an entity to a team. Used by stealth tests to control
    /// team-based visibility in the `nearby_transforms` view.
    #[reducer]
    pub fn debug_set_team(
        ctx: &ReducerContext,
        entity_id: u64,
        team_id: u32,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_set_team: admin only".into());
        }
        if ctx.db.entity().entity_id().find(&entity_id).is_none() {
            return Err(format!("Entity {entity_id} does not exist"));
        }
        if ctx.db.entity_team().entity_id().find(&entity_id).is_some() {
            ctx.db.entity_team().entity_id().update(EntityTeam {
                entity_id,
                team_id,
            });
        } else {
            ctx.db.entity_team().insert(EntityTeam {
                entity_id,
                team_id,
            });
        }
        log::info!("debug_set_team: entity={entity_id} team_id={team_id}");
        Ok(())
    }
}
