use crate::tables::*;
use game_schema::intent_clock::{
    DEFAULT_INTENT_INPUT_LEAD_TICKS, DEFAULT_OBSERVED_FUTURE_TOLERANCE_TICKS, IntentClockBinding,
};
use spacetimedb::{ReducerContext, Table, TimeDuration, reducer};

const CLAIM_LOOT_MAX_DISTANCE_M: f32 = 4.0;

// ── Lifecycle ───────────────────────────────────────────────────────

#[reducer(init)]
pub fn init(ctx: &ReducerContext) {
    log::info!("Module initializing — seeding tick 0");

    // Store the publishing identity as admin for owner-only reducers.
    ctx.db.module_config().insert(ModuleConfig {
        key: 0,
        admin: ctx.sender(),
        last_committed_tick: 0,
        next_instance_layer: 100,
        next_tick_id: 0,
        global_max_rewind_ticks: 4,
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

    // Reconnect: if the player has an instance_membership with disconnect_at
    // inside the grace window, clear it and restore them to the instance layer.
    if let Some(seq) = ctx.db.client_sequence().client_identity().find(&caller) {
        if let Some(membership) = ctx
            .db
            .instance_membership()
            .entity_id()
            .find(&seq.entity_id)
        {
            if membership.disconnect_at.is_some() {
                // Check the instance still exists and is active.
                if let Some(instance) = ctx
                    .db
                    .instance()
                    .instance_id()
                    .find(&membership.instance_id)
                {
                    if instance.state == InstanceState::Active
                        || instance.state == InstanceState::Pending
                    {
                        // Clear disconnect timer — player is back.
                        ctx.db
                            .instance_membership()
                            .entity_id()
                            .update(InstanceMembership {
                                entity_id: seq.entity_id,
                                instance_id: membership.instance_id,
                                disconnect_at: None,
                            });
                        // Restore entity_region to the instance layer.
                        if let Some(er) = ctx.db.entity_region().entity_id().find(&seq.entity_id) {
                            ctx.db.entity_region().entity_id().update(EntityRegion {
                                entity_id: seq.entity_id,
                                region_x: er.region_x,
                                region_z: er.region_z,
                                layer: instance.layer,
                            });
                            ctx.db.entity_layer().entity_id().update(EntityLayer {
                                entity_id: seq.entity_id,
                                layer: instance.layer,
                            });
                        }
                        log::info!(
                            "Instance reconnect: entity {} restored to instance {} (layer {})",
                            seq.entity_id,
                            membership.instance_id,
                            instance.layer
                        );
                    }
                }
            }
        }
    }
}

#[reducer(client_disconnected)]
pub fn client_disconnected(ctx: &ReducerContext) {
    let caller = ctx.sender();
    log::info!("Client disconnected: {:?}", caller);

    // If the player is in an instance, set disconnect_at for grace window.
    if let Some(seq) = ctx.db.client_sequence().client_identity().find(&caller) {
        if let Some(membership) = ctx
            .db
            .instance_membership()
            .entity_id()
            .find(&seq.entity_id)
        {
            if membership.disconnect_at.is_none() {
                ctx.db
                    .instance_membership()
                    .entity_id()
                    .update(InstanceMembership {
                        entity_id: seq.entity_id,
                        instance_id: membership.instance_id,
                        disconnect_at: Some(ctx.timestamp.to_micros_since_unix_epoch()),
                    });
                log::info!(
                    "Instance disconnect: entity {} in instance {} — grace window started",
                    seq.entity_id,
                    membership.instance_id
                );
            }
        }
    }
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

/// Prune sim_tick every N ticks to amortize scan + delete + view re-eval cost.
const PRUNE_INTERVAL: u64 = 20;

#[reducer]
pub fn tick_trigger(ctx: &ReducerContext, _schedule: TickSchedule) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("tick_trigger may only be invoked by the scheduler".into());
    }

    // Read the counter from module_config (O(1) indexed lookup).
    let mut cfg = ctx
        .db
        .module_config()
        .key()
        .find(0)
        .ok_or("module_config missing")?;
    let mut current_max = cfg.next_tick_id;

    // Backpressure guard: skip inserting a new tick if the simulation worker
    // has not committed recent ticks. This prevents sim_tick from accumulating
    // faster than the worker can consume when it is slow or disconnected.
    //
    // Guard is only active once a worker has committed at least one tick
    // (last_committed_tick > 0).  Before that — clean deploy, hot-reload after a
    // schema migration that zero-initialises the field, or a fresh server with no
    // worker yet — we let ticks flow freely.  The sim_tick table is already bounded
    // by the SIM_TICK_RETAIN pruning below, so unbounded growth is not a concern.
    if cfg.last_committed_tick > 0 {
        let backlog = current_max.saturating_sub(cfg.last_committed_tick);
        if backlog > BACKLOG_LIMIT {
            // Self-healing: the worker that created these sim_tick rows crashed
            // before committing results. Delete the orphaned rows and reset
            // last_committed_tick to 0 (cold-start mode). The next worker that
            // connects will seed from MAX(sim_tick) normally, and its first
            // commit will bootstrap the sequence via the cold-start bypass in
            // commit_tick_results.
            let to_delete: Vec<u64> = ctx
                .db
                .sim_tick()
                .iter()
                .filter(|t| t.tick_id > cfg.last_committed_tick)
                .map(|t| t.tick_id)
                .collect();
            let count = to_delete.len();
            for id in to_delete {
                ctx.db.sim_tick().tick_id().delete(&id);
            }
            current_max = cfg.last_committed_tick;
            cfg.last_committed_tick = 0;
            cfg.next_tick_id = current_max;
            ctx.db.module_config().key().update(cfg);
            log::warn!(
                "tick_trigger: cleaned {count} orphaned sim_tick rows (last_committed was {}) — entering cold-start recovery",
                current_max
            );
            // Re-read cfg after update for the insert below.
            cfg = ctx
                .db
                .module_config()
                .key()
                .find(0)
                .ok_or("module_config missing after recovery")?;
        }
    }

    let next_tick_id = current_max + 1;

    ctx.db.sim_tick().insert(SimTick {
        tick_id: next_tick_id,
        timestamp_us: ctx.timestamp.to_micros_since_unix_epoch(),
    });

    // Update the counter so the next invocation skips the O(N) scan.
    cfg.next_tick_id = next_tick_id;
    ctx.db.module_config().key().update(cfg);

    // Prune old sim_tick rows to cap table size.
    // Batched every PRUNE_INTERVAL ticks to amortize scan + delete + view re-eval.
    if next_tick_id > SIM_TICK_RETAIN && next_tick_id % PRUNE_INTERVAL == 0 {
        let cutoff = next_tick_id - SIM_TICK_RETAIN;
        let to_delete: Vec<u64> = ctx
            .db
            .sim_tick()
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
// thresholds and transitions world_phase.

#[reducer]
pub fn world_clock(ctx: &ReducerContext, _schedule: WorldClockSchedule) -> Result<(), String> {
    if ctx.sender() != ctx.database_identity() {
        return Err("world_clock may only be invoked by the scheduler".into());
    }

    let now = ctx.timestamp.to_micros_since_unix_epoch();
    let expiry_report = expire_instances_inner(ctx, now)?;
    if expiry_report.has_activity() {
        log::info!(
            "world_clock: instance maintenance expired={} returned_members={} stale_members={} removed_entities={} cleared_buffs={} cleared_interactables={} cleared_counters={} cleared_phases={} cleared_world_events={}",
            expiry_report.expired_instances,
            expiry_report.returned_members,
            expiry_report.stale_members,
            expiry_report.removed_entities,
            expiry_report.cleared_buffs,
            expiry_report.cleared_interactables,
            expiry_report.cleared_counters,
            expiry_report.cleared_phases,
            expiry_report.cleared_world_events,
        );
    }

    // ── Tier 2 evaluation: zone_counter → world_phase transitions ───
    //
    // Aggregate zone_counter rows per (layer, region_x, region_z) and
    // evaluate simple threshold rules.  When a threshold is met and no
    // matching world_phase row exists (or the phase name differs), upsert
    // the world_phase table.
    //
    // Threshold convention (V1 — hardcoded, future: data-driven):
    //   "kills" >= 5.0  → phase "boss_ready"
    //   "boss_killed" >= 1.0 → phase "completed"
    //
    // zone_id is synthesised as `layer * 1_000_000 + (region_x+500)*1000 + (region_z+500)`
    // to produce a unique u32 per cell.  This is deliberately lossy outside
    // ±499 but sufficient for the dungeon-instance use case.

    struct ThresholdRule {
        counter_name: &'static str,
        threshold: f64,
        phase_name: &'static str,
        /// Higher priority wins when multiple rules match the same zone.
        priority: u32,
        /// `required_players` for the matching `world_event` row produced
        /// alongside the `world_phase` transition. Used by director
        /// triggers gated on `WorldEventActive { tag, min_players }` to
        /// prevent offscreen spawns (Finding #4, 2026-06-09 review).
        /// `0` = no presence gate (terminal events like `completed`).
        required_players: u32,
        /// Initial state of the produced `world_event` row.
        event_state: WorldActivityEventState,
    }

    const RULES: &[ThresholdRule] = &[
        ThresholdRule {
            counter_name: "boss_killed",
            threshold: 1.0,
            phase_name: "completed",
            priority: 10,
            required_players: 0,
            event_state: WorldActivityEventState::Completed,
        },
        ThresholdRule {
            counter_name: "kills",
            threshold: 5.0,
            phase_name: "boss_ready",
            priority: 1,
            required_players: 1,
            event_state: WorldActivityEventState::Active,
        },
    ];

    // Collect all zone_counter rows into per-zone summed maps.
    //
    // Sum same-name counters in the same zone defensively: writers go
    // through `upsert_zone_counter` so logical-key uniqueness is
    // maintained today, but if a legacy bug ever produced duplicate rows
    // for the same `(layer, rx, rz, counter_name)`, this prevents
    // world_clock from under-evaluating a threshold (Finding #5).
    // `HashMap<(zone, name), f64>` makes the summation O(1) per row.
    let mut zone_counters: std::collections::HashMap<(u32, i32, i32, String), f64> =
        std::collections::HashMap::new();
    for c in ctx.db.zone_counter().iter() {
        *zone_counters
            .entry((c.layer, c.region_x, c.region_z, c.counter_name.clone()))
            .or_insert(0.0) += c.value;
    }
    let mut zones: std::collections::HashMap<(u32, i32, i32), Vec<(String, f64)>> =
        std::collections::HashMap::new();
    for ((layer, rx, rz, name), value) in zone_counters {
        zones
            .entry((layer, rx, rz))
            .or_default()
            .push((name, value));
    }

    for (&(layer, rx, rz), counters) in &zones {
        // Find the highest-priority matching rule for this zone.
        let mut best: Option<&ThresholdRule> = None;
        for rule in RULES {
            let met = counters
                .iter()
                .any(|(name, val)| name == rule.counter_name && *val >= rule.threshold);
            if met {
                if best.map_or(true, |b| rule.priority > b.priority) {
                    best = Some(rule);
                }
            }
        }

        if let Some(rule) = best {
            let zone_id = synthesise_zone_id(layer, rx, rz);
            let existing = ctx.db.world_phase().zone_id().find(&zone_id);
            let needs_write = match &existing {
                Some(wp) => wp.phase_name != rule.phase_name,
                None => true,
            };
            if needs_write {
                let wp = WorldPhase {
                    zone_id,
                    phase_name: rule.phase_name.to_string(),
                    started_at: now,
                    metadata: String::new(),
                };
                if existing.is_some() {
                    ctx.db.world_phase().zone_id().update(wp);
                } else {
                    ctx.db.world_phase().insert(wp);
                }
                log::info!(
                    "world_clock: zone ({},{},{}) → phase '{}' (counter '{}' >= {})",
                    layer,
                    rx,
                    rz,
                    rule.phase_name,
                    rule.counter_name,
                    rule.threshold
                );
            }
            // Mirror the phase transition as a `world_activity_event` row so
            // director triggers can gate on presence (Finding #4, messaging
            // spine step 4). One row per logical `(scope, tag)` — the helper
            // updates state in place if the row already exists, so a
            // re-evaluation that lands on the same rule doesn't spam
            // duplicates.
            upsert_world_activity_event(
                ctx,
                layer,
                rx,
                rz,
                rule.phase_name,
                rule.event_state,
                rule.required_players,
                now,
            );
            if rule.phase_name == "completed" {
                mark_instance_completed_for_layer(ctx, layer);
            }
        }
    }

    log::trace!("world_clock: evaluated {} zones", zones.len());
    Ok(())
}

/// Synthesise a deterministic `zone_id: u32` from (layer, region_x, region_z).
/// Covers ±499 cells per axis per layer.  Dungeon instances (layer ≥ 100) with
/// small arenas are well within range.
fn synthesise_zone_id(layer: u32, rx: i32, rz: i32) -> u32 {
    layer
        .wrapping_mul(1_000_000)
        .wrapping_add(((rx + 500) as u32).wrapping_mul(1000))
        .wrapping_add((rz + 500) as u32)
}

/// Single upsert path for `zone_counter`.
///
/// SpacetimeDB v2 cannot enforce composite uniqueness in storage (see the
/// `terrain_set` comment in `tables.rs`), so logical uniqueness on
/// `(layer, region_x, region_z, counter_name)` is enforced by funnelling all
/// writers through this one helper. `commit_tick_results` and the
/// `increment_zone_counter` admin/debug reducer both call here. If a third
/// raw-insert path is ever introduced, this helper is the place to add a
/// linter check or a hard-fail. See Finding #5 in the 2026-06-09 messaging
/// review and `world_clock`'s defensive summation for the read-side mirror.
fn upsert_zone_counter(
    ctx: &ReducerContext,
    layer: u32,
    region_x: i32,
    region_z: i32,
    counter_name: String,
    delta: f64,
) {
    let existing = ctx
        .db
        .zone_counter()
        .by_zone()
        .filter((layer, region_x, region_z..=region_z))
        .find(|c| c.counter_name == counter_name);

    if let Some(mut row) = existing {
        row.value += delta;
        ctx.db.zone_counter().counter_id().update(row);
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
}

/// Single upsert path for `world_activity_event`.
///
/// Same workspace pattern as `upsert_zone_counter`: SpacetimeDB v2 cannot
/// express composite uniqueness in storage, so logical uniqueness on
/// `(scope_layer, scope_region_x, scope_region_z, tag)` is enforced by
/// routing all writers through here. When a row for the scope+tag
/// already exists the state / required_players / payload are updated in
/// place; `started_at` is only set on insert so timer-based events
/// retain their original activation tick. Future kinds (Timer, Chain,
/// Escalation) extend through `payload` without a schema migration. See
/// the table comment in `tables.rs` and Finding #4 of the 2026-06-09
/// messaging-spine review.
fn upsert_world_activity_event(
    ctx: &ReducerContext,
    scope_layer: u32,
    scope_region_x: i32,
    scope_region_z: i32,
    tag: &str,
    state: WorldActivityEventState,
    required_players: u32,
    now_micros: i64,
) {
    let existing = ctx
        .db
        .world_activity_event()
        .by_scope()
        .filter((scope_layer, scope_region_x, scope_region_z..=scope_region_z))
        .find(|e| e.tag == tag);

    if let Some(mut row) = existing {
        // Only write if something observable changed; avoids waking up
        // every worker subscription on identical re-evaluations.
        if row.state != state || row.required_players != required_players {
            row.state = state;
            row.required_players = required_players;
            ctx.db.world_activity_event().event_id().update(row);
        }
    } else {
        ctx.db.world_activity_event().insert(WorldActivityEvent {
            event_id: 0,
            scope_layer,
            scope_region_x,
            scope_region_z,
            tag: tag.to_string(),
            state,
            required_players,
            started_at: now_micros,
            payload: String::new(),
        });
    }
}

fn mark_instance_completed_for_layer(ctx: &ReducerContext, layer: u32) {
    if layer < 100 {
        return;
    }

    let Some(instance) = ctx.db.instance().iter().find(|i| {
        i.layer == layer && (i.state == InstanceState::Active || i.state == InstanceState::Pending)
    }) else {
        return;
    };

    ctx.db.instance().instance_id().update(Instance {
        instance_id: instance.instance_id,
        template_id: instance.template_id,
        layer: instance.layer,
        layer_group: instance.layer_group,
        state: InstanceState::Completed,
        created_at: instance.created_at,
        expires_at: instance.expires_at,
        max_players: instance.max_players,
    });
    log::info!(
        "Instance completed: id={} layer={}",
        instance.instance_id,
        instance.layer
    );
}

fn layer_has_completed_world_phase(ctx: &ReducerContext, layer: u32) -> bool {
    ctx.db
        .world_phase()
        .iter()
        .any(|wp| wp.phase_name == "completed" && wp.zone_id / 1_000_000 == layer)
}

#[derive(Clone, Copy)]
struct IntentClockContext {
    current_tick: u64,
    max_rewind_ticks: u64,
}

fn read_intent_clock_context(ctx: &ReducerContext) -> Result<IntentClockContext, String> {
    let cfg = ctx
        .db
        .module_config()
        .key()
        .find(0)
        .ok_or("module_config missing")?;
    Ok(IntentClockContext {
        current_tick: cfg.next_tick_id,
        max_rewind_ticks: cfg.global_max_rewind_ticks as u64,
    })
}

fn bind_player_intent_clock(
    clock: IntentClockContext,
    client_observed_tick: u64,
) -> Result<IntentClockBinding, String> {
    game_schema::intent_clock::bind_intent_clock(
        clock.current_tick,
        DEFAULT_INTENT_INPUT_LEAD_TICKS,
        client_observed_tick,
        clock.max_rewind_ticks,
        DEFAULT_OBSERVED_FUTURE_TOLERANCE_TICKS,
    )
    .map_err(|err| format!("Invalid client_observed_tick: {err}"))
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
    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
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

    // Rate limit: cap the number of pending (unprocessed) intents per entity.
    // At 20 Hz a queue depth of 5 = 250 ms of buffered input — enough for normal
    // gameplay but prevents a buggy or malicious client from flooding DB writes.
    const MAX_QUEUED_INTENTS: usize = 5;
    let queued = ctx
        .db
        .player_intent()
        .entity_id()
        .filter(&entity_id)
        .count();
    if queued >= MAX_QUEUED_INTENTS {
        return Err("Intent queue full: reduce submission rate".into());
    }

    let clock = read_intent_clock_context(ctx)?;
    let binding = bind_player_intent_clock(clock, client_observed_tick)?;

    ctx.db.player_intent().insert(PlayerIntent {
        intent_id: 0, // auto_inc
        client_identity: caller,
        entity_id,
        sequence_id,
        target_tick: binding.target_tick,
        client_observed_tick: binding.client_observed_tick,
        action,
    });

    // Update last processed sequence only after successful insert
    ctx.db
        .client_sequence()
        .client_identity()
        .update(ClientSequence {
            client_identity: caller,
            last_processed_sequence: sequence_id,
            entity_id: seq.entity_id,
        });

    Ok(())
}

/// One element of a batched intent submission.
///
/// Carries the same payload as `submit_intent`'s positional arguments minus
/// the entity_id (which is shared across the batch and validated once).
#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct BatchedIntent {
    pub sequence_id: u64,
    pub client_observed_tick: u64,
    pub action: IntentAction,
}

/// Submit a redundant batch of recent intents in a single reducer call.
///
/// The client maintains a small ring buffer of recent unacknowledged intents
/// and resends the tail on every send tick. The server idempotently drops
/// any entry that is either already processed (`sequence_id <=
/// last_processed_sequence`) or already queued (matching `sequence_id` row
/// already present for `entity_id`). New entries are scheduled for
/// the server-owned intent clock policy, identical to the single-shot path.
///
/// Rate-limit semantics: rather than rejecting the whole call when the
/// resulting queue would exceed `MAX_QUEUED_INTENTS`, the server inserts
/// new entries until the cap is reached and silently drops the rest. The
/// client will resend dropped entries in the next batch — a single
/// dropped network call no longer loses input forever.
#[reducer]
pub fn submit_intents_batch(
    ctx: &ReducerContext,
    entity_id: u64,
    intents: Vec<BatchedIntent>,
) -> Result<(), String> {
    let caller = ctx.sender();

    // Verify client owns this entity.
    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Client not registered")?;

    if seq.entity_id != entity_id {
        return Err("Client does not own this entity".into());
    }

    if intents.is_empty() {
        return Ok(());
    }

    // Reject batches large enough to be abusive on their own. The legitimate
    // client ring buffer is bounded; anything substantially larger is either
    // a bug or an attempt to flood DB writes.
    const MAX_BATCH_LEN: usize = 16;
    if intents.len() > MAX_BATCH_LEN {
        return Err(format!(
            "Intent batch too large: {} > {MAX_BATCH_LEN}",
            intents.len()
        ));
    }

    // Snapshot the queue's current sequence_ids and depth in a single scan.
    let mut queued_seqs: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut queued_depth: usize = 0;
    for row in ctx.db.player_intent().entity_id().filter(&entity_id) {
        queued_seqs.insert(row.sequence_id);
        queued_depth += 1;
    }

    // Process in ascending sequence order so `last_processed_sequence`
    // advances monotonically and dropped-tail entries are the newest ones,
    // which the next batch will resend with the lowest urgency cost.
    let mut sorted = intents;
    sorted.sort_by_key(|i| i.sequence_id);

    let clock = read_intent_clock_context(ctx)?;
    let target_tick = game_schema::intent_clock::compute_intent_target_tick(
        clock.current_tick,
        DEFAULT_INTENT_INPUT_LEAD_TICKS,
    );

    const MAX_QUEUED_INTENTS: usize = 5;
    let mut highest_inserted: u64 = seq.last_processed_sequence;
    let mut inserted = 0usize;
    let mut stale = 0usize;
    let mut duplicate = 0usize;
    let mut dropped_queue_full = 0usize;

    let mut pending_inserts: Vec<(BatchedIntent, u64)> = Vec::new();

    for intent in sorted {
        // Idempotent: already-processed intents are silently skipped.
        if intent.sequence_id <= seq.last_processed_sequence {
            stale += 1;
            continue;
        }
        // Idempotent: already-queued sequence_ids are silently skipped.
        // Note: we deliberately do NOT advance `highest_inserted` here.
        // A duplicate is "already accepted" so it's safe in principle, but
        // it would let a single seen-but-unprocessed sequence mask a true
        // gap (e.g. queued=[12], batch=[12,14] would advance past 13).
        // Only insertions advance the cursor.
        if queued_seqs.contains(&intent.sequence_id) {
            duplicate += 1;
            continue;
        }
        // Cap the resulting queue depth, not the batch size.
        if queued_depth >= MAX_QUEUED_INTENTS {
            dropped_queue_full += 1;
            continue;
        }

        let sequence_id = intent.sequence_id;
        let binding = bind_player_intent_clock(clock, intent.client_observed_tick)?;

        pending_inserts.push((intent, binding.client_observed_tick));
        queued_seqs.insert(sequence_id);
        queued_depth += 1;
        inserted += 1;
        if sequence_id > highest_inserted {
            highest_inserted = sequence_id;
        }
    }

    for (intent, client_observed_tick) in pending_inserts {
        ctx.db.player_intent().insert(PlayerIntent {
            intent_id: 0, // auto_inc
            client_identity: caller,
            entity_id,
            sequence_id: intent.sequence_id,
            target_tick,
            client_observed_tick,
            action: intent.action,
        });
    }

    // Telemetry: structured log when a batch produced anything other than
    // a clean all-duplicate redundancy resend. Stale/duplicate are normal
    // for the redundancy mechanism; dropped_queue_full is the signal that
    // the client is sending faster than the queue cap allows.
    if inserted > 0 || dropped_queue_full > 0 {
        log::debug!(
            "submit_intents_batch entity={} inserted={} stale={} duplicate={} dropped_queue_full={} highest_inserted={}",
            entity_id,
            inserted,
            stale,
            duplicate,
            dropped_queue_full,
            highest_inserted,
        );
    }
    if dropped_queue_full > 0 {
        log::warn!(
            "intent batch tail dropped (queue full) entity={} dropped={} queue_depth={}",
            entity_id,
            dropped_queue_full,
            queued_depth,
        );
    }

    // Advance the sequence cursor only if we inserted something new.
    // `highest_inserted` is updated only on actual insertion above, so this
    // never advances past a sequence the server hasn't accepted.
    if inserted > 0 && highest_inserted > seq.last_processed_sequence {
        ctx.db
            .client_sequence()
            .client_identity()
            .update(ClientSequence {
                client_identity: caller,
                last_processed_sequence: highest_inserted,
                entity_id: seq.entity_id,
            });
    }

    Ok(())
}

// ── Entity Spawning ─────────────────────────────────────────────────
// Register a player entity for the connected client.

#[reducer]
pub fn spawn_player(ctx: &ReducerContext) -> Result<(), String> {
    let caller = ctx.sender();

    // A client can own only one spawned player entity.
    if ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .is_some()
    {
        return Err("Player already spawned".into());
    }

    let current_tick = ctx
        .db
        .module_config()
        .key()
        .find(0)
        .map(|c| c.next_tick_id)
        .unwrap_or(0);

    let entity = ctx.db.entity().insert(Entity {
        entity_id: 0, // auto_inc
        kind: EntityKind::Player,
        state: EntityState::Spawning,
        spawned_at_tick: current_tick,
        owner_identity: Some(caller),
        rls_group: 0,
    });

    let eid = entity.entity_id;

    // Resolve the open-world spawn position via the unified resolver
    // (RespawnPoint table → layers.ron → fallback). Layer 0 is the
    // open world; alt starting layers would be a separate reducer.
    let spawn_pos = resolve_layer_spawn(ctx, 0, None);
    let region_x = (spawn_pos[0] / 50.0).floor() as i32;
    let region_z = (spawn_pos[2] / 50.0).floor() as i32;

    ctx.db.entity_transform().insert(EntityTransform {
        entity_id: eid,
        pos_x: spawn_pos[0],
        pos_y: spawn_pos[1],
        pos_z: spawn_pos[2],
        rot_x: 0.0,
        rot_y: 0.0,
        rot_z: 0.0,
        rot_w: 1.0,
        vel_x: 0.0,
        vel_y: 0.0,
        vel_z: 0.0,
        angvel_x: 0.0,
        angvel_y: 0.0,
        angvel_z: 0.0,
        last_tick: current_tick,
        rls_group: 0,
    });

    ctx.db.entity_health().insert(EntityHealth {
        entity_id: eid,
        hp: 1000.0,
        max_hp: 1000.0,
        rls_group: 0,
    });

    ctx.db.entity_region().insert(EntityRegion {
        entity_id: eid,
        region_x,
        region_z,
        layer: 0,
    });

    ctx.db.entity_layer().insert(EntityLayer {
        entity_id: eid,
        layer: 0,
    });

    ctx.db.client_sequence().insert(ClientSequence {
        client_identity: caller,
        last_processed_sequence: 0,
        entity_id: eid,
    });

    log::info!(
        "Player spawned: entity_id={}, identity={:?}, pos=({:.1},{:.1},{:.1})",
        eid,
        caller,
        spawn_pos[0],
        spawn_pos[1],
        spawn_pos[2]
    );
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
    spawn_npc_internal_on_layer(ctx, kind, pos_x, pos_y, pos_z, max_hp, 0, config)
}

fn spawn_npc_internal_on_layer(
    ctx: &ReducerContext,
    kind: EntityKind,
    pos_x: f32,
    pos_y: f32,
    pos_z: f32,
    max_hp: f32,
    layer: u32,
    config: Option<NpcConfig>,
) -> u64 {
    let current_tick = ctx
        .db
        .module_config()
        .key()
        .find(0)
        .map(|c| c.next_tick_id)
        .unwrap_or(0);

    let entity = ctx.db.entity().insert(Entity {
        entity_id: 0,
        kind,
        state: EntityState::Spawning,
        spawned_at_tick: current_tick,
        owner_identity: None,
        rls_group: 0,
    });
    let eid = entity.entity_id;

    ctx.db.entity_transform().insert(EntityTransform {
        entity_id: eid,
        pos_x,
        pos_y,
        pos_z,
        rot_x: 0.0,
        rot_y: 0.0,
        rot_z: 0.0,
        rot_w: 1.0,
        vel_x: 0.0,
        vel_y: 0.0,
        vel_z: 0.0,
        angvel_x: 0.0,
        angvel_y: 0.0,
        angvel_z: 0.0,
        last_tick: current_tick,
        rls_group: 0,
    });

    ctx.db.entity_health().insert(EntityHealth {
        entity_id: eid,
        hp: max_hp,
        max_hp,
        rls_group: 0,
    });

    ctx.db.entity_region().insert(EntityRegion {
        entity_id: eid,
        region_x: (pos_x / 50.0).floor() as i32,
        region_z: (pos_z / 50.0).floor() as i32,
        layer,
    });

    ctx.db.entity_layer().insert(EntityLayer {
        entity_id: eid,
        layer,
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
    log::info!(
        "NPC spawned: entity_id={} pos=({},{},{}) max_hp={}",
        eid,
        pos_x,
        pos_y,
        pos_z,
        max_hp
    );
    Ok(())
}
// ── Commit Tick Results ─────────────────────────────────────────────
// Trusted-worker reducer: applies one authoritative simulation tick to DB rows.

/// Check if the caller is the module admin (the identity that published the module).
fn is_module_admin(ctx: &ReducerContext) -> bool {
    if ctx.sender() == ctx.database_identity() {
        return true;
    }
    ctx.db
        .module_config()
        .key()
        .find(0)
        .is_some_and(|cfg| ctx.sender() == cfg.admin)
}

/// Check if the caller is the module itself or a registered simulation worker.
fn is_trusted_caller(ctx: &ReducerContext) -> bool {
    if ctx.sender() == ctx.database_identity() {
        return true;
    }
    ctx.db
        .trusted_worker()
        .worker_identity()
        .find(&ctx.sender())
        .is_some()
}

/// When the `debug` feature is enabled, any authenticated caller is allowed
/// to invoke admin/debug reducers. In production builds this always returns false.
#[cfg(feature = "debug")]
fn is_debug_caller(_ctx: &ReducerContext) -> bool {
    true
}
#[cfg(not(feature = "debug"))]
fn is_debug_caller(_ctx: &ReducerContext) -> bool {
    false
}

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
    npc_state_updates: Vec<NpcStateUpdate>,
    director_spawns: Vec<DirectorSpawnInput>,
    encounter_memberships: Vec<EncounterAddMembershipInput>,
    interactable_updates: Vec<InteractableUpdate>,
    death_state_inserts: Vec<DeathStateInsertInput>,
    loot_rolls: Vec<LootRollInput>,
    sim_log_entries: Vec<SimLogInput>,
    boss_phase_updates: Vec<BossPhaseUpdateInput>,
    zone_counter_deltas: Vec<ZoneCounterDeltaInput>,
) -> Result<(), String> {
    // Accept only trusted worker identities (or module identity in internal calls).
    if !is_trusted_caller(ctx) {
        return Err("commit_tick_results may only be invoked by a trusted worker".into());
    }

    // ── Tick sequence guard (read-only, BEFORE any mutations) ─────────
    // Reject duplicates and gaps before touching any table.  The cursor
    // advance itself is deferred to the end so that if any mutation in the
    // middle fails the cursor stays unchanged.
    let cfg = ctx.db.module_config().key().find(0);
    if let Some(ref c) = cfg {
        if c.last_committed_tick != 0 {
            if tick_id <= c.last_committed_tick {
                // Duplicate / replay — return Ok so the worker ack succeeds,
                // but skip all mutations below.
                log::warn!(
                    "commit_tick_results: tick_id={} already committed (last={}), skipping entire reducer",
                    tick_id,
                    c.last_committed_tick
                );
                return Ok(());
            }
            let expected = c.last_committed_tick + 1;
            if tick_id != expected {
                // Gap detected — reject so the worker sees a reducer error and retries.
                return Err(format!(
                    "commit_tick_results: tick_id={} skips expected {} — gap rejected",
                    tick_id, expected
                ));
            }
        }
    }

    // Apply transform updates
    for t in transforms {
        if ctx
            .db
            .entity_transform()
            .entity_id()
            .find(&t.entity_id)
            .is_none()
        {
            log::warn!(
                "commit_tick_results tick={}: no entity_transform row for entity_id={} — update skipped",
                tick_id,
                t.entity_id
            );
            continue;
        }
        ctx.db
            .entity_transform()
            .entity_id()
            .update(EntityTransform {
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
                last_tick: tick_id,
                rls_group: 0,
            });
    }

    // Apply health updates
    for h in health_updates {
        if ctx
            .db
            .entity_health()
            .entity_id()
            .find(&h.entity_id)
            .is_none()
        {
            log::warn!(
                "commit_tick_results tick={}: no entity_health row for entity_id={} — update skipped",
                tick_id,
                h.entity_id
            );
            continue;
        }
        ctx.db.entity_health().entity_id().update(EntityHealth {
            entity_id: h.entity_id,
            hp: h.hp,
            max_hp: h.max_hp,
            rls_group: 0,
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
                rls_group: 0,
            });

            // Create death state for player deaths from authoritative
            // worker-emitted rows (see `death_state_inserts` below). The
            // reducer no longer derives death state from observed lifecycle
            // transitions — doing so loses killer attribution and silently
            // fabricates fallbacks for missing position/layer.

            // Clean up companion rows for entities reaching terminal Removed state
            // so they disappear from nearby_transforms and other views.
            if u.new_state == EntityState::Removed {
                delete_removed_entity_companion_rows(ctx, u.entity_id);
                // Note: death_state is NOT deleted here — players need it for respawn.
                // Buff rows for Removed entities are already cleaned up by the
                // buff_cleared section below — the worker explicitly adds Removed
                // entity IDs to that list.
                log::info!(
                    "commit_tick_results tick={}: entity {} removed — companion rows deleted",
                    tick_id,
                    u.entity_id
                );
            }
        } else {
            log::warn!(
                "commit_tick_results tick={}: no entity row for entity_id={} — state update skipped",
                tick_id,
                u.entity_id
            );
        }
    }

    // Insert authoritative death-state rows produced by the worker.
    // The reducer applies respawn-timing policy (`RESPAWN_DELAY_TICKS`) but
    // does not derive the row contents — killer attribution, layer, and
    // death position come from worker simulation state.
    for d in death_state_inserts {
        if ctx
            .db
            .death_state()
            .entity_id()
            .find(&d.entity_id)
            .is_some()
        {
            // Idempotent by design: a player keeps a single `death_state` row
            // for the duration of their respawn cycle (the row is deleted when
            // they actually respawn). If a duplicate insert arrives for the
            // same player within that window — e.g. resurrected by a buff and
            // killed again before the respawn timer elapses — the original
            // row's `respawn_at_tick` is preserved. This is intentional: the
            // first death's respawn schedule wins until the cycle completes.
            continue;
        }
        ctx.db.death_state().insert(DeathState {
            entity_id: d.entity_id,
            died_at_tick: tick_id,
            respawn_at_tick: tick_id + RESPAWN_DELAY_TICKS,
            killer_entity: d.killer_entity,
            layer: d.layer,
            death_pos_x: d.death_pos_x,
            death_pos_y: d.death_pos_y,
            death_pos_z: d.death_pos_z,
        });
    }

    for roll in loot_rolls {
        if roll.items.is_empty() || roll.eligible_claimants.is_empty() {
            log::warn!(
                "commit_tick_results tick={}: dropping loot roll for corpse {} (items={}, claimants={})",
                tick_id,
                roll.corpse_entity,
                roll.items.len(),
                roll.eligible_claimants.len(),
            );
            continue;
        }

        let pile = ctx.db.loot_pile().insert(LootPile {
            loot_pile_id: 0,
            corpse_entity: roll.corpse_entity,
            layer: roll.layer,
            pos_x: roll.pos_x,
            pos_y: roll.pos_y,
            pos_z: roll.pos_z,
            eligible_claimants: roll.eligible_claimants,
            created_at_tick: tick_id,
            expires_at_tick: tick_id.saturating_add(roll.claim_window_ticks),
        });

        for item in roll.items {
            if item.quantity == 0 {
                continue;
            }
            ctx.db.loot_pile_item().insert(LootPileItem {
                loot_pile_item_id: 0,
                loot_pile_id: pile.loot_pile_id,
                item_id: item.item_id,
                quantity: item.quantity,
            });
        }
    }

    // Apply region updates.
    // The sim worker mirrors the DB-authoritative layer via the
    // entity_layer.on_update → set_entity_layer → entity_regions[eid].layer
    // bridge, so r.layer is already the correct value.  Previously this loop
    // did a per-entity entity_region().find() to preserve the DB layer, which
    // added O(N) indexed reads on every heavy tick.
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
                mod_ai_override_kind: b.mod_ai_override_kind,
                mod_ai_override_target: b.mod_ai_override_target,
                mod_stealth: b.mod_stealth,
                last_dot_tick: b.last_dot_tick,
            });
        }

        // Derive stealthed_entity from committed buffs.
        // For each entity whose buffs were refreshed this tick, check if any
        // buff has mod_stealth=true. If so, look up the entity's team and
        // upsert a stealthed_entity row; otherwise delete any existing row.
        for &entity_id in &buff_cleared_entity_ids {
            let is_stealthed = ctx
                .db
                .active_buff()
                .entity_id()
                .filter(&entity_id)
                .any(|b| b.mod_stealth == Some(true));
            if is_stealthed {
                let team_id = ctx
                    .db
                    .entity_team()
                    .entity_id()
                    .find(&entity_id)
                    .map(|t| t.team_id)
                    .unwrap_or(0);
                if ctx
                    .db
                    .stealthed_entity()
                    .entity_id()
                    .find(&entity_id)
                    .is_some()
                {
                    ctx.db
                        .stealthed_entity()
                        .entity_id()
                        .update(StealthedEntity { entity_id, team_id });
                } else {
                    ctx.db
                        .stealthed_entity()
                        .insert(StealthedEntity { entity_id, team_id });
                }
            } else {
                ctx.db.stealthed_entity().entity_id().delete(&entity_id);
            }
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
    let mut director_spawn_ids: Vec<u64> = Vec::with_capacity(director_spawns.len());
    let mut director_spawn_layers: Vec<u32> = Vec::with_capacity(director_spawns.len());
    for s in director_spawns {
        let entity = ctx.db.entity().insert(Entity {
            entity_id: 0, // auto_inc
            kind: s.kind,
            state: EntityState::Spawning,
            spawned_at_tick: tick_id,
            owner_identity: None,
            rls_group: 0,
        });
        let eid = entity.entity_id;
        director_spawn_ids.push(eid);
        director_spawn_layers.push(s.layer);

        ctx.db.entity_transform().insert(EntityTransform {
            entity_id: eid,
            pos_x: s.pos_x,
            pos_y: s.pos_y,
            pos_z: s.pos_z,
            rot_x: 0.0,
            rot_y: 0.0,
            rot_z: 0.0,
            rot_w: 1.0,
            vel_x: 0.0,
            vel_y: 0.0,
            vel_z: 0.0,
            angvel_x: 0.0,
            angvel_y: 0.0,
            angvel_z: 0.0,
            last_tick: tick_id,
            rls_group: 0,
        });

        ctx.db.entity_health().insert(EntityHealth {
            entity_id: eid,
            hp: s.max_hp,
            max_hp: s.max_hp,
            rls_group: 0,
        });

        ctx.db.entity_region().insert(EntityRegion {
            entity_id: eid,
            region_x: (s.pos_x / 50.0).floor() as i32,
            region_z: (s.pos_z / 50.0).floor() as i32,
            layer: s.layer,
        });

        ctx.db.entity_layer().insert(EntityLayer {
            entity_id: eid,
            layer: s.layer,
        });

        if let Some(team_id) = s.team_id {
            ctx.db.entity_team().insert(EntityTeam {
                entity_id: eid,
                team_id,
            });
        }

        if let Some(cfg) = s.npc_config {
            if s.kind == EntityKind::Npc || s.kind == EntityKind::Boss {
                let mut ability_ids = cfg.ability_ids.into_iter().take(4);
                ctx.db.npc_config().insert(NpcConfig {
                    entity_id: eid,
                    encounter_name: None,
                    archetype_id: None,
                    passive: cfg.passive,
                    no_chase: cfg.no_chase,
                    ability_id_1: ability_ids.next(),
                    ability_id_2: ability_ids.next(),
                    ability_id_3: ability_ids.next(),
                    ability_id_4: ability_ids.next(),
                    leash_radius: cfg.leash_radius,
                    aggro_radius: cfg.aggro_radius,
                    body_shape: cfg.body_shape,
                });
            } else {
                log::warn!(
                    "director spawn {} kind {:?} carried npc_config; dropping actor config",
                    eid,
                    s.kind
                );
            }
        }
    }

    // ── Encounter Add Memberships ─────────────────────────────────────
    // Pair each membership with the matching director spawn by index and
    // insert an `encounter_add` row carrying the boss link + tag list.
    // See `docs/contracts/spawn_add_membership_contract.md`.
    for m in encounter_memberships {
        let idx = m.spawn_index as usize;
        if idx >= director_spawn_ids.len() {
            log::warn!(
                "encounter_add: spawn_index {} out of range (director_spawns={}); skipping",
                m.spawn_index,
                director_spawn_ids.len()
            );
            continue;
        }
        // Validate boss exists; skip cleanly if not (e.g. boss died same tick).
        if ctx.db.entity().entity_id().find(&m.boss_entity).is_none() {
            log::warn!(
                "encounter_add: boss_entity {} not found; skipping membership for add {}",
                m.boss_entity,
                director_spawn_ids[idx]
            );
            continue;
        }
        // Enforce Spec C rule #4 (dungeon_layer_propagation_contract.md):
        // add and boss must share a visibility layer. Both sides are
        // visible inside this transaction — the add layer was just
        // inserted above, the boss layer is queryable via entity_layer.
        let add_layer = director_spawn_layers[idx];
        let boss_layer = ctx
            .db
            .entity_layer()
            .entity_id()
            .find(&m.boss_entity)
            .map(|r| r.layer);
        match boss_layer {
            Some(bl) if bl == add_layer => {}
            Some(bl) => {
                log::warn!(
                    "encounter_add: layer mismatch (add {} layer {} vs boss {} layer {}); skipping",
                    director_spawn_ids[idx],
                    add_layer,
                    m.boss_entity,
                    bl
                );
                continue;
            }
            None => {
                log::warn!(
                    "encounter_add: boss {} has no entity_layer row; skipping membership for add {}",
                    m.boss_entity,
                    director_spawn_ids[idx]
                );
                continue;
            }
        }
        // Cap tags: ≤ 8 entries, each ≤ 32 bytes UTF-8.
        let tags: Vec<String> = m
            .tags
            .into_iter()
            .filter(|t| {
                if t.len() > 32 {
                    log::warn!("encounter_add: tag exceeds 32 bytes, dropping: {:?}", t);
                    false
                } else {
                    true
                }
            })
            .take(8)
            .collect();

        ctx.db.encounter_add().insert(EncounterAdd {
            add_entity: director_spawn_ids[idx],
            boss_entity: m.boss_entity,
            tags,
            archetype: m.archetype,
            spawned_at_tick: tick_id,
        });
    }

    // Apply interactable state changes (switch toggled, chest opened, etc.)
    for u in interactable_updates {
        if let Some(existing) = ctx.db.interactable_config().entity_id().find(&u.entity_id) {
            ctx.db
                .interactable_config()
                .entity_id()
                .update(InteractableConfig {
                    entity_id: existing.entity_id,
                    interact_kind: existing.interact_kind,
                    linked_entity: existing.linked_entity,
                    script_id: existing.script_id,
                    tags: existing.tags,
                    required_buff: existing.required_buff,
                    required_item: existing.required_item,
                    interact_range: existing.interact_range,
                    puzzle_group: existing.puzzle_group,
                    puzzle_required_count: existing.puzzle_required_count,
                    puzzle_window_ticks: existing.puzzle_window_ticks,
                    state: u.state,
                    body_shape: existing.body_shape,
                });
        }
    }

    // ── Sim diagnostic log entries (warnings from the pipeline) ───────
    // Inserted as event-table rows — auto-deleted after broadcast so there
    // is no persistent accumulation. Stream live with:
    //   spacetime subscribe <module> "SELECT * FROM sim_log"
    for entry in sim_log_entries {
        ctx.db.sim_log().insert(SimLog {
            log_id: 0, // auto_inc
            tick_id,
            level: entry.level,
            message: entry.message,
        });
    }

    // Process boss_phase_updates inline.
    for update in boss_phase_updates {
        if ctx
            .db
            .boss_phase()
            .boss_entity_id()
            .find(&update.boss_entity_id)
            .is_some()
        {
            ctx.db.boss_phase().boss_entity_id().update(BossPhase {
                boss_entity_id: update.boss_entity_id,
                phase: update.phase,
                entered_at_tick: update.entered_at_tick,
            });
        } else {
            ctx.db.boss_phase().insert(BossPhase {
                boss_entity_id: update.boss_entity_id,
                phase: update.phase,
                entered_at_tick: update.entered_at_tick,
            });
        }
    }

    // Process zone_counter_deltas inline.
    //
    // All zone_counter writes go through `upsert_zone_counter` so the
    // logical key (layer, region_x, region_z, counter_name) stays unique
    // across this reducer and the trusted `increment_zone_counter`
    // helper — SpacetimeDB v2 can't express composite uniqueness in
    // storage, so the invariant is enforced here (see Finding #5).
    for delta in zone_counter_deltas {
        upsert_zone_counter(
            ctx,
            delta.layer,
            delta.region_x,
            delta.region_z,
            delta.counter_name,
            delta.delta,
        );
    }

    // ── Advance last_committed_tick cursor (after all mutations) ────────
    // Deferred to the end so the guard at the top is read-only and any
    // mid-reducer failure leaves the cursor unchanged.
    if let Some(mut cfg) = ctx.db.module_config().key().find(0) {
        cfg.last_committed_tick = tick_id;
        ctx.db.module_config().key().update(cfg);
    }

    Ok(())
}

// ── Commit reducer input types ──────────────────────────────────────
// These are the wire types the simulation worker sends when calling
// commit_tick_results. They use SpacetimeType so they are serializable
// across the SpacetimeDB boundary.

/// Diagnostic log entry from the simulation pipeline.
/// level: 1=Info 2=Warn 3=Error
#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct SimLogInput {
    pub level: u8,
    pub message: String,
}

/// Boss phase transition committed inline with `commit_tick_results`.
#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct BossPhaseUpdateInput {
    pub boss_entity_id: u64,
    pub phase: u32,
    pub entered_at_tick: u64,
}

/// Zone counter delta committed inline with `commit_tick_results`.
#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct ZoneCounterDeltaInput {
    pub layer: u32,
    pub region_x: i32,
    pub region_z: i32,
    pub counter_name: String,
    pub delta: f64,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct TransformUpdate {
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
    pub mod_ai_override_kind: Option<u8>,
    pub mod_ai_override_target: Option<u64>,
    pub mod_stealth: Option<bool>,
    pub last_dot_tick: Option<u64>,
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
    pub layer: u32,
    pub npc_config: Option<DirectorNpcConfigInput>,
    pub team_id: Option<u32>,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct DirectorNpcConfigInput {
    pub passive: bool,
    pub no_chase: bool,
    pub ability_ids: Vec<u32>,
    pub leash_radius: f32,
    pub aggro_radius: f32,
    pub body_shape: Option<u8>,
}

/// Encounter-add membership entry emitted by the worker. Pairs with
/// `director_spawns` by `spawn_index`. The reducer validates the index,
/// caps tags, and inserts an `encounter_add` row alongside the spawned
/// entity. See `docs/contracts/spawn_add_membership_contract.md`.
#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct EncounterAddMembershipInput {
    pub spawn_index: u32,
    pub boss_entity: u64,
    pub archetype: String,
    pub tags: Vec<String>,
}

/// Authoritative death-state row produced by the simulation worker.
///
/// Per Phase 8b finalization the worker emits one entry per dying player
/// with killer attribution, layer, and death position resolved from
/// authoritative simulation state. The reducer inserts the row verbatim;
/// it must not derive death state from observed lifecycle transitions.
#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct DeathStateInsertInput {
    pub entity_id: u64,
    pub killer_entity: Option<u64>,
    pub layer: u32,
    pub death_pos_x: f32,
    pub death_pos_y: f32,
    pub death_pos_z: f32,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct LootRollItemInput {
    pub item_id: u32,
    pub quantity: u32,
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct LootRollInput {
    pub corpse_entity: u64,
    pub layer: u32,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
    pub eligible_claimants: Vec<u64>,
    pub claim_window_ticks: u64,
    pub items: Vec<LootRollItemInput>,
}

// ── Worker Registration ─────────────────────────────────────────────
// Register a trusted worker identity that may call commit/cleanup reducers.
// Authorization is module-admin based (`is_module_admin`), not module-identity-only.

#[reducer]
pub fn register_worker(
    ctx: &ReducerContext,
    worker_identity: spacetimedb::Identity,
) -> Result<(), String> {
    if !is_module_admin(ctx) {
        return Err("register_worker may only be invoked by the module admin".into());
    }
    if ctx
        .db
        .trusted_worker()
        .worker_identity()
        .find(&worker_identity)
        .is_some()
    {
        return Err("Worker already registered".into());
    }
    ctx.db.trusted_worker().insert(TrustedWorker {
        worker_identity,
        rls_group: 0,
    });
    log::info!("Registered trusted worker: {:?}", worker_identity);
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
    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Client not registered")?;
    if seq.entity_id != entity_id {
        return Err("Client does not own this entity".into());
    }

    // Find the inventory item
    let inv_item = ctx
        .db
        .player_inventory()
        .owner_entity()
        .filter(&entity_id)
        .find(|r| r.slot_index == inventory_slot)
        .ok_or("No item in that inventory slot")?;

    let equipping_item_id = inv_item.item_id;
    let inv_row_id = inv_item.row_id;

    // Check if equipment slot is already occupied
    let existing_equip = ctx
        .db
        .player_equipment()
        .owner_entity()
        .filter(&entity_id)
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
        entity_id,
        equipping_item_id,
        target_slot
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

    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Client not registered")?;
    if seq.entity_id != entity_id {
        return Err("Client does not own this entity".into());
    }

    // Find the equipped item
    let equip = ctx
        .db
        .player_equipment()
        .owner_entity()
        .filter(&entity_id)
        .find(|r| r.slot == equipment_slot)
        .ok_or("Nothing equipped in that slot")?;

    let item_id = equip.item_id;
    let equip_row_id = equip.row_id;

    // Verify target inventory slot is empty
    let slot_occupied = ctx
        .db
        .player_inventory()
        .owner_entity()
        .filter(&entity_id)
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
        entity_id,
        item_id,
        equipment_slot,
        target_inventory_slot
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

    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Client not registered")?;
    if seq.entity_id != entity_id {
        return Err("Client does not own this entity".into());
    }

    if slot_a == slot_b {
        return Ok(()); // No-op
    }

    let item_a = ctx
        .db
        .player_inventory()
        .owner_entity()
        .filter(&entity_id)
        .find(|r| r.slot_index == slot_a);
    let item_b = ctx
        .db
        .player_inventory()
        .owner_entity()
        .filter(&entity_id)
        .find(|r| r.slot_index == slot_b);

    match (item_a, item_b) {
        (Some(a), Some(b)) => {
            // Swap both entries
            let (a_id, a_row, a_item, a_qty) = (a.row_id, a.slot_index, a.item_id, a.quantity);
            let (b_id, b_row, b_item, b_qty) = (b.row_id, b.slot_index, b.item_id, b.quantity);
            ctx.db.player_inventory().row_id().update(PlayerInventory {
                row_id: a_id,
                owner_entity: entity_id,
                slot_index: a_row,
                item_id: b_item,
                quantity: b_qty,
            });
            ctx.db.player_inventory().row_id().update(PlayerInventory {
                row_id: b_id,
                owner_entity: entity_id,
                slot_index: b_row,
                item_id: a_item,
                quantity: a_qty,
            });
        }
        (Some(a), None) => {
            // Move A to slot B
            ctx.db.player_inventory().row_id().update(PlayerInventory {
                row_id: a.row_id,
                owner_entity: entity_id,
                slot_index: slot_b,
                item_id: a.item_id,
                quantity: a.quantity,
            });
        }
        (None, Some(b)) => {
            // Move B to slot A
            ctx.db.player_inventory().row_id().update(PlayerInventory {
                row_id: b.row_id,
                owner_entity: entity_id,
                slot_index: slot_a,
                item_id: b.item_id,
                quantity: b.quantity,
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
    let slot_occupied = ctx
        .db
        .player_inventory()
        .owner_entity()
        .filter(&entity_id)
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

    log::info!(
        "loot_item: entity={} item={} qty={} slot={}",
        entity_id,
        item_id,
        quantity,
        target_slot
    );
    Ok(())
}

#[reducer]
pub fn claim_loot(
    ctx: &ReducerContext,
    loot_pile_id: u64,
    item_id: u32,
    target_slot: u32,
) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Client not registered")?;
    let entity_id = seq.entity_id;

    let pile = ctx
        .db
        .loot_pile()
        .loot_pile_id()
        .find(&loot_pile_id)
        .ok_or("Loot pile expired or already claimed")?;

    let current_tick = current_committed_tick(ctx);
    if current_tick >= pile.expires_at_tick {
        sweep_expired_loot_piles(ctx, pile.layer, current_tick);
        return Err("Loot pile expired or already claimed".into());
    }

    if !pile.eligible_claimants.contains(&entity_id) {
        return Err("Entity is not eligible for this loot".into());
    }

    let claimant_layer = ctx
        .db
        .entity_layer()
        .entity_id()
        .find(&entity_id)
        .ok_or("Claimant layer is unknown")?;
    if claimant_layer.layer != pile.layer {
        return Err("Entity is not on the loot pile layer".into());
    }

    let claimant_transform = ctx
        .db
        .entity_transform()
        .entity_id()
        .find(&entity_id)
        .ok_or("Claimant position is unknown")?;
    if !loot_claim_in_range(&claimant_transform, &pile) {
        return Err("Loot pile is out of range".into());
    }

    let slot_occupied = ctx
        .db
        .player_inventory()
        .owner_entity()
        .filter(&entity_id)
        .any(|r| r.slot_index == target_slot);
    if slot_occupied {
        return Err("Target inventory slot is occupied".into());
    }

    let item = ctx
        .db
        .loot_pile_item()
        .loot_pile_id()
        .filter(&loot_pile_id)
        .find(|row| row.item_id == item_id)
        .ok_or("Loot item already claimed")?;

    ctx.db.player_inventory().insert(PlayerInventory {
        row_id: 0,
        owner_entity: entity_id,
        slot_index: target_slot,
        item_id: item.item_id,
        quantity: item.quantity,
    });
    ctx.db
        .loot_pile_item()
        .loot_pile_item_id()
        .delete(&item.loot_pile_item_id);

    if !ctx
        .db
        .loot_pile_item()
        .loot_pile_id()
        .filter(&loot_pile_id)
        .any(|_| true)
    {
        ctx.db.loot_pile().loot_pile_id().delete(&loot_pile_id);
    }

    sweep_expired_loot_piles(ctx, pile.layer, current_tick);
    Ok(())
}

fn loot_claim_in_range(transform: &EntityTransform, pile: &LootPile) -> bool {
    let dx = transform.pos_x - pile.pos_x;
    let dy = transform.pos_y - pile.pos_y;
    let dz = transform.pos_z - pile.pos_z;
    let max_distance_sq = CLAIM_LOOT_MAX_DISTANCE_M * CLAIM_LOOT_MAX_DISTANCE_M;
    dx * dx + dy * dy + dz * dz <= max_distance_sq
}

fn current_committed_tick(ctx: &ReducerContext) -> u64 {
    ctx.db
        .module_config()
        .key()
        .find(0)
        .map(|cfg| cfg.last_committed_tick)
        .unwrap_or(0)
}

fn sweep_expired_loot_piles(ctx: &ReducerContext, layer: u32, current_tick: u64) {
    let expired: Vec<u64> = ctx
        .db
        .loot_pile()
        .layer()
        .filter(&layer)
        .filter(|pile| current_tick >= pile.expires_at_tick)
        .map(|pile| pile.loot_pile_id)
        .collect();

    for loot_pile_id in expired {
        delete_loot_pile(ctx, loot_pile_id);
    }
}

fn delete_loot_pile(ctx: &ReducerContext, loot_pile_id: u64) {
    let item_ids: Vec<u64> = ctx
        .db
        .loot_pile_item()
        .loot_pile_id()
        .filter(&loot_pile_id)
        .map(|item| item.loot_pile_item_id)
        .collect();
    for item_id in item_ids {
        ctx.db.loot_pile_item().loot_pile_item_id().delete(&item_id);
    }
    ctx.db.loot_pile().loot_pile_id().delete(&loot_pile_id);
}

/// Accept a pending trade. Placeholder reducer; trade acceptance is not implemented yet.
#[reducer]
pub fn trade_accept(ctx: &ReducerContext, _trade_id: u64) -> Result<(), String> {
    let _caller = ctx.sender();
    Err("Trading system not yet implemented".into())
}

// ── Respawn ─────────────────────────────────────────────────────────
// Re-spawn a dead player at the nearest respawn point after a delay.
// Death state is created automatically by commit_tick_results when
// a player entity transitions to DespawnPending.

/// Default fallback used as a last resort when no authored spawn /
/// respawn point is configured for a layer. Kept as a single named
/// constant so future audits can find every "we gave up" path easily.
const FALLBACK_SPAWN: [f32; 3] = [0.0, 1.0, 0.0];

/// Pick a spawn position from a candidate list.
///
/// * `hint = Some(pos)` → return the candidate with smallest XZ distance
///   to `pos` (used by death → respawn so the player drops at the
///   nearest authored point to where they died).
/// * `hint = None` → return the first entry (deterministic, used by
///   initial spawn / dungeon entry / dungeon exit).
///
/// Returns `None` when `points` is empty so callers can chain into the
/// next layer of the resolver.
fn pick_spawn_from(points: &[[f32; 3]], hint: Option<(f32, f32, f32)>) -> Option<[f32; 3]> {
    if points.is_empty() {
        return None;
    }
    if let Some((hx, _, hz)) = hint {
        return points
            .iter()
            .min_by(|a, b| {
                let da = (a[0] - hx).powi(2) + (a[2] - hz).powi(2);
                let db = (b[0] - hx).powi(2) + (b[2] - hz).powi(2);
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied();
    }
    points.first().copied()
}

/// Parse `data/layers.ron` and return the matching `WorldLayerDef`.
/// Mirrors `load_dungeon_template`'s embedded-RON pattern; called only
/// on the cold path (spawn / respawn / leave_instance).
fn load_world_layer(layer_id: u32) -> Result<game_schema::dungeon::WorldLayerDef, String> {
    use game_schema::dungeon::WorldLayersFile;
    const SRC: &str = include_str!("../../../data/layers.ron");
    let file: WorldLayersFile =
        ron::from_str(SRC).map_err(|e| format!("layers.ron parse error: {e}"))?;
    file.layers
        .into_iter()
        .find(|l| l.layer_id == layer_id)
        .ok_or_else(|| format!("unknown layer_id {layer_id}"))
}

/// Unified spawn resolver for a static layer.
///
/// Resolution order:
/// 1. `RespawnPoint` rows for `layer` (DB-backed, admin-managed).
/// 2. `WorldLayerDef.spawn_points` from `data/layers.ron`.
/// 3. `FALLBACK_SPAWN` (with a warn).
///
/// `hint` carries the player's last known position (death pos, exit
/// origin, …); it influences (1) and (2) via `pick_spawn_from` so the
/// player lands near where they were rather than at a random point.
fn resolve_layer_spawn(
    ctx: &ReducerContext,
    layer: u32,
    hint: Option<(f32, f32, f32)>,
) -> [f32; 3] {
    // Tier 1: DB-backed respawn points.
    let db_points: Vec<[f32; 3]> = ctx
        .db
        .respawn_point()
        .layer()
        .filter(&layer)
        .map(|p| [p.pos_x, p.pos_y, p.pos_z])
        .collect();
    if let Some(pos) = pick_spawn_from(&db_points, hint) {
        return pos;
    }

    // Tier 2: static `WorldLayerDef.spawn_points` from RON.
    match load_world_layer(layer) {
        Ok(def) => {
            if let Some(pos) = pick_spawn_from(&def.spawn_points, hint) {
                return pos;
            }
        }
        Err(e) => {
            log::warn!("resolve_layer_spawn: load_world_layer({layer}) failed: {e}");
        }
    }

    // Tier 3: hardcoded last-resort.
    log::warn!(
        "resolve_layer_spawn: no respawn_point rows or layers.ron spawn_points \
         for layer {layer}; falling back to {:?}",
        FALLBACK_SPAWN
    );
    FALLBACK_SPAWN
}

/// Backwards-compatible wrapper kept so existing call sites do not
/// need to translate the `(f32, f32, f32)` return shape inline.
fn find_nearest_respawn_point(
    ctx: &ReducerContext,
    layer: u32,
    death_pos: Option<(f32, f32, f32)>,
) -> (f32, f32, f32) {
    let pos = resolve_layer_spawn(ctx, layer, death_pos);
    (pos[0], pos[1], pos[2])
}

#[reducer]
pub fn respawn_player(ctx: &ReducerContext) -> Result<(), String> {
    let caller = ctx.sender();

    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Not registered — call spawn_player first")?;

    let entity_id = seq.entity_id;

    let entity = ctx
        .db
        .entity()
        .entity_id()
        .find(&entity_id)
        .ok_or("Entity row not found")?;

    // `respawn_player` doubles as a "self-rescue" reducer: it works both
    // from terminal states (post-death) and from live states. The latter
    // covers cases where a player has fallen out of the map, has gotten
    // stuck on geometry, or otherwise needs to be teleported back to a
    // safe spawn point without going through the death/timer pipeline.
    //
    // The only truly invalid state is `Spawning`, which would race with
    // the worker's pending sync_insert for the same entity.
    if matches!(entity.state, EntityState::Spawning) {
        return Err(format!("Entity is {:?} — already respawning", entity.state));
    }

    let current_tick = ctx
        .db
        .module_config()
        .key()
        .find(0)
        .map(|c| c.next_tick_id)
        .unwrap_or(0);

    // Death-state respawn timer only applies when the player is actually
    // dead. Self-rescue from a live state has no cooldown — if you fall
    // through the world we want recovery to be immediate.
    let death = ctx.db.death_state().entity_id().find(&entity_id);
    let is_alive = !matches!(
        entity.state,
        EntityState::DespawnPending | EntityState::Removed
    );
    if !is_alive {
        if let Some(ref ds) = death {
            if current_tick < ds.respawn_at_tick {
                let remaining = ds.respawn_at_tick - current_tick;
                return Err(format!(
                    "Cannot respawn yet — {} ticks remaining",
                    remaining
                ));
            }
        }
    }

    // Resolve the target layer + spawn point.
    //
    // Priority:
    //   1. Player is in an instance (membership exists)        → dungeon spawn on instance.layer
    //   2. Player is dead (death_state present)                → death.layer + death pos hint
    //   3. Player is alive on a dungeon layer w/o membership   → ESCAPE to layer 0 (orphan
    //      recovery — happens when an LLM-generated edge case
    //      strands a live player on a layer they were never a
    //      member of; without this the client never rehydrates
    //      dungeon walls and the player appears to stand in
    //      empty space surrounded by props/bosses)
    //   4. Player is alive on a static world layer             → resolve_layer_spawn at current pos
    let membership = ctx.db.instance_membership().entity_id().find(&entity_id);
    let live_transform = ctx.db.entity_transform().entity_id().find(&entity_id);
    let live_layer = ctx
        .db
        .entity_layer()
        .entity_id()
        .find(&entity_id)
        .map(|r| r.layer)
        .unwrap_or(0);

    let (layer, respawn_pos) = if let Some(m) = membership.as_ref() {
        // Stay in the dungeon — re-pick the authored spawn point closest
        // to the player's current/death location.
        let inst = ctx
            .db
            .instance()
            .instance_id()
            .find(&m.instance_id)
            .ok_or("Instance row missing for membership")?;
        let hint = death
            .as_ref()
            .map(|d| (d.death_pos_x, d.death_pos_y, d.death_pos_z))
            .or_else(|| live_transform.as_ref().map(|t| (t.pos_x, t.pos_y, t.pos_z)));
        let pos = match load_dungeon_template(&inst.template_id) {
            Ok(t) => pick_spawn_from(&t.spawn_points, hint).unwrap_or(FALLBACK_SPAWN),
            Err(e) => {
                log::warn!(
                    "respawn_player: could not load template '{}': {e}; \
                     falling back to {:?}",
                    inst.template_id,
                    FALLBACK_SPAWN
                );
                FALLBACK_SPAWN
            }
        };
        (inst.layer, (pos[0], pos[1], pos[2]))
    } else if let Some(ref ds) = death {
        // Dead and not in a dungeon — use death.layer if it's still a
        // valid open-world layer, else escape to layer 0.
        let target_layer = if ds.layer < 100 { ds.layer } else { 0 };
        let pos = find_nearest_respawn_point(
            ctx,
            target_layer,
            Some((ds.death_pos_x, ds.death_pos_y, ds.death_pos_z)),
        );
        (target_layer, pos)
    } else if live_layer >= 100 {
        // Orphan dungeon layer with no membership — ESCAPE to layer 0.
        let hint = live_transform.as_ref().map(|t| (t.pos_x, t.pos_y, t.pos_z));
        let pos = find_nearest_respawn_point(ctx, 0, hint);
        log::warn!(
            "respawn_player: entity {entity_id} stranded on dungeon layer {live_layer} \
             without instance_membership — escaping to layer 0"
        );
        (0, pos)
    } else {
        // Live self-rescue on a static world layer.
        let hint = live_transform.as_ref().map(|t| (t.pos_x, t.pos_y, t.pos_z));
        let pos = find_nearest_respawn_point(ctx, live_layer, hint);
        (live_layer, pos)
    };

    // Restore entity state. We drive the worker's respawn pipeline
    // (entity.on_update → sync_insert) by transitioning *into* `Spawning`.
    // This works even when the prior state was `Active` — the worker's
    // `sync_update` clears stale physics/tactical state (including any
    // active fall arc) before re-inserting at the new pos/layer.
    ctx.db.entity().entity_id().update(Entity {
        entity_id,
        kind: entity.kind,
        state: EntityState::Spawning,
        spawned_at_tick: current_tick,
        owner_identity: entity.owner_identity,
        rls_group: 0,
    });

    // Upsert companion rows: on DespawnPending the old rows still exist
    // (they're only deleted on Removed), so insert-if-missing would keep
    // the corpse position / stale region.  Unconditional upsert is correct.
    let transform_row = EntityTransform {
        entity_id,
        pos_x: respawn_pos.0,
        pos_y: respawn_pos.1,
        pos_z: respawn_pos.2,
        rot_x: 0.0,
        rot_y: 0.0,
        rot_z: 0.0,
        rot_w: 1.0,
        vel_x: 0.0,
        vel_y: 0.0,
        vel_z: 0.0,
        angvel_x: 0.0,
        angvel_y: 0.0,
        angvel_z: 0.0,
        last_tick: current_tick,
        rls_group: 0,
    };
    if ctx
        .db
        .entity_transform()
        .entity_id()
        .find(&entity_id)
        .is_some()
    {
        ctx.db.entity_transform().entity_id().update(transform_row);
    } else {
        ctx.db.entity_transform().insert(transform_row);
    }

    let health_row = EntityHealth {
        entity_id,
        hp: 1000.0,
        max_hp: 1000.0,
        rls_group: 0,
    };
    if ctx
        .db
        .entity_health()
        .entity_id()
        .find(&entity_id)
        .is_some()
    {
        ctx.db.entity_health().entity_id().update(health_row);
    } else {
        ctx.db.entity_health().insert(health_row);
    }

    let region_x = (respawn_pos.0 / 50.0).floor() as i32;
    let region_z = (respawn_pos.2 / 50.0).floor() as i32;
    let region_row = EntityRegion {
        entity_id,
        region_x,
        region_z,
        layer,
    };
    if ctx
        .db
        .entity_region()
        .entity_id()
        .find(&entity_id)
        .is_some()
    {
        ctx.db.entity_region().entity_id().update(region_row);
    } else {
        ctx.db.entity_region().insert(region_row);
    }

    let layer_row = EntityLayer { entity_id, layer };
    if ctx.db.entity_layer().entity_id().find(&entity_id).is_some() {
        ctx.db.entity_layer().entity_id().update(layer_row);
    } else {
        ctx.db.entity_layer().insert(layer_row);
    }

    // If we escaped an orphan dungeon layer (live_layer >= 100, no
    // membership) the membership row is already absent. If we *did* have
    // membership but escaped to layer 0 (e.g. forced via death.layer
    // sanitization above), drop the orphaned membership too so the
    // client geometry layer matches reality.
    if layer == 0 && membership.is_some() {
        ctx.db.instance_membership().entity_id().delete(&entity_id);
    }

    // Clean up death state.
    ctx.db.death_state().entity_id().delete(&entity_id);

    log::info!(
        "Player respawned: entity_id={}, identity={:?}, layer={}, pos=({},{},{}), was_alive={}",
        entity_id,
        caller,
        layer,
        respawn_pos.0,
        respawn_pos.1,
        respawn_pos.2,
        is_alive,
    );
    Ok(())
}

// ── Party System ────────────────────────────────────────────────────
// CRUD reducers for party management. Worker subscribes for team
// awareness. Required for dungeon entry (Phase B).

#[reducer]
pub fn create_party(ctx: &ReducerContext) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    if ctx
        .db
        .party_member()
        .entity_id()
        .filter(&entity_id)
        .next()
        .is_some()
    {
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

    log::info!(
        "Party created: party_id={}, leader={}",
        party.party_id,
        entity_id
    );
    Ok(())
}

#[reducer]
pub fn invite_to_party(ctx: &ReducerContext, target_entity_id: u64) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    let membership = ctx
        .db
        .party_member()
        .entity_id()
        .filter(&entity_id)
        .next()
        .ok_or("Not in a party")?;
    let party = ctx
        .db
        .party()
        .party_id()
        .find(&membership.party_id)
        .ok_or("Party not found")?;
    if party.leader_entity != entity_id {
        return Err("Only the leader can invite".into());
    }

    if ctx
        .db
        .entity()
        .entity_id()
        .find(&target_entity_id)
        .is_none()
    {
        return Err("Target entity does not exist".into());
    }
    if ctx
        .db
        .party_member()
        .entity_id()
        .filter(&target_entity_id)
        .next()
        .is_some()
    {
        return Err("Target is already in a party".into());
    }
    if ctx
        .db
        .party_invite()
        .invitee_entity()
        .filter(&target_entity_id)
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

    log::info!(
        "Party invite: {} invited {} to party {}",
        entity_id,
        target_entity_id,
        party.party_id
    );
    Ok(())
}

#[reducer]
pub fn accept_party_invite(ctx: &ReducerContext, invite_id: u64) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    let invite = ctx
        .db
        .party_invite()
        .invite_id()
        .find(&invite_id)
        .ok_or("Invite not found")?;
    if invite.invitee_entity != entity_id {
        return Err("This invite is not for you".into());
    }
    if invite.expires_at < ctx.timestamp.to_micros_since_unix_epoch() {
        ctx.db.party_invite().invite_id().delete(&invite_id);
        return Err("Invite has expired".into());
    }
    if ctx
        .db
        .party_member()
        .entity_id()
        .filter(&entity_id)
        .next()
        .is_some()
    {
        ctx.db.party_invite().invite_id().delete(&invite_id);
        return Err("Already in a party".into());
    }

    let party = ctx
        .db
        .party()
        .party_id()
        .find(&invite.party_id)
        .ok_or("Party no longer exists")?;
    let member_count = ctx
        .db
        .party_member()
        .party_id()
        .filter(&invite.party_id)
        .count() as u32;
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

    log::info!(
        "Party join: entity {} joined party {}",
        entity_id,
        party.party_id
    );
    Ok(())
}

#[reducer]
pub fn decline_party_invite(ctx: &ReducerContext, invite_id: u64) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Not registered")?;

    let invite = ctx
        .db
        .party_invite()
        .invite_id()
        .find(&invite_id)
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
    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    let membership = ctx
        .db
        .party_member()
        .entity_id()
        .filter(&entity_id)
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
            log::info!(
                "Party {}: leader left, promoted entity {}",
                party_id,
                new_leader
            );
        }
    }

    log::info!("Party leave: entity {} left party {}", entity_id, party_id);
    Ok(())
}

#[reducer]
pub fn kick_from_party(ctx: &ReducerContext, target_entity_id: u64) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    let membership = ctx
        .db
        .party_member()
        .entity_id()
        .filter(&entity_id)
        .next()
        .ok_or("Not in a party")?;
    let party = ctx
        .db
        .party()
        .party_id()
        .find(&membership.party_id)
        .ok_or("Party not found")?;
    if party.leader_entity != entity_id {
        return Err("Only the leader can kick".into());
    }
    if target_entity_id == entity_id {
        return Err("Cannot kick yourself — use leave_party".into());
    }

    let target = ctx
        .db
        .party_member()
        .entity_id()
        .filter(&target_entity_id)
        .find(|m| m.party_id == party.party_id)
        .ok_or("Target is not in your party")?;

    ctx.db.party_member().member_id().delete(&target.member_id);

    log::info!(
        "Party kick: {} kicked {} from party {}",
        entity_id,
        target_entity_id,
        party.party_id
    );
    Ok(())
}

#[reducer]
pub fn disband_party(ctx: &ReducerContext) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    let membership = ctx
        .db
        .party_member()
        .entity_id()
        .filter(&entity_id)
        .next()
        .ok_or("Not in a party")?;
    let party = ctx
        .db
        .party()
        .party_id()
        .find(&membership.party_id)
        .ok_or("Party not found")?;
    if party.leader_entity != entity_id {
        return Err("Only the leader can disband".into());
    }

    let party_id = party.party_id;
    ctx.db.party_member().party_id().delete(&party_id);
    ctx.db.party_invite().party_id().delete(&party_id);
    ctx.db.party().party_id().delete(&party_id);

    log::info!(
        "Party disbanded: party_id={} by leader={}",
        party_id,
        entity_id
    );
    Ok(())
}

// ── World Management ────────────────────────────────────────────────
// Admin reducers for respawn points and trusted-worker reducers for
// boss phase / zone counter updates (ADR-0002 Tier 1).

/// Insert a `RespawnPoint` row that the unified spawn resolver
/// (`resolve_layer_spawn`) will pick up for the given layer. Intended
/// for live admin / CLI use (e.g. `spacetime call ... add_respawn_point
/// 0 10 1 -5 "town_north"`); the module itself does not invoke this —
/// when no DB rows exist the resolver falls back to
/// `WorldLayerDef.spawn_points` from `data/layers.ron`.
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
        point.point_id,
        name,
        layer,
        pos_x,
        pos_y,
        pos_z
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

    upsert_zone_counter(ctx, layer, region_x, region_z, counter_name, delta);

    Ok(())
}

// ── Instance Management ─────────────────────────────────────────────
// Dungeon instancing lifecycle. Layers 0 = open world, 1–99 reserved,
// 100+ dynamic instances. Cross-layer transfers are reducer-driven:
// source worker drops entity via subscription, destination picks it up.

/// Grace period before a disconnected player is removed from an instance (2 min in microseconds).
const INSTANCE_DISCONNECT_GRACE_MICROS: i64 = 120_000_000;
/// Grace period before ALL-disconnected instance is expired (5 min in microseconds).
const INSTANCE_ALL_DISCONNECT_GRACE_MICROS: i64 = 300_000_000;

#[derive(Default, Debug, Clone, Copy)]
struct ExpireReport {
    expired_instances: usize,
    returned_members: usize,
    stale_members: usize,
    removed_entities: usize,
    cleared_buffs: usize,
    cleared_interactables: usize,
    cleared_counters: usize,
    cleared_phases: usize,
    cleared_world_events: usize,
}

impl ExpireReport {
    fn has_activity(&self) -> bool {
        self.expired_instances > 0
            || self.returned_members > 0
            || self.stale_members > 0
            || self.removed_entities > 0
            || self.cleared_buffs > 0
            || self.cleared_interactables > 0
            || self.cleared_counters > 0
            || self.cleared_phases > 0
            || self.cleared_world_events > 0
    }
}

fn interact_kind_from_def(kind: &game_schema::dungeon::InteractKindDef) -> InteractKind {
    match kind {
        game_schema::dungeon::InteractKindDef::Gate => InteractKind::Gate,
        game_schema::dungeon::InteractKindDef::Switch => InteractKind::Switch,
        game_schema::dungeon::InteractKindDef::Chest => InteractKind::Chest,
        game_schema::dungeon::InteractKindDef::BossSpawn { .. } => InteractKind::BossSpawn,
        game_schema::dungeon::InteractKindDef::NpcSpawn { .. } => InteractKind::NpcSpawn,
    }
}

fn default_body_shape_for_interact_kind(kind: InteractKind) -> game_schema::dungeon::BodyShapeDef {
    match kind {
        InteractKind::Gate => game_schema::dungeon::BodyShapeDef::GateCuboid,
        InteractKind::Switch => game_schema::dungeon::BodyShapeDef::SwitchCuboid,
        InteractKind::Chest => game_schema::dungeon::BodyShapeDef::ChestCuboid,
        InteractKind::Grab => game_schema::dungeon::BodyShapeDef::CrateCuboid,
        InteractKind::BossSpawn => game_schema::dungeon::BodyShapeDef::BossCapsule,
        InteractKind::NpcSpawn => game_schema::dungeon::BodyShapeDef::NpcCapsule,
    }
}

fn body_shape_allowed_for_interact_kind(
    kind: InteractKind,
    shape: game_schema::dungeon::BodyShapeDef,
) -> bool {
    use game_schema::dungeon::BodyShapeDef as Shape;

    match kind {
        InteractKind::BossSpawn => matches!(shape, Shape::BossCapsule | Shape::LargeBossCapsule),
        InteractKind::NpcSpawn => matches!(shape, Shape::NpcCapsule),
        InteractKind::Gate | InteractKind::Switch | InteractKind::Chest | InteractKind::Grab => {
            matches!(
                shape,
                Shape::GateCuboid | Shape::SwitchCuboid | Shape::ChestCuboid | Shape::CrateCuboid
            )
        }
    }
}

fn expected_body_shape_family(kind: InteractKind) -> &'static str {
    match kind {
        InteractKind::BossSpawn => "BossCapsule or LargeBossCapsule",
        InteractKind::NpcSpawn => "NpcCapsule",
        InteractKind::Gate | InteractKind::Switch | InteractKind::Chest | InteractKind::Grab => {
            "a cuboid prop shape"
        }
    }
}

fn resolve_body_shape_for_interact_kind(
    kind: InteractKind,
    authored_shape: Option<game_schema::dungeon::BodyShapeDef>,
) -> Result<u8, String> {
    let shape = authored_shape.unwrap_or_else(|| default_body_shape_for_interact_kind(kind));
    if body_shape_allowed_for_interact_kind(kind, shape) {
        Ok(shape.to_u8())
    } else {
        Err(format!(
            "invalid body_shape {shape:?} for {kind:?}; expected {}",
            expected_body_shape_family(kind)
        ))
    }
}

fn validate_interactable_body_shapes(
    template: &game_schema::dungeon::DungeonTemplate,
) -> Result<(), String> {
    for def in &template.interactables {
        let kind = interact_kind_from_def(&def.kind);
        resolve_body_shape_for_interact_kind(kind, def.body_shape).map_err(|e| {
            format!(
                "template '{}' interactable local_id={} {e}",
                template.template_id, def.local_id
            )
        })?;
    }
    Ok(())
}

fn delete_removed_entity_companion_rows(ctx: &ReducerContext, entity_id: u64) {
    ctx.db.entity_transform().entity_id().delete(&entity_id);
    ctx.db.entity_region().entity_id().delete(&entity_id);
    ctx.db.entity_layer().entity_id().delete(&entity_id);
    ctx.db.entity_health().entity_id().delete(&entity_id);
    ctx.db.player_intent().entity_id().delete(&entity_id);
    ctx.db.npc_state().entity_id().delete(&entity_id);
    ctx.db.npc_config().entity_id().delete(&entity_id);
    ctx.db.stealthed_entity().entity_id().delete(&entity_id);
    ctx.db.entity_team().entity_id().delete(&entity_id);
    ctx.db.boss_phase().boss_entity_id().delete(&entity_id);
    ctx.db.npc_goal().entity_id().delete(&entity_id);
    ctx.db.instance_membership().entity_id().delete(&entity_id);
    ctx.db.interactable_config().entity_id().delete(&entity_id);
    ctx.db.encounter_add().add_entity().delete(&entity_id);
    let orphan_ids: Vec<u64> = ctx
        .db
        .encounter_add()
        .by_boss()
        .filter(&entity_id)
        .map(|r| r.add_entity)
        .collect();
    for add_id in orphan_ids {
        ctx.db.encounter_add().add_entity().delete(&add_id);
    }
}

/// Embedded dungeon templates are parsed per reducer invocation.
/// Instance creation is infrequent enough that this has not required caching so far.
fn load_dungeon_template(
    template_id: &str,
) -> Result<game_schema::dungeon::DungeonTemplate, String> {
    use game_schema::dungeon::DungeonFile;
    const SRC: &str = include_str!("../../../data/dungeons.ron");
    let file: DungeonFile =
        ron::from_str(SRC).map_err(|e| format!("dungeons.ron parse error: {e}"))?;
    file.templates
        .into_iter()
        .find(|t| t.template_id == template_id)
        .ok_or_else(|| format!("unknown template_id '{template_id}'"))
}

fn load_npc_archetypes()
-> Result<std::collections::HashMap<String, game_schema::NpcArchetype>, String> {
    // Shared validator: same code path the simulation worker runs at startup
    // (`NpcArchetypeRegistry::with_builtins` → `parse_and_validate`).
    // Authoring bugs that the worker would warn-and-empty here become a
    // create_instance failure, which is what we want: a server actor row must
    // never reference an archetype the worker would silently disagree about.
    const SRC: &str = include_str!("../../../data/npc_archetypes.ron");
    let file = game_schema::NpcArchetypeFile::parse_and_validate(SRC)?;
    Ok(file.archetypes)
}

struct ResolvedActorSpawnConfig {
    kind: EntityKind,
    max_hp: f32,
    team_id: Option<u32>,
    archetype_id: Option<String>,
    ability_ids: [Option<u32>; 4],
    body_shape: Option<u8>,
}

fn resolve_actor_spawn_config(
    kind: &game_schema::dungeon::InteractKindDef,
    body_shape: Option<game_schema::dungeon::BodyShapeDef>,
    archetypes: &std::collections::HashMap<String, game_schema::NpcArchetype>,
) -> Result<Option<ResolvedActorSpawnConfig>, String> {
    let (expected_kind, interact_kind, fallback_hp, archetype_id, owner) = match kind {
        game_schema::dungeon::InteractKindDef::BossSpawn {
            npc_name,
            archetype_id,
            ..
        } => (
            EntityKind::Boss,
            InteractKind::BossSpawn,
            1000.0_f32,
            archetype_id.as_deref(),
            format!("BossSpawn '{npc_name}'"),
        ),
        game_schema::dungeon::InteractKindDef::NpcSpawn {
            npc_name,
            archetype_id,
        } => (
            EntityKind::Npc,
            InteractKind::NpcSpawn,
            80.0_f32,
            archetype_id.as_deref(),
            format!("NpcSpawn '{npc_name}'"),
        ),
        _ => return Ok(None),
    };

    let mut max_hp = fallback_hp;
    let mut team_id = None;
    let mut ability_ids = [None; 4];
    let mut resolved_body_shape = body_shape;
    let mut resolved_archetype_id = None;

    if let Some(archetype_id) = archetype_id.filter(|id| !id.is_empty()) {
        let archetype = archetypes
            .get(archetype_id)
            .ok_or_else(|| format!("{owner} references unknown archetype_id '{archetype_id}'"))?;
        if !archetype.usage.allows_actor_spawn() {
            return Err(format!(
                "{owner} references archetype_id '{archetype_id}' with usage {:?}; actor spawns require Both or ActorOnly",
                archetype.usage
            ));
        }
        if archetype.kind != expected_kind {
            return Err(format!(
                "{owner} references archetype_id '{archetype_id}' with kind {:?}; expected {:?}",
                archetype.kind, expected_kind
            ));
        }
        if archetype.max_hp <= 0.0 {
            return Err(format!(
                "{owner} references archetype_id '{archetype_id}' with non-positive max_hp {}",
                archetype.max_hp
            ));
        }
        max_hp = archetype.max_hp;
        team_id = archetype.team_id;
        ability_ids = ability_id_slots(&archetype.ability_ids);
        resolved_body_shape = body_shape.or(archetype.body_shape);
        resolved_archetype_id = Some(archetype_id.to_string());
    }

    Ok(Some(ResolvedActorSpawnConfig {
        kind: expected_kind,
        max_hp,
        team_id,
        archetype_id: resolved_archetype_id,
        ability_ids,
        body_shape: Some(resolve_body_shape_for_interact_kind(
            interact_kind,
            resolved_body_shape,
        )?),
    }))
}

fn ability_id_slots(ability_ids: &[u32]) -> [Option<u32>; 4] {
    let mut slots = [None; 4];
    for (slot, ability_id) in slots.iter_mut().zip(ability_ids.iter().copied()) {
        *slot = Some(ability_id);
    }
    slots
}

/// Clear all `active_buff` rows for an entity (and the derived
/// `stealthed_entity` row).
///
/// Used by `leave_instance` and `expire_instances_inner` to prevent
/// encounter-scoped buffs (notably the `MechanicLocked` Manaya marks
/// 800/801/803) from following players out of an instance or lingering on
/// reducer-removed instance NPCs. The reducer module has no access to
/// `BuffRegistry`, so we cannot distinguish encounter-scoped from open-world
/// buffs here; an instance exit/removal unconditionally resets buff state,
/// matching the typical MMO convention.
fn clear_entity_buffs(ctx: &ReducerContext, entity_id: u64) -> usize {
    let buff_ids: Vec<u64> = ctx
        .db
        .active_buff()
        .entity_id()
        .filter(&entity_id)
        .map(|b| b.buff_instance_id)
        .collect();
    let n = buff_ids.len();
    for bid in buff_ids {
        ctx.db.active_buff().buff_instance_id().delete(&bid);
    }
    // `stealthed_entity` is derived from `ActiveBuff.mod_stealth`; with all
    // buffs gone the row must also go or stealth would persist visibly.
    ctx.db.stealthed_entity().entity_id().delete(&entity_id);
    n
}

/// Atomically reposition an already-existing entity to `(spawn_point, layer)`.
///
/// Overwrites `EntityTransform` (pos, zeroed velocities), `EntityRegion`
/// (recomputed from spawn_point XZ) and `EntityLayer`. `Entity` row is
/// left alone — this is used for live entities, not first-time spawn.
/// The Y written here is advisory: the sim_worker coordinator snaps the
/// character to terrain via `raycast_surface` on the new layer before the
/// next physics tick (see `crates/simulation_worker/src/coordinator.rs`
/// `entity_layer.on_update`).
fn reposition_entity_to_spawn(
    ctx: &ReducerContext,
    entity_id: u64,
    spawn_point: [f32; 3],
    layer: u32,
) {
    // `next_tick_id` names the upcoming tick the worker will commit. Other
    // writers of `EntityTransform.last_tick` stamp the tick the snapshot
    // belongs to (i.e. the last *completed* tick), so mirror that here:
    // otherwise this row looks one tick fresher than any worker-produced
    // transform and stale-gate comparisons ("is this input newer than the
    // last committed transform?") flip their ordering.
    let last_committed_tick = ctx
        .db
        .module_config()
        .key()
        .find(0)
        .map(|c| c.next_tick_id.saturating_sub(1))
        .unwrap_or(0);

    let transform_row = EntityTransform {
        entity_id,
        pos_x: spawn_point[0],
        pos_y: spawn_point[1],
        pos_z: spawn_point[2],
        rot_x: 0.0,
        rot_y: 0.0,
        rot_z: 0.0,
        rot_w: 1.0,
        vel_x: 0.0,
        vel_y: 0.0,
        vel_z: 0.0,
        angvel_x: 0.0,
        angvel_y: 0.0,
        angvel_z: 0.0,
        last_tick: last_committed_tick,
        rls_group: 0,
    };
    if ctx
        .db
        .entity_transform()
        .entity_id()
        .find(&entity_id)
        .is_some()
    {
        ctx.db.entity_transform().entity_id().update(transform_row);
    } else {
        ctx.db.entity_transform().insert(transform_row);
    }

    let region_x = (spawn_point[0] / 50.0).floor() as i32;
    let region_z = (spawn_point[2] / 50.0).floor() as i32;
    let region_row = EntityRegion {
        entity_id,
        region_x,
        region_z,
        layer,
    };
    if ctx
        .db
        .entity_region()
        .entity_id()
        .find(&entity_id)
        .is_some()
    {
        ctx.db.entity_region().entity_id().update(region_row);
    } else {
        ctx.db.entity_region().insert(region_row);
    }

    let layer_row = EntityLayer { entity_id, layer };
    if ctx.db.entity_layer().entity_id().find(&entity_id).is_some() {
        ctx.db.entity_layer().entity_id().update(layer_row);
    } else {
        ctx.db.entity_layer().insert(layer_row);
    }
}

#[reducer]
pub fn create_instance(
    ctx: &ReducerContext,
    template_id: String,
    max_players: u32,
) -> Result<(), String> {
    if !is_trusted_caller(ctx) && !is_module_admin(ctx) && !is_debug_caller(ctx) {
        return Err("create_instance: trusted caller only".into());
    }

    let template = load_dungeon_template(&template_id)?;
    validate_interactable_body_shapes(&template)?;
    let npc_archetypes = load_npc_archetypes()?;

    let mut cfg = ctx
        .db
        .module_config()
        .key()
        .find(0)
        .ok_or("ModuleConfig not found")?;
    let layer = cfg.next_instance_layer;
    cfg.next_instance_layer = layer + 1;
    ctx.db.module_config().key().update(cfg);

    let current_tick = ctx
        .db
        .sim_tick()
        .iter()
        .max_by_key(|t| t.tick_id)
        .map(|t| t.tick_id)
        .unwrap_or(0);

    let now = ctx.timestamp.to_micros_since_unix_epoch();
    // Default expiry: 2 hours.
    let expires_at = now + 7_200_000_000;

    let inst = ctx.db.instance().insert(Instance {
        instance_id: 0,
        template_id: template_id.clone(),
        layer,
        layer_group: 0,
        state: InstanceState::Active,
        created_at: now,
        expires_at,
        max_players,
    });

    // ── Pass 2: spawn interactable entities ─────────────────────────
    // Maps template-scoped local_id → real entity_id for linked_to resolution.
    let mut local_to_entity: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();

    // First pass: create all prop/NPC/boss entities (so we have real IDs).
    for def in &template.interactables {
        let actor_spawn = resolve_actor_spawn_config(&def.kind, def.body_shape, &npc_archetypes)?;
        let (entity_kind, hp, max_hp) = actor_spawn
            .as_ref()
            .map(|cfg| (cfg.kind, cfg.max_hp, cfg.max_hp))
            .unwrap_or((EntityKind::Prop, 1.0_f32, 1.0_f32));
        let entity = ctx.db.entity().insert(Entity {
            entity_id: 0,
            kind: entity_kind,
            state: EntityState::Spawning,
            spawned_at_tick: current_tick,
            owner_identity: None,
            rls_group: 0,
        });
        let eid = entity.entity_id;
        local_to_entity.insert(def.local_id, eid);

        ctx.db.entity_transform().insert(EntityTransform {
            entity_id: eid,
            pos_x: def.position[0],
            pos_y: def.position[1],
            pos_z: def.position[2],
            rot_x: 0.0,
            rot_y: 0.0,
            rot_z: 0.0,
            rot_w: 1.0,
            vel_x: 0.0,
            vel_y: 0.0,
            vel_z: 0.0,
            angvel_x: 0.0,
            angvel_y: 0.0,
            angvel_z: 0.0,
            last_tick: current_tick,
            rls_group: 0,
        });

        ctx.db.entity_health().insert(EntityHealth {
            entity_id: eid,
            hp,
            max_hp,
            rls_group: 0,
        });

        ctx.db.entity_region().insert(EntityRegion {
            entity_id: eid,
            region_x: (def.position[0] / 50.0).floor() as i32,
            region_z: (def.position[2] / 50.0).floor() as i32,
            layer,
        });

        ctx.db.entity_layer().insert(EntityLayer {
            entity_id: eid,
            layer,
        });

        if let Some(team_id) = actor_spawn.as_ref().and_then(|cfg| cfg.team_id) {
            ctx.db.entity_team().insert(EntityTeam {
                entity_id: eid,
                team_id,
            });
        }

        // BossSpawn/NpcSpawn entries are actors, not switch-like props. They
        // get NpcConfig so the worker configures AI and character collision.
        match &def.kind {
            game_schema::dungeon::InteractKindDef::BossSpawn {
                npc_name,
                encounter_name,
                archetype_id,
            } => {
                let Some(actor_spawn) = actor_spawn.as_ref() else {
                    continue;
                };
                let encounter_key = encounter_name
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| npc_name.clone());
                ctx.db.npc_config().insert(NpcConfig {
                    entity_id: eid,
                    encounter_name: Some(encounter_key.clone()),
                    archetype_id: actor_spawn.archetype_id.clone(),
                    passive: false,
                    no_chase: false,
                    ability_id_1: actor_spawn.ability_ids[0],
                    ability_id_2: actor_spawn.ability_ids[1],
                    ability_id_3: actor_spawn.ability_ids[2],
                    ability_id_4: actor_spawn.ability_ids[3],
                    leash_radius: 30.0,
                    aggro_radius: 15.0,
                    body_shape: actor_spawn.body_shape,
                });
                log::info!(
                    "Boss entity {} ({npc_name}) spawned in instance layer={layer} encounter={encounter_key} archetype={:?}",
                    eid,
                    archetype_id,
                );
            }
            game_schema::dungeon::InteractKindDef::NpcSpawn {
                npc_name,
                archetype_id,
            } => {
                let Some(actor_spawn) = actor_spawn.as_ref() else {
                    continue;
                };
                ctx.db.npc_config().insert(NpcConfig {
                    entity_id: eid,
                    encounter_name: None,
                    archetype_id: actor_spawn.archetype_id.clone(),
                    passive: false,
                    no_chase: false,
                    ability_id_1: actor_spawn.ability_ids[0],
                    ability_id_2: actor_spawn.ability_ids[1],
                    ability_id_3: actor_spawn.ability_ids[2],
                    ability_id_4: actor_spawn.ability_ids[3],
                    leash_radius: 30.0,
                    aggro_radius: 15.0,
                    body_shape: actor_spawn.body_shape,
                });
                log::info!(
                    "NPC entity {} ({npc_name}) spawned in instance layer={layer} archetype={:?}",
                    eid,
                    archetype_id,
                );
            }
            _ => {}
        }
    }

    // Second pass: create interactable_config rows with resolved linked_entity IDs.
    for def in &template.interactables {
        let eid = local_to_entity[&def.local_id];
        let linked_entity = def
            .linked_to
            .and_then(|lid| local_to_entity.get(&lid).copied());
        let interact_kind = interact_kind_from_def(&def.kind);

        ctx.db.interactable_config().insert(InteractableConfig {
            entity_id: eid,
            interact_kind,
            linked_entity,
            script_id: def.script_id.clone(),
            tags: def.tags.clone(),
            required_buff: def.required_buff,
            required_item: def.required_item,
            interact_range: def.interact_range.unwrap_or(3.0),
            puzzle_group: def.puzzle_group.clone(),
            puzzle_required_count: def.puzzle_required_count.unwrap_or(0),
            puzzle_window_ticks: def.puzzle_window_ticks.unwrap_or(0),
            state: InteractState::Idle,
            body_shape: resolve_body_shape_for_interact_kind(interact_kind, def.body_shape)?,
        });
    }

    log::info!(
        "Instance created: id={} template={} layer={} props={} max_players={}",
        inst.instance_id,
        template_id,
        layer,
        template.interactables.len(),
        max_players
    );
    Ok(())
}

#[reducer]
pub fn join_instance(ctx: &ReducerContext, instance_id: u64) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    let instance = ctx
        .db
        .instance()
        .instance_id()
        .find(&instance_id)
        .ok_or("Instance not found")?;
    if instance.state != InstanceState::Active && instance.state != InstanceState::Pending {
        return Err(format!("Instance is {:?} — cannot join", instance.state));
    }

    if ctx
        .db
        .instance_membership()
        .entity_id()
        .find(&entity_id)
        .is_some()
    {
        return Err("Already in an instance".into());
    }

    // Check party requirement (must be in a party to enter dungeons).
    if ctx
        .db
        .party_member()
        .entity_id()
        .filter(&entity_id)
        .next()
        .is_none()
    {
        return Err("Must be in a party to enter an instance".into());
    }

    let member_count = ctx
        .db
        .instance_membership()
        .instance_id()
        .filter(&instance_id)
        .count() as u32;
    if member_count >= instance.max_players {
        return Err("Instance is full".into());
    }

    // Create membership.
    ctx.db.instance_membership().insert(InstanceMembership {
        entity_id,
        instance_id,
        disconnect_at: None,
    });

    // Atomically move entity to instance layer + spawn point. Without the
    // transform write the character would stay at its open-world XZ on the
    // new layer (see the worker-side reconcile in coordinator.rs which
    // raycast-snaps the Y to the instance's terrain).
    let spawn_point = match load_dungeon_template(&instance.template_id) {
        Ok(t) => pick_spawn_from(&t.spawn_points, None).unwrap_or(FALLBACK_SPAWN),
        Err(e) => {
            log::warn!(
                "join_instance: could not load template '{}' for spawn_points: {e}; \
                 falling back to {:?}",
                instance.template_id,
                FALLBACK_SPAWN
            );
            FALLBACK_SPAWN
        }
    };
    reposition_entity_to_spawn(ctx, entity_id, spawn_point, instance.layer);

    log::info!(
        "Instance join: entity {} joined instance {} (layer {}) at spawn ({:.1},{:.1},{:.1})",
        entity_id,
        instance_id,
        instance.layer,
        spawn_point[0],
        spawn_point[1],
        spawn_point[2]
    );
    Ok(())
}

#[reducer]
pub fn leave_instance(ctx: &ReducerContext) -> Result<(), String> {
    let caller = ctx.sender();
    let seq = ctx
        .db
        .client_sequence()
        .client_identity()
        .find(&caller)
        .ok_or("Not registered")?;
    let entity_id = seq.entity_id;

    let membership = ctx
        .db
        .instance_membership()
        .entity_id()
        .find(&entity_id)
        .ok_or("Not in an instance")?;
    let instance_id = membership.instance_id;

    // Capture template_id before deleting the membership row (we still
    // want to honour the dungeon's authored exit_points even though the
    // player is no longer a member).
    let template_id = ctx
        .db
        .instance()
        .instance_id()
        .find(&instance_id)
        .map(|i| i.template_id);

    // Remove membership.
    ctx.db.instance_membership().entity_id().delete(&entity_id);

    // Clear encounter-scoped buff state so MechanicLocked marks (e.g.
    // Manaya Core marks 800/801/803) cannot ride out with the player.
    let cleared = clear_entity_buffs(ctx, entity_id);
    if cleared > 0 {
        log::debug!("leave_instance: cleared {cleared} active_buff rows for entity {entity_id}");
    }

    // Resolve exit destination on layer 0:
    //   1. `DungeonTemplate.exit_points` (designer-authored), if any.
    //   2. Open-world layer's spawn points / RespawnPoint rows via
    //      `resolve_layer_spawn`.
    //   3. `FALLBACK_SPAWN` (logged inside the resolver).
    let exit_pos = template_id
        .as_deref()
        .and_then(|tid| match load_dungeon_template(tid) {
            Ok(t) => pick_spawn_from(&t.exit_points, None),
            Err(e) => {
                log::warn!("leave_instance: could not load template '{tid}' for exit_points: {e}");
                None
            }
        })
        .unwrap_or_else(|| resolve_layer_spawn(ctx, 0, None));

    reposition_entity_to_spawn(ctx, entity_id, exit_pos, 0);

    log::info!(
        "Instance leave: entity {} returned to open world at ({:.1},{:.1},{:.1})",
        entity_id,
        exit_pos[0],
        exit_pos[1],
        exit_pos[2]
    );
    Ok(())
}

/// Scheduled cleanup: expire timed-out instances and remove long-disconnected members.
/// Called by admin or trusted worker; `world_clock` also reuses the same inner
/// maintenance path before evaluating zone counters.
#[reducer]
pub fn expire_instances(ctx: &ReducerContext) -> Result<(), String> {
    if !is_trusted_caller(ctx) && !is_module_admin(ctx) && !is_debug_caller(ctx) {
        return Err("expire_instances: trusted caller only".into());
    }

    let now = ctx.timestamp.to_micros_since_unix_epoch();
    let report = expire_instances_inner(ctx, now)?;
    if report.has_activity() {
        log::info!(
            "expire_instances: expired={} returned_members={} stale_members={} removed_entities={} cleared_buffs={} cleared_interactables={} cleared_counters={} cleared_phases={} cleared_world_events={}",
            report.expired_instances,
            report.returned_members,
            report.stale_members,
            report.removed_entities,
            report.cleared_buffs,
            report.cleared_interactables,
            report.cleared_counters,
            report.cleared_phases,
            report.cleared_world_events,
        );
    }
    Ok(())
}

fn expire_instances_inner(ctx: &ReducerContext, now: i64) -> Result<ExpireReport, String> {
    let mut report = ExpireReport::default();

    let completed_layers: Vec<u32> = ctx
        .db
        .instance()
        .iter()
        .filter(|i| i.state == InstanceState::Active || i.state == InstanceState::Pending)
        .filter(|i| layer_has_completed_world_phase(ctx, i.layer))
        .map(|i| i.layer)
        .collect();
    for layer in completed_layers {
        mark_instance_completed_for_layer(ctx, layer);
    }

    // Collect instances to expire (time-based).
    let expired: Vec<u64> = ctx
        .db
        .instance()
        .iter()
        .filter(|i| {
            (i.state == InstanceState::Active
                || i.state == InstanceState::Pending
                || i.state == InstanceState::Completed)
                && i.expires_at < now
        })
        .map(|i| i.instance_id)
        .collect();

    let completed_empty: Vec<u64> = ctx
        .db
        .instance()
        .iter()
        .filter(|i| i.state == InstanceState::Completed)
        .filter(|i| !expired.contains(&i.instance_id))
        .filter(|i| {
            ctx.db
                .instance_membership()
                .instance_id()
                .filter(&i.instance_id)
                .next()
                .is_none()
        })
        .map(|i| i.instance_id)
        .collect();

    // Check instances where ALL members are disconnected beyond grace period.
    let all_disconnected: Vec<u64> = ctx
        .db
        .instance()
        .iter()
        .filter(|i| {
            i.state == InstanceState::Active
                || i.state == InstanceState::Pending
                || i.state == InstanceState::Completed
        })
        .filter(|i| !expired.contains(&i.instance_id))
        .filter(|i| !completed_empty.contains(&i.instance_id))
        .filter(|i| {
            let members: Vec<_> = ctx
                .db
                .instance_membership()
                .instance_id()
                .filter(&i.instance_id)
                .collect();
            !members.is_empty()
                && members.iter().all(|m| {
                    m.disconnect_at
                        .is_some_and(|d| now - d > INSTANCE_ALL_DISCONNECT_GRACE_MICROS)
                })
        })
        .map(|i| i.instance_id)
        .collect();

    let to_expire: Vec<u64> = expired
        .into_iter()
        .chain(completed_empty)
        .chain(all_disconnected)
        .collect();
    report.expired_instances = to_expire.len();

    for instance_id in &to_expire {
        // Remove all memberships — return members to open world.
        let members: Vec<u64> = ctx
            .db
            .instance_membership()
            .instance_id()
            .filter(instance_id)
            .map(|m| m.entity_id)
            .collect();
        for eid in &members {
            ctx.db.instance_membership().entity_id().delete(eid);
            // Clear encounter-scoped buffs (MechanicLocked marks etc.) so they
            // don't survive instance expiry. Mirrors `leave_instance`.
            let cleared = clear_entity_buffs(ctx, *eid);
            report.cleared_buffs += cleared;
            if cleared > 0 {
                log::debug!(
                    "expire_instances: cleared {cleared} active_buff rows for entity {eid}"
                );
            }
            if let Some(er) = ctx.db.entity_region().entity_id().find(eid) {
                ctx.db.entity_region().entity_id().update(EntityRegion {
                    entity_id: *eid,
                    region_x: er.region_x,
                    region_z: er.region_z,
                    layer: 0,
                });
            }
            ctx.db.entity_layer().entity_id().update(EntityLayer {
                entity_id: *eid,
                layer: 0,
            });
        }
        report.returned_members += members.len();

        // Mark instance expired.
        let instance_layer = if let Some(inst) = ctx.db.instance().instance_id().find(instance_id) {
            let instance_layer = inst.layer;
            ctx.db.instance().instance_id().update(Instance {
                instance_id: *instance_id,
                template_id: inst.template_id,
                layer: instance_layer,
                layer_group: inst.layer_group,
                state: InstanceState::Expired,
                created_at: inst.created_at,
                expires_at: inst.expires_at,
                max_players: inst.max_players,
            });
            instance_layer
        } else {
            0
        };

        // Clean up interactable configs on the instance layer immediately so
        // clients and the worker do not keep targeting stale gates, switches,
        // chests, or spawn markers while entity removal drains asynchronously.
        let instance_entities: Vec<u64> = ctx
            .db
            .entity_region()
            .iter()
            .filter(|er| er.layer == instance_layer)
            .map(|er| er.entity_id)
            .collect();
        let mut interactable_count = 0usize;
        for eid in &instance_entities {
            if ctx.db.interactable_config().entity_id().delete(eid) {
                interactable_count += 1;
            }
        }
        report.cleared_interactables += interactable_count;

        // Entity cleanup (gates, switches, props, NPCs, bosses) is terminal
        // for an expired instance. Remove DB companion rows immediately so
        // entities that never reached worker Phase 8 cannot survive as stale
        // Spawning rows. The worker mirrors Removed updates by force-removing
        // any existing runtime entity.
        let mut removed_count = 0usize;
        let mut removed_buff_count = 0usize;
        for eid in instance_entities {
            if let Some(entity) = ctx.db.entity().entity_id().find(&eid) {
                if entity.kind != EntityKind::Player && entity.state != EntityState::Removed {
                    ctx.db.entity().entity_id().update(Entity {
                        entity_id: eid,
                        kind: entity.kind,
                        state: EntityState::Removed,
                        spawned_at_tick: entity.spawned_at_tick,
                        owner_identity: entity.owner_identity,
                        rls_group: 0,
                    });
                    removed_buff_count += clear_entity_buffs(ctx, eid);
                    delete_removed_entity_companion_rows(ctx, eid);
                    removed_count += 1;
                }
            }
        }
        report.removed_entities += removed_count;
        report.cleared_buffs += removed_buff_count;

        // Delete zone_counter rows for this instance layer so world_clock
        // stops evaluating stale counters and the counter won't pollute a
        // future instance that happens to reuse the same layer number.
        let stale_counters: Vec<u64> = ctx
            .db
            .zone_counter()
            .iter()
            .filter(|zc| zc.layer == instance_layer)
            .map(|zc| zc.counter_id)
            .collect();
        let counter_count = stale_counters.len();
        report.cleared_counters += counter_count;
        for counter_id in stale_counters {
            ctx.db.zone_counter().counter_id().delete(&counter_id);
        }

        // Delete world_phase rows for this instance layer.  zone_id encodes
        // layer in the top bits: layer = zone_id / 1_000_000.
        let stale_phases: Vec<u32> = ctx
            .db
            .world_phase()
            .iter()
            .filter(|wp| wp.zone_id / 1_000_000 == instance_layer)
            .map(|wp| wp.zone_id)
            .collect();
        let phase_count = stale_phases.len();
        report.cleared_phases += phase_count;
        for zone_id in stale_phases {
            ctx.db.world_phase().zone_id().delete(&zone_id);
        }

        // Delete world_activity_event rows for this instance layer so director
        // triggers stop matching against stale events when the instance
        // layer is recycled. See `tables.rs` `WorldActivityEvent` comment.
        let stale_events: Vec<u64> = ctx
            .db
            .world_activity_event()
            .iter()
            .filter(|ev| ev.scope_layer == instance_layer)
            .map(|ev| ev.event_id)
            .collect();
        let world_event_count = stale_events.len();
        report.cleared_world_events += world_event_count;
        for event_id in stale_events {
            ctx.db.world_activity_event().event_id().delete(&event_id);
        }

        log::info!(
            "Instance expired: id={} layer={} (removed {} entities, cleared {} buffs, {} interactables, {} counters, {} phases, {} world_events)",
            instance_id,
            instance_layer,
            removed_count,
            removed_buff_count,
            interactable_count,
            counter_count,
            phase_count,
            world_event_count,
        );
    }

    // Remove individual members disconnected > 2 min (party continues).
    let stale_members: Vec<u64> = ctx
        .db
        .instance_membership()
        .iter()
        .filter(|m| {
            m.disconnect_at
                .is_some_and(|d| now - d > INSTANCE_DISCONNECT_GRACE_MICROS)
        })
        .filter(|m| {
            // Only remove if the instance is still active (not already expired above).
            ctx.db
                .instance()
                .instance_id()
                .find(&m.instance_id)
                .is_some_and(|i| i.state == InstanceState::Active)
        })
        .map(|m| m.entity_id)
        .collect();
    for eid in &stale_members {
        ctx.db.instance_membership().entity_id().delete(eid);
        if let Some(er) = ctx.db.entity_region().entity_id().find(eid) {
            ctx.db.entity_region().entity_id().update(EntityRegion {
                entity_id: *eid,
                region_x: er.region_x,
                region_z: er.region_z,
                layer: 0,
            });
        }
        ctx.db.entity_layer().entity_id().update(EntityLayer {
            entity_id: *eid,
            layer: 0,
        });
        report.stale_members += 1;
        log::info!(
            "Instance: stale member {} removed (disconnect grace expired)",
            eid
        );
    }

    Ok(report)
}

#[derive(spacetimedb::SpacetimeType, Clone, Debug)]
pub struct InteractableUpdate {
    pub entity_id: u64,
    pub state: InteractState,
}

// ── Voxel Terrain Editor Reducers (§4.8b Phase 1) ───────────────────
// Admin/debug-gated upserts for the offline editor pipeline. The worker
// (Phase 5) only READS these tables; nobody mutates terrain at runtime.
//
// Logical uniqueness on `(terrain_set_id, chunk_morton[, voxel_idx])` is
// enforced here (read-then-update or insert) since SpacetimeDB v2 has no
// multi-column `#[unique]`. The `terrain_set` name is unique at the
// storage layer.
//
// `terrain_core_upsert` is intentionally deferred until the voxel editor
// lands — payload is per-voxel and large; no caller exists yet.

/// Insert or update a terrain set by name. Returns the resulting
/// `terrain_set_id` indirectly via the `name` unique index.
#[reducer]
pub fn terrain_set_upsert(
    ctx: &ReducerContext,
    name: String,
    content_hash: String,
    version: u32,
) -> Result<(), String> {
    if !is_module_admin(ctx) && !is_debug_caller(ctx) {
        return Err("terrain_set_upsert: admin only".into());
    }
    if name.is_empty() {
        return Err("terrain_set_upsert: name must not be empty".into());
    }
    if let Some(existing) = ctx.db.terrain_set().name().find(&name) {
        ctx.db.terrain_set().terrain_set_id().update(TerrainSet {
            terrain_set_id: existing.terrain_set_id,
            name,
            content_hash,
            version,
        });
    } else {
        ctx.db.terrain_set().insert(TerrainSet {
            terrain_set_id: 0,
            name,
            content_hash,
            version,
        });
    }
    Ok(())
}

/// Insert or replace one baked terrain chunk. Worker (Phase 5) will
/// react to `on_update` to swap the matching Rapier collider.
#[reducer]
pub fn terrain_chunk_upsert(
    ctx: &ReducerContext,
    terrain_set_id: u32,
    chunk_morton: u64,
    vertices: Vec<f32>,
    indices: Vec<u32>,
    lod: u8,
) -> Result<(), String> {
    if !is_module_admin(ctx) && !is_debug_caller(ctx) {
        return Err("terrain_chunk_upsert: admin only".into());
    }
    if vertices.len() % 3 != 0 {
        return Err("terrain_chunk_upsert: vertices length must be a multiple of 3".into());
    }
    if indices.len() % 3 != 0 {
        return Err("terrain_chunk_upsert: indices length must be a multiple of 3".into());
    }
    let n_verts = (vertices.len() / 3) as u32;
    if let Some(bad) = indices.iter().find(|&&i| i >= n_verts) {
        return Err(format!(
            "terrain_chunk_upsert: index {bad} out of range for {n_verts} vertices",
        ));
    }
    if ctx
        .db
        .terrain_set()
        .terrain_set_id()
        .find(&terrain_set_id)
        .is_none()
    {
        return Err(format!(
            "terrain_chunk_upsert: unknown terrain_set_id {terrain_set_id}",
        ));
    }
    let existing = ctx
        .db
        .terrain_chunk()
        .by_set_chunk()
        .filter((terrain_set_id, chunk_morton..=chunk_morton))
        .next();
    if let Some(row) = existing {
        ctx.db.terrain_chunk().row_id().update(TerrainChunk {
            row_id: row.row_id,
            terrain_set_id,
            chunk_morton,
            vertices,
            indices,
            lod,
        });
    } else {
        ctx.db.terrain_chunk().insert(TerrainChunk {
            row_id: 0,
            terrain_set_id,
            chunk_morton,
            vertices,
            indices,
            lod,
        });
    }
    Ok(())
}

/// Insert or update one chunk's manifest entry (content hash for client
/// cache validation). Bumped by the editor whenever a chunk is rebaked.
#[reducer]
pub fn terrain_manifest_upsert(
    ctx: &ReducerContext,
    terrain_set_id: u32,
    chunk_morton: u64,
    content_hash: String,
    version: u32,
) -> Result<(), String> {
    if !is_module_admin(ctx) && !is_debug_caller(ctx) {
        return Err("terrain_manifest_upsert: admin only".into());
    }
    if ctx
        .db
        .terrain_set()
        .terrain_set_id()
        .find(&terrain_set_id)
        .is_none()
    {
        return Err(format!(
            "terrain_manifest_upsert: unknown terrain_set_id {terrain_set_id}",
        ));
    }
    let existing = ctx
        .db
        .terrain_manifest()
        .by_set_chunk()
        .filter((terrain_set_id, chunk_morton..=chunk_morton))
        .next();
    if let Some(row) = existing {
        ctx.db.terrain_manifest().row_id().update(TerrainManifest {
            row_id: row.row_id,
            terrain_set_id,
            chunk_morton,
            content_hash,
            version,
        });
    } else {
        ctx.db.terrain_manifest().insert(TerrainManifest {
            row_id: 0,
            terrain_set_id,
            chunk_morton,
            content_hash,
            version,
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

    /// Set an entity's HP directly in the DB row, bypassing the tick pipeline.
    ///
    /// # Limitations
    /// Setting `hp = 0` does **not** kill the entity. Death detection only fires
    /// when the simulation worker processes a `HealthUpdate` from `commit_tick_results`.
    /// Using this with `hp = 0` will leave the entity `Active` with 0 HP; it will
    /// not transition to `DespawnPending` and the player cannot respawn.
    /// Use only for healing / restoring HP to a positive value.
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
        if ctx
            .db
            .entity_health()
            .entity_id()
            .find(&entity_id)
            .is_none()
        {
            return Err(format!("No entity_health row for entity {entity_id}"));
        }
        ctx.db.entity_health().entity_id().update(EntityHealth {
            entity_id,
            hp,
            max_hp,
            rls_group: 0,
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
        mod_stealth: Option<bool>,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_apply_buff: admin only".into());
        }
        if ctx.db.entity().entity_id().find(&entity_id).is_none() {
            return Err(format!("Entity {entity_id} does not exist"));
        }

        let current_tick = ctx
            .db
            .sim_tick()
            .iter()
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
            mod_ai_override_kind: None,
            mod_ai_override_target: None,
            mod_stealth,
            last_dot_tick: None,
        });

        // Derive stealthed_entity so the view filter works immediately.
        if mod_stealth == Some(true) {
            let team_id = ctx
                .db
                .entity_team()
                .entity_id()
                .find(&entity_id)
                .map(|t| t.team_id)
                .unwrap_or(0);
            if ctx
                .db
                .stealthed_entity()
                .entity_id()
                .find(&entity_id)
                .is_some()
            {
                ctx.db
                    .stealthed_entity()
                    .entity_id()
                    .update(StealthedEntity { entity_id, team_id });
            } else {
                ctx.db
                    .stealthed_entity()
                    .insert(StealthedEntity { entity_id, team_id });
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
        let tf = ctx
            .db
            .entity_transform()
            .entity_id()
            .find(&entity_id)
            .ok_or(format!("No transform row for entity {entity_id}"))?;

        ctx.db
            .entity_transform()
            .entity_id()
            .update(EntityTransform {
                entity_id,
                pos_x: x,
                pos_y: y,
                pos_z: z,
                rot_x: tf.rot_x,
                rot_y: tf.rot_y,
                rot_z: tf.rot_z,
                rot_w: tf.rot_w,
                vel_x: 0.0,
                vel_y: 0.0,
                vel_z: 0.0,
                angvel_x: 0.0,
                angvel_y: 0.0,
                angvel_z: 0.0,
                last_tick: tf.last_tick,
                rls_group: 0,
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

        log::info!(
            "debug_teleport: entity={entity_id} -> ({x}, {y}, {z}) region=({new_rx}, {new_rz})"
        );
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
        let max_slot = ctx
            .db
            .player_inventory()
            .owner_entity()
            .filter(&entity_id)
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

        log::info!(
            "debug_grant_item: entity={entity_id} item={item_id} qty={quantity} slot={max_slot}"
        );
        Ok(())
    }

    /// Force-remove an entity. Deletes companion rows (transform, health,
    /// region, npc_state, buffs, pending inputs, and client ownership).
    /// Use to clean up stuck or unwanted entities during testing.
    ///
    /// # Limitations
    /// This jumps directly to `EntityState::Removed`, skipping `DespawnPending`.
    /// The simulation worker normally reacts to `DespawnPending` to tear down its
    /// in-memory Rapier rigid body and aggro tables. Jumping straight to `Removed`
    /// may leave a ghost rigid body in the worker until it reconciles on the next
    /// sync cycle. Safe for cleanup in a dev environment; do not use as a substitute
    /// for natural NPC death in combat tests.
    #[reducer]
    pub fn debug_remove_entity(ctx: &ReducerContext, entity_id: u64) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_remove_entity: admin only".into());
        }
        let entity = ctx
            .db
            .entity()
            .entity_id()
            .find(&entity_id)
            .ok_or(format!("Entity {entity_id} does not exist"))?;

        // Transition to Removed.
        ctx.db.entity().entity_id().update(Entity {
            entity_id,
            kind: entity.kind,
            state: EntityState::Removed,
            spawned_at_tick: entity.spawned_at_tick,
            owner_identity: entity.owner_identity,
            rls_group: 0,
        });

        if let Some(owner_identity) = entity.owner_identity {
            ctx.db
                .client_sequence()
                .client_identity()
                .delete(&owner_identity);
        }

        // Buffs.
        ctx.db.active_buff().entity_id().delete(&entity_id);

        // Party, death state, and client ownership are debug-only cleanup.
        // Shared companion rows use the same path as commit/expiry removal.
        ctx.db.party_member().entity_id().delete(&entity_id);
        ctx.db.party_invite().invitee_entity().delete(&entity_id);
        ctx.db.death_state().entity_id().delete(&entity_id);
        delete_removed_entity_companion_rows(ctx, entity_id);

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
        log::info!(
            "debug_spawn_boss: entity_id={eid} pos=({pos_x},{pos_y},{pos_z}) max_hp={max_hp}"
        );
        Ok(())
    }

    /// Spawn a configured encounter boss near an existing entity and on the
    /// same visibility layer. This is the fast path for testing a new boss
    /// script inside whatever dungeon/open-world context the tester is already
    /// standing in, without editing the dungeon template just to change the
    /// `BossSpawn` row.
    #[reducer]
    pub fn debug_spawn_encounter_boss(
        ctx: &ReducerContext,
        anchor_entity_id: u64,
        encounter_name: String,
        offset_x: f32,
        offset_y: f32,
        offset_z: f32,
        max_hp: f32,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_spawn_encounter_boss: admin only".into());
        }
        if encounter_name.trim().is_empty() {
            return Err("debug_spawn_encounter_boss: encounter_name is required".into());
        }
        if max_hp <= 0.0 {
            return Err("debug_spawn_encounter_boss: max_hp must be positive".into());
        }

        let anchor_tf = ctx
            .db
            .entity_transform()
            .entity_id()
            .find(&anchor_entity_id)
            .ok_or_else(|| format!("No transform row for anchor entity {anchor_entity_id}"))?;
        let layer = ctx
            .db
            .entity_layer()
            .entity_id()
            .find(&anchor_entity_id)
            .map(|r| r.layer)
            .or_else(|| {
                ctx.db
                    .entity_region()
                    .entity_id()
                    .find(&anchor_entity_id)
                    .map(|r| r.layer)
            })
            .ok_or_else(|| format!("No layer row for anchor entity {anchor_entity_id}"))?;

        let pos_x = anchor_tf.pos_x + offset_x;
        let pos_y = anchor_tf.pos_y + offset_y;
        let pos_z = anchor_tf.pos_z + offset_z;
        let eid = spawn_npc_internal_on_layer(
            ctx,
            EntityKind::Boss,
            pos_x,
            pos_y,
            pos_z,
            max_hp,
            layer,
            Some(NpcConfig {
                entity_id: 0,
                encounter_name: Some(encounter_name.clone()),
                archetype_id: None,
                passive: false,
                no_chase: false,
                ability_id_1: None,
                ability_id_2: None,
                ability_id_3: None,
                ability_id_4: None,
                leash_radius: 30.0,
                aggro_radius: 15.0,
                body_shape: Some(game_schema::dungeon::BodyShapeDef::BossCapsule.to_u8()),
            }),
        );

        log::info!(
            "debug_spawn_encounter_boss: entity_id={eid} encounter={encounter_name} anchor={anchor_entity_id} layer={layer} pos=({pos_x},{pos_y},{pos_z}) max_hp={max_hp}"
        );
        Ok(())
    }

    /// Spawn a predefined test layout. Available scenarios:
    /// - `"combat"`: training dummy + reactive NPC + lock-on NPC (no chase)
    /// - `"stress"`: 1000 NPCs in a grid for performance testing
    #[reducer]
    pub fn debug_spawn_scenario(ctx: &ReducerContext, scenario: String) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_spawn_scenario: admin only".into());
        }

        match scenario.as_str() {
            "combat" => {
                // Training dummy: passive, high HP, no abilities, at (5, 1, 0)
                let e1 = spawn_npc_internal(
                    ctx,
                    EntityKind::Npc,
                    5.0,
                    1.0,
                    0.0,
                    10000.0,
                    Some(NpcConfig {
                        entity_id: 0,
                        encounter_name: None,
                        archetype_id: None,
                        passive: true,
                        no_chase: false,
                        ability_id_1: None,
                        ability_id_2: None,
                        ability_id_3: None,
                        ability_id_4: None,
                        leash_radius: 0.0,
                        aggro_radius: 0.0,
                        body_shape: None,
                    }),
                );
                log::info!("combat scenario: training dummy entity_id={e1}");

                // Reactive melee NPC: fights back with Slash, no chase, at (10, 1, 0)
                let e2 = spawn_npc_internal(
                    ctx,
                    EntityKind::Npc,
                    10.0,
                    1.0,
                    0.0,
                    500.0,
                    Some(NpcConfig {
                        entity_id: 0,
                        encounter_name: None,
                        archetype_id: None,
                        passive: false,
                        no_chase: true,
                        ability_id_1: Some(1),
                        ability_id_2: None,
                        ability_id_3: None,
                        ability_id_4: None,
                        leash_radius: 0.0,
                        aggro_radius: 0.0,
                        body_shape: None,
                    }),
                );
                log::info!("combat scenario: melee NPC entity_id={e2}");

                // Ranged NPC: fights back with Fireball (2), no chase, at (15, 1, 0)
                let e3 = spawn_npc_internal(
                    ctx,
                    EntityKind::Npc,
                    15.0,
                    1.0,
                    0.0,
                    500.0,
                    Some(NpcConfig {
                        entity_id: 0,
                        encounter_name: None,
                        archetype_id: None,
                        passive: false,
                        no_chase: true,
                        ability_id_1: Some(2),
                        ability_id_2: None,
                        ability_id_3: None,
                        ability_id_4: None,
                        leash_radius: 0.0,
                        aggro_radius: 0.0,
                        body_shape: None,
                    }),
                );
                log::info!("combat scenario: ranged NPC entity_id={e3}");

                // Full combat NPC: chases, multi-ability, at (20, 1, 0)
                let e4 = spawn_npc_internal(
                    ctx,
                    EntityKind::Npc,
                    20.0,
                    1.0,
                    0.0,
                    300.0,
                    Some(NpcConfig {
                        entity_id: 0,
                        encounter_name: None,
                        archetype_id: None,
                        passive: false,
                        no_chase: false,
                        ability_id_1: Some(1),
                        ability_id_2: Some(2),
                        ability_id_3: None,
                        ability_id_4: None,
                        leash_radius: 0.0,
                        aggro_radius: 0.0,
                        body_shape: None,
                    }),
                );
                log::info!("combat scenario: full-combat NPC entity_id={e4}");

                log::info!(
                    "combat scenario spawned: dummy={e1}, melee={e2}, ranged={e3}, full={e4}"
                );
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
                        if count >= 1000 {
                            break;
                        }
                        let x = col as f32 * spacing - offset;
                        let z = row as f32 * spacing - offset;
                        spawn_npc_internal(
                            ctx,
                            EntityKind::Npc,
                            x,
                            1.0,
                            z,
                            100.0,
                            Some(NpcConfig {
                                entity_id: 0,
                                encounter_name: None,
                                archetype_id: None,
                                passive: true,
                                no_chase: false,
                                ability_id_1: None,
                                ability_id_2: None,
                                ability_id_3: None,
                                ability_id_4: None,
                                leash_radius: 0.0,
                                aggro_radius: 0.0,
                                body_shape: None,
                            }),
                        );
                        count += 1;
                    }
                }
                log::info!(
                    "stress scenario spawned: {count} passive NPCs in {grid_size}x{grid_size} grid"
                );
                Ok(())
            }
            _ => Err(format!(
                "Unknown scenario: {scenario}. Available: combat, stress, props"
            )),
        }
    }

    /// Spawn many NPCs for performance testing. All NPCs are passive.
    #[reducer]
    pub fn debug_spawn_many(ctx: &ReducerContext, count: u32, spacing: f32) -> Result<(), String> {
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
                if spawned >= count {
                    break;
                }
                let x = col as f32 * spacing - offset;
                let z = row as f32 * spacing - offset;
                spawn_npc_internal(
                    ctx,
                    EntityKind::Npc,
                    x,
                    1.0,
                    z,
                    100.0,
                    Some(NpcConfig {
                        entity_id: 0,
                        encounter_name: None,
                        archetype_id: None,
                        passive: true,
                        no_chase: false,
                        ability_id_1: None,
                        ability_id_2: None,
                        ability_id_3: None,
                        ability_id_4: None,
                        leash_radius: 0.0,
                        aggro_radius: 0.0,
                        body_shape: None,
                    }),
                );
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
                if spawned >= pair_count {
                    break;
                }
                let cx = col as f32 * pair_spacing - offset;
                let cz = row as f32 * pair_spacing - offset;

                // Spawn pair 1 unit apart on each side of the centre.
                let id_a = spawn_npc_internal(
                    ctx,
                    EntityKind::Npc,
                    cx - 1.0,
                    1.0,
                    cz,
                    1000.0,
                    Some(NpcConfig {
                        entity_id: 0,
                        encounter_name: None,
                        archetype_id: None,
                        passive: false,
                        no_chase,
                        ability_id_1: Some(1), // Slash
                        ability_id_2: None,
                        ability_id_3: None,
                        ability_id_4: None,
                        leash_radius: 0.0,
                        aggro_radius: 0.0,
                        body_shape: None,
                    }),
                );
                let id_b = spawn_npc_internal(
                    ctx,
                    EntityKind::Npc,
                    cx + 1.0,
                    1.0,
                    cz,
                    1000.0,
                    Some(NpcConfig {
                        entity_id: 0,
                        encounter_name: None,
                        archetype_id: None,
                        passive: false,
                        no_chase,
                        ability_id_1: Some(1), // Slash
                        ability_id_2: None,
                        ability_id_3: None,
                        ability_id_4: None,
                        leash_radius: 0.0,
                        aggro_radius: 0.0,
                        body_shape: None,
                    }),
                );

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

        log::info!(
            "debug_spawn_combat: spawned {spawned} fighting pairs ({} NPCs)",
            spawned * 2
        );
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

        let current_tick = ctx
            .db
            .sim_tick()
            .iter()
            .max_by_key(|t| t.tick_id)
            .map(|t| t.tick_id)
            .unwrap_or(0);

        let entity = ctx.db.entity().insert(Entity {
            entity_id: 0,
            kind: EntityKind::Prop,
            state: EntityState::Spawning,
            spawned_at_tick: current_tick,
            owner_identity: None,
            rls_group: 0,
        });
        let eid = entity.entity_id;

        ctx.db.entity_transform().insert(EntityTransform {
            entity_id: eid,
            pos_x,
            pos_y,
            pos_z,
            rot_x: 0.0,
            rot_y: 0.0,
            rot_z: 0.0,
            rot_w: 1.0,
            vel_x: 0.0,
            vel_y: 0.0,
            vel_z: 0.0,
            angvel_x: 0.0,
            angvel_y: 0.0,
            angvel_z: 0.0,
            last_tick: current_tick,
            rls_group: 0,
        });

        ctx.db.entity_health().insert(EntityHealth {
            entity_id: eid,
            hp: 1.0,
            max_hp: 1.0,
            rls_group: 0,
        });

        ctx.db.entity_region().insert(EntityRegion {
            entity_id: eid,
            region_x: (pos_x / 50.0).floor() as i32,
            region_z: (pos_z / 50.0).floor() as i32,
            layer: 0,
        });

        ctx.db.entity_layer().insert(EntityLayer {
            entity_id: eid,
            layer: 0,
        });

        log::info!("debug_spawn_prop: entity_id={eid} pos=({pos_x},{pos_y},{pos_z})");
        Ok(())
    }

    /// Move an entity to a different visibility layer. Useful for testing
    /// layer isolation in the `nearby_transforms` view (dungeon instances,
    /// phasing, stealth).
    ///
    /// # Limitations
    /// Only updates `EntityRegion.layer` and `EntityLayer.layer` — the entity's
    /// `EntityTransform` position is **not** moved. The simulation worker tracks
    /// entities per-layer based on `EntityLayer` subscription callbacks; it will
    /// pick up this change on the next sync cycle, but the entity will appear at
    /// its old XZ coordinates on the new layer. For a proper layer transfer (with
    /// repositioning) use `join_instance` / `leave_instance` or `debug_join_instance`.
    #[reducer]
    pub fn debug_set_layer(ctx: &ReducerContext, entity_id: u64, layer: u32) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_set_layer: admin only".into());
        }
        let er = ctx
            .db
            .entity_region()
            .entity_id()
            .find(&entity_id)
            .ok_or(format!("No entity_region row for entity {entity_id}"))?;
        ctx.db.entity_region().entity_id().update(EntityRegion {
            entity_id,
            region_x: er.region_x,
            region_z: er.region_z,
            layer,
        });
        ctx.db
            .entity_layer()
            .entity_id()
            .update(EntityLayer { entity_id, layer });
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
            ctx.db
                .entity_team()
                .entity_id()
                .update(EntityTeam { entity_id, team_id });
        } else {
            ctx.db
                .entity_team()
                .insert(EntityTeam { entity_id, team_id });
        }
        log::info!("debug_set_team: entity={entity_id} team_id={team_id}");
        Ok(())
    }

    /// Join an instance without the party membership requirement.
    /// Identical to `join_instance` but skips the party check, allowing
    /// solo testing of dungeon flows from the CLI.  Takes an explicit
    /// `entity_id` so admin/CLI callers (who have no registered player)
    /// can move any entity into an instance.
    #[reducer]
    pub fn debug_join_instance(
        ctx: &ReducerContext,
        entity_id: u64,
        instance_id: u64,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_join_instance: admin only".into());
        }
        // Validate entity exists.
        ctx.db
            .entity()
            .entity_id()
            .find(&entity_id)
            .ok_or_else(|| format!("Entity {entity_id} not found"))?;

        let instance = ctx
            .db
            .instance()
            .instance_id()
            .find(&instance_id)
            .ok_or("Instance not found")?;
        if instance.state != InstanceState::Active && instance.state != InstanceState::Pending {
            return Err(format!("Instance is {:?} — cannot join", instance.state));
        }

        if ctx
            .db
            .instance_membership()
            .entity_id()
            .find(&entity_id)
            .is_some()
        {
            return Err("Already in an instance".into());
        }

        let member_count = ctx
            .db
            .instance_membership()
            .instance_id()
            .filter(&instance_id)
            .count() as u32;
        if member_count >= instance.max_players {
            return Err("Instance is full".into());
        }

        ctx.db.instance_membership().insert(InstanceMembership {
            entity_id,
            instance_id,
            disconnect_at: None,
        });

        let spawn_point = match load_dungeon_template(&instance.template_id) {
            Ok(t) => pick_spawn_from(&t.spawn_points, None).unwrap_or(FALLBACK_SPAWN),
            Err(e) => {
                log::warn!(
                    "debug_join_instance: could not load template '{}' for spawn_points: {e}; \
                     falling back to {:?}",
                    instance.template_id,
                    FALLBACK_SPAWN
                );
                FALLBACK_SPAWN
            }
        };
        reposition_entity_to_spawn(ctx, entity_id, spawn_point, instance.layer);

        log::info!(
            "debug_join_instance: entity {} joined instance {} (layer {}) at spawn ({:.1},{:.1},{:.1}) — party check skipped",
            entity_id,
            instance_id,
            instance.layer,
            spawn_point[0],
            spawn_point[1],
            spawn_point[2]
        );
        Ok(())
    }

    /// Create a test instance using `test_dungeon_01` and immediately join
    /// the given entity into it.  Takes an explicit `entity_id` so
    /// admin/CLI callers can operate without a registered player.
    #[reducer]
    pub fn debug_create_instance(ctx: &ReducerContext, entity_id: u64) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_create_instance: admin only".into());
        }
        debug_create_instance_for_template(ctx, entity_id, "test_dungeon_01".into(), 4)
    }

    /// Create an instance from any dungeon template and immediately join the
    /// given entity into it. This keeps ad-hoc boss tests from having to reuse
    /// or edit `test_dungeon_01` just to exercise a different authored arena.
    #[reducer]
    pub fn debug_create_instance_for_template(
        ctx: &ReducerContext,
        entity_id: u64,
        template_id: String,
        max_players: u32,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_create_instance_for_template: admin only".into());
        }
        if ctx.db.entity().entity_id().find(&entity_id).is_none() {
            return Err(format!("Entity {entity_id} not found"));
        }
        if template_id.trim().is_empty() {
            return Err("debug_create_instance_for_template: template_id is required".into());
        }
        if max_players == 0 {
            return Err("debug_create_instance_for_template: max_players must be > 0".into());
        }

        create_instance(ctx, template_id.clone(), max_players)?;

        let inst = ctx
            .db
            .instance()
            .iter()
            .filter(|i| i.template_id == template_id)
            .max_by_key(|i| i.instance_id)
            .ok_or("Instance not found after creation")?;

        debug_join_instance(ctx, entity_id, inst.instance_id)?;
        log::info!(
            "debug_create_instance_for_template: created + joined instance {} template={} layer={} max_players={}",
            inst.instance_id,
            inst.template_id,
            inst.layer,
            inst.max_players
        );
        Ok(())
    }

    /// Increment the "kills" zone counter for testing `world_clock` transitions.
    /// Admin-callable wrapper around `increment_zone_counter`, which remains
    /// restricted to trusted workers.
    #[reducer]
    pub fn debug_add_zone_kill(
        ctx: &ReducerContext,
        layer: u32,
        region_x: i32,
        region_z: i32,
        count: u32,
    ) -> Result<(), String> {
        if !is_module_admin(ctx) && !is_debug_caller(ctx) {
            return Err("debug_add_zone_kill: admin only".into());
        }
        for _ in 0..count.min(100) {
            increment_zone_counter(ctx, layer, region_x, region_z, "kills".into(), 1.0)?;
        }
        log::info!(
            "debug_add_zone_kill: layer={layer} region=({region_x},{region_z}) +{count} kills"
        );
        Ok(())
    }
}
