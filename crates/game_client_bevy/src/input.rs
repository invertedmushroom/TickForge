use bevy::prelude::*;
use bevy::time::{Time, Timer, TimerMode};
use game_client::module_bindings::*;
use spacetimedb_sdk::Table;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::ability_bar::{AbilityCooldowns, BLOCK_ABILITY_ID, ClientTargetingMode, all_abilities};
#[allow(unused)]
use crate::camera::CursorCaptured;
use crate::camera::GameCamera;
use crate::camera::LocalPlayer;
use crate::diagnostics::DiagnosticsState;
use crate::hud::reticle_viewport_position;
use crate::spacetime::{LocalPlayerEntity, StdbConnection, TickCounter};

pub struct InputPlugin;

#[derive(SystemSet, Debug, Hash, PartialEq, Eq, Clone)]
pub enum InputSet {
    DriveInput,
}

#[derive(Component, Default)]
pub struct LastMoveDir {
    pub world: Vec3,
}

#[derive(Resource)]
pub struct IntentThrottle(pub Timer);

/// Async reducer acknowledgement counters for quick on-screen debugging.
///
/// Also owns the redundant-resend ring buffer so that `submit_intent_logged`
/// only needs a single resource handle on the call sites.
#[derive(Resource)]
pub struct IntentAckStats {
    sent: Arc<AtomicU64>,
    accepted: Arc<AtomicU64>,
    rejected: Arc<AtomicU64>,
    ring: IntentRingBuffer,
}

impl Default for IntentAckStats {
    fn default() -> Self {
        Self {
            sent: Arc::new(AtomicU64::new(0)),
            accepted: Arc::new(AtomicU64::new(0)),
            rejected: Arc::new(AtomicU64::new(0)),
            ring: IntentRingBuffer::default(),
        }
    }
}

impl IntentAckStats {
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }
    pub fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }
    /// Advance the ring's ack cursor to the authoritative server cursor.
    /// Called from `pump_connection` every frame with
    /// `client_sequence.last_processed_sequence` from the table cache, which
    /// is the only ground-truth signal of which sequences the server has
    /// committed to its queue. Reducer success callbacks alone are not
    /// sufficient: `submit_intents_batch` returns `Ok(())` even when its
    /// tail entries were silently dropped to the queue cap.
    pub fn ack_up_to(&self, sequence_id: u64) {
        self.ring.mark_acked(sequence_id);
    }
}

/// Maximum number of recent unacknowledged intents the client retains for
/// redundant batch resends. At 20 Hz this is ~600 ms of input redundancy —
/// comfortable headroom over typical jitter and a couple of consecutive
/// dropped reducer calls, while staying well below the server-side
/// `MAX_BATCH_LEN = 16`.
const INTENT_RING_CAPACITY: usize = 12;

/// Minimum age a ring entry must reach before the redundant-resend path
/// will include it in a batch. The single-shot `submit_intent` call's ack
/// (or the authoritative `client_sequence` cursor) normally lands within
/// one server tick (~50 ms at 20 Hz). This threshold sits one full resend
/// cycle past that, so an entry only gets resent when its original
/// reducer call was *actually* dropped — not merely in flight. Keeps the
/// redundancy path zero-cost during normal play.
const RESEND_MIN_AGE: Duration = Duration::from_millis(75);

/// Ring buffer of recent intents the client has submitted but not yet seen
/// acknowledged. A 20 Hz Bevy system resends the buffer contents via the
/// `submit_intents_batch` reducer so that a single dropped network call
/// no longer permanently loses an input.
#[derive(Default, Clone)]
pub struct IntentRingBuffer {
    inner: Arc<Mutex<RingState>>,
}

/// Single ring entry: `(sequence_id, client_observed_tick, action,
/// pushed_at)`. `pushed_at` powers the adaptive resend gate — entries
/// younger than `RESEND_MIN_AGE` are skipped so the redundancy path
/// only fires when the single-shot reducer call was actually dropped.
type RingEntry = (u64, u64, IntentAction, Instant);

#[derive(Default)]
struct RingState {
    /// Recent unacknowledged intents. Bounded to `INTENT_RING_CAPACITY`;
    /// oldest entries evicted as new intents are pushed.
    entries: VecDeque<RingEntry>,
    /// Highest sequence_id known to be accepted by the server.
    /// Entries with `sequence_id <= highest_acked` are pruned on push and
    /// before each redundant batch send.
    highest_acked: u64,
}

impl IntentRingBuffer {
    fn push(&self, sequence_id: u64, observed_tick: u64, action: IntentAction) {
        self.push_at(sequence_id, observed_tick, action, Instant::now());
    }

    /// Test seam: push with an explicit timestamp so unit tests can
    /// exercise the `RESEND_MIN_AGE` gate without sleeping.
    fn push_at(
        &self,
        sequence_id: u64,
        observed_tick: u64,
        action: IntentAction,
        pushed_at: Instant,
    ) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        // Drop entries the server has already acknowledged.
        while guard
            .entries
            .front()
            .is_some_and(|(seq, _, _, _)| *seq <= guard.highest_acked)
        {
            guard.entries.pop_front();
        }
        // Cap the buffer — drop the oldest unacked entry to make room.
        while guard.entries.len() >= INTENT_RING_CAPACITY {
            guard.entries.pop_front();
        }
        guard
            .entries
            .push_back((sequence_id, observed_tick, action, pushed_at));
    }

    fn mark_acked(&self, sequence_id: u64) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        if sequence_id > guard.highest_acked {
            guard.highest_acked = sequence_id;
        }
    }

    /// Snapshot **all** unacked entries. Used by tests and any future
    /// caller that wants the full ring contents regardless of age.
    #[cfg(test)]
    fn snapshot(&self) -> Vec<BatchedIntent> {
        self.snapshot_filtered(None)
    }

    /// Snapshot only entries that have been waiting at least `min_age`
    /// without an ack. The single-shot reducer call's ack typically lands
    /// well under this threshold, so during normal play the result is
    /// empty and `resend_intent_batch` makes no reducer call.
    fn snapshot_stale(&self, min_age: Duration) -> Vec<BatchedIntent> {
        self.snapshot_filtered(Some((Instant::now(), min_age)))
    }

    fn snapshot_filtered(&self, age_gate: Option<(Instant, Duration)>) -> Vec<BatchedIntent> {
        let Ok(mut guard) = self.inner.lock() else {
            return Vec::new();
        };
        while guard
            .entries
            .front()
            .is_some_and(|(seq, _, _, _)| *seq <= guard.highest_acked)
        {
            guard.entries.pop_front();
        }
        guard
            .entries
            .iter()
            .filter(|(_, _, _, pushed_at)| match age_gate {
                Some((now, min_age)) => now.saturating_duration_since(*pushed_at) >= min_age,
                None => true,
            })
            .map(|(seq, tick, action, _)| BatchedIntent {
                sequence_id: *seq,
                client_observed_tick: *tick,
                action: action.clone(),
            })
            .collect()
    }
}

/// 20 Hz timer driving redundant `submit_intents_batch` resends.
#[derive(Resource)]
pub struct IntentBatchTimer(pub Timer);

impl Default for IntentBatchTimer {
    fn default() -> Self {
        Self(Timer::from_seconds(1.0 / 20.0, TimerMode::Repeating))
    }
}

/// Current target-lock state used for cast-at-target intent generation.
#[derive(Resource, Default)]
pub struct TargetLockState {
    pub target_entity: Option<u64>,
}

/// Dynamic keybindings for skills 1 through 9.
#[derive(Resource)]
pub struct SkillBindings {
    pub slots: [u32; 9],
}

impl Default for SkillBindings {
    fn default() -> Self {
        Self {
            slots: [1, 2, 3, BLOCK_ABILITY_ID, 23, 24, 20, 21, 22], // Slash, Fireball, Smash, Block, FlameAura, FirePatch, ChainLtng, Backstab, Blink
        }
    }
}

/// Crosshair world aim state. Updated each frame by `update_crosshair`.
///
/// Computed via camera raycast through screen center (TERA-style reticle).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AimPointSource {
    #[default]
    None,
    EntityHit,
    WorldHit,
    Fallback,
}

#[derive(Resource, Default)]
pub struct CrosshairAim {
    /// Canonical world-space aim point used for directional casts and aiming.
    pub world_aim_point: Option<Vec3F>,
    /// Ground-plane intersection point (Y≈0). Always set when looking down.
    pub ground_position: Option<Vec3F>,
    /// Soft-target: entity the crosshair is hovering over.
    pub soft_target: Option<u64>,
    /// What kind of world aim point was resolved this frame.
    pub aim_source: AimPointSource,
    /// The normalized direction of the camera ray.
    pub camera_ray_dir: Option<Vec3F>,
    /// The origin point of the camera ray.
    pub camera_ray_origin: Option<Vec3F>,
}

/// Client-side lock-on session state. Tracks whether a TERA-style lock-on
/// ability is currently in its tagging phase (between first UseAbility and
/// second UseAbility/fire or ReleaseAbility/cancel).
#[derive(Resource, Default)]
pub struct LockOnSession {
    /// The lock-on ability id currently in tagging phase, if any.
    pub active_ability: Option<u32>,
    /// The observed sim tick when the client should locally expire this session.
    pub expires_at_tick: Option<u64>,
}

const LOCK_ON_DEFAULT_TIMEOUT_TICKS: u64 = 400;

fn lock_on_timeout_ticks_for(ability_id: u32) -> u64 {
    all_abilities()
        .iter()
        .find(|a| a.id == ability_id)
        .and_then(|a| a.lock_on_timeout_ticks)
        .map(u64::from)
        .filter(|ticks| *ticks > 0)
        .unwrap_or(LOCK_ON_DEFAULT_TIMEOUT_TICKS)
}

fn clear_lock_on_session(lock_on: &mut LockOnSession) {
    lock_on.active_ability = None;
    lock_on.expires_at_tick = None;
}

fn clamp_ground_target_position(player_pos: Vec3, point: &Vec3F, max_range: f32) -> Vec3F {
    let mut ground_pt = point.clone();
    ground_pt.y = GROUND_Y; // Force to ground plane

    if !max_range.is_finite() || max_range <= 0.0 {
        return ground_pt;
    }

    let dx = ground_pt.x - player_pos.x;
    let dz = ground_pt.z - player_pos.z;
    let dist_sq = dx * dx + dz * dz;
    let max_sq = max_range * max_range;
    if !dist_sq.is_finite() || dist_sq <= max_sq {
        return ground_pt;
    }

    let inv_dist = 1.0 / dist_sq.sqrt();
    Vec3F {
        x: player_pos.x + dx * inv_dist * max_range,
        y: GROUND_Y,
        z: player_pos.z + dz * inv_dist * max_range,
    }
}

fn orbit_forward(yaw: f32) -> Vec3F {
    Vec3F {
        x: -yaw.sin(),
        y: 0.0,
        z: -yaw.cos(),
    }
}

fn resolve_aim_direction(
    player_tf: &Transform,
    crosshair: &CrosshairAim,
    orbit_yaw: f32,
) -> Option<Vec3F> {
    if let Some(aim) = &crosshair.world_aim_point {
        let dx = aim.x - player_tf.translation.x;
        let dy = aim.y - player_tf.translation.y;
        let dz = aim.z - player_tf.translation.z;
        let len = (dx * dx + dy * dy + dz * dz).sqrt();
        if len > 1e-4 {
            return Some(Vec3F {
                x: dx / len,
                y: dy / len,
                z: dz / len,
            });
        }
    }

    // Ultimate fallback if no camera ray data exists
    Some(orbit_forward(orbit_yaw))
}

fn submit_intent_logged(
    stdb: &StdbConnection,
    ack: &IntentAckStats,
    entity_id: u64,
    sequence_id: u64,
    action: IntentAction,
    client_observed_tick: u64,
    label: &'static str,
) {
    ack.sent.fetch_add(1, Ordering::Relaxed);
    let accepted = Arc::clone(&ack.accepted);
    let rejected = Arc::clone(&ack.rejected);
    let ring_for_callback = ack.ring.clone();

    // Record locally for redundant batch resend before issuing the reducer
    // call. If the single-shot call is dropped on the wire, the next
    // `submit_intents_batch` tick will redeliver this entry.
    ack.ring
        .push(sequence_id, client_observed_tick, action.clone());

    if let Err(e) = stdb.conn.reducers.submit_intent_then(
        entity_id,
        sequence_id,
        action,
        client_observed_tick,
        // INVARIANT: this callback body MUST be order-independent. The
        // SpacetimeDB SDK does not guarantee callback delivery order
        // relative to other reducer callbacks (or relative to the
        // overlapping `submit_intents_batch_then` callback below).
        // The only mutations performed here are:
        //   - `accepted` / `rejected` AtomicU64 counters (commutative)
        //   - `ring.mark_acked(seq)` which is max(highest_acked, seq)
        //     (commutative, monotonic)
        // Authoritative gameplay sequence state lives in the
        // `client_sequence` table cache and is read in `pump_connection`.
        // Do NOT add gameplay-affecting writes to this closure.
        move |_ctx, result| match result {
            Ok(Ok(())) => {
                accepted.fetch_add(1, Ordering::Relaxed);
                ring_for_callback.mark_acked(sequence_id);
            }
            Ok(Err(msg)) => {
                rejected.fetch_add(1, Ordering::Relaxed);
                // Only a "stale sequence" rejection counts as an implicit
                // ack: the server cursor is already at or above this seq,
                // so further redundancy resends would just be skipped. This
                // is an early-prune optimization; the same prune will
                // happen via `pump_connection` once the next frame reads
                // `client_sequence.last_processed_sequence`. Other
                // rejections (queue full, ownership/registration errors)
                // MUST NOT prune the ring — queue-full is transient
                // backpressure that the next 20 Hz `submit_intents_batch`
                // resend should recover.
                if msg.starts_with("Stale sequence") {
                    ring_for_callback.mark_acked(sequence_id);
                }
                log::warn!(
                    "Intent rejected ({label}, seq={sequence_id}, tick={client_observed_tick}): {msg}"
                );
            }
            Err(err) => {
                rejected.fetch_add(1, Ordering::Relaxed);
                log::warn!(
                    "Intent callback error ({label}, seq={sequence_id}, tick={client_observed_tick}): {err}"
                );
            }
        },
    ) {
        log::warn!(
            "Failed to submit intent ({label}, seq={sequence_id}, tick={client_observed_tick}): {e}"
        );
    }
}

impl Plugin for InputPlugin {
    fn build(&self, app: &mut App) {
        // 20 Hz intent throttle for continuous movement intents (50ms)
        let timer = Timer::from_seconds(1.0 / 20.0, TimerMode::Repeating);
        app.insert_resource(IntentThrottle(timer));
        app.init_resource::<IntentAckStats>();
        app.init_resource::<IntentBatchTimer>();
        app.init_resource::<TargetLockState>();
        app.init_resource::<SkillBindings>();
        app.init_resource::<CrosshairAim>();
        app.init_resource::<LockOnSession>();
        app.add_systems(
            Update,
            (
                handle_target_lock,
                update_crosshair,
                handle_movement,
                handle_jump,
                handle_abilities,
                handle_lock_on_input,
                handle_weapon_swap,
                handle_interact,
                resend_intent_batch,
            )
                .in_set(InputSet::DriveInput),
        );
    }
}

/// Periodically resend the recent unacknowledged intents as a single
/// `submit_intents_batch` reducer call. The server idempotently drops any
/// entries it has already processed, so a single dropped single-shot call
/// no longer permanently loses an input.
fn resend_intent_batch(
    time: Res<Time>,
    mut timer: ResMut<IntentBatchTimer>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    ack: Res<IntentAckStats>,
) {
    if !timer.0.tick(time.delta()).just_finished() {
        return;
    }
    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else {
        return;
    };

    // Adaptive gate: only resend entries that have already outlived
    // their normal ack window. In the common case the single-shot
    // `submit_intent` ack arrives first and prunes the ring via
    // `mark_acked`, so this snapshot is empty and we make no reducer
    // call. The redundancy path only costs a reducer call when the
    // original single-shot was actually dropped on the wire.
    let snapshot = ack.ring.snapshot_stale(RESEND_MIN_AGE);
    if snapshot.is_empty() {
        return;
    }

    // Track the highest sequence in this batch for log correlation.
    let highest_seq = snapshot.iter().map(|i| i.sequence_id).max().unwrap_or(0);

    if let Err(e) = stdb.conn.reducers.submit_intents_batch_then(
        entity_id,
        snapshot,
        // INVARIANT: order-independent. See `submit_intent_then` callback
        // above for the rationale. The batch reducer returns `Ok(())` even
        // when tail entries were silently dropped to the server-side queue
        // cap, so we deliberately do NOT mark the ring acked from this
        // callback. The authoritative ack signal is
        // `client_sequence.last_processed_sequence`, pumped into the ring
        // via `IntentAckStats::ack_up_to` in `pump_connection`.
        move |_ctx, result| match result {
            Ok(Ok(())) => {}
            Ok(Err(msg)) => {
                log::warn!("Intent batch rejected (highest_seq={highest_seq}): {msg}");
            }
            Err(err) => {
                log::warn!("Intent batch callback error (highest_seq={highest_seq}): {err}");
            }
        },
    ) {
        log::warn!("Failed to submit intent batch (highest_seq={highest_seq}): {e}");
    }
}

/// Cycle target lock with Tab, clear with Escape.
fn handle_target_lock(
    keyboard: Res<ButtonInput<KeyCode>>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    mut lock: ResMut<TargetLockState>,
) {
    if !keyboard.just_pressed(KeyCode::Tab) && !keyboard.just_pressed(KeyCode::Escape) {
        return;
    }

    if keyboard.just_pressed(KeyCode::Escape) {
        lock.target_entity = None;
        log::info!("Target lock cleared");
        return;
    }

    let Some(stdb) = stdb else { return };
    let Some(local_id) = local_player.entity_id else {
        return;
    };

    let mut candidates: Vec<u64> = stdb
        .conn
        .db
        .nearby_transforms()
        .iter()
        .map(|r| r.entity_id)
        .filter(|id| *id != local_id)
        .collect();

    candidates.sort_unstable();
    candidates.dedup();

    if candidates.is_empty() {
        lock.target_entity = None;
        log::info!("Target lock: no nearby targets");
        return;
    }

    let next = match lock.target_entity {
        None => candidates[0],
        Some(current) => {
            let idx = candidates
                .iter()
                .position(|id| *id == current)
                .unwrap_or(usize::MAX);
            if idx == usize::MAX || idx + 1 >= candidates.len() {
                candidates[0]
            } else {
                candidates[idx + 1]
            }
        }
    };
    lock.target_entity = Some(next);
    log::info!("Target locked: #{next}");
}

/// WASD movement → submit_intent(Move) calls.
fn handle_movement(
    keyboard: Res<ButtonInput<KeyCode>>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    mut tick_counter: ResMut<TickCounter>,
    time: Res<Time>,
    mut throttle: ResMut<IntentThrottle>,
    mut diag: ResMut<DiagnosticsState>,
    ack: Res<IntentAckStats>,
    orbit: Res<crate::camera::OrbitState>,
    mut player_q: Query<(&mut Transform, &mut LastMoveDir), With<LocalPlayer>>,
) {
    // Always tick the throttle timer so it keeps a consistent cadence.
    throttle.0.tick(time.delta());

    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else {
        return;
    };

    let dir = movement_input_vector(&keyboard);

    if dir == Vec3::ZERO {
        if let Ok((_, mut last_move)) = player_q.get_single_mut() {
            last_move.world = Vec3::ZERO;
        }
        // Send Stop on key release (immediate, no throttle).
        if keyboard.any_just_released([KeyCode::KeyE, KeyCode::KeyD, KeyCode::KeyS, KeyCode::KeyF])
        {
            tick_counter.intent_seq += 1;
            submit_intent_logged(
                &stdb,
                &ack,
                entity_id,
                tick_counter.intent_seq,
                IntentAction::Stop,
                tick_counter.last_tick,
                "Stop",
            );
            log::info!(
                "Stop intent sent (seq={}, tick={})",
                tick_counter.intent_seq,
                tick_counter.last_tick
            );
        }
        return;
    }

    let dir = dir.normalize();
    let world_dir = rotated_move_dir(dir, orbit.yaw);

    if let Ok((mut tf, mut last_move)) = player_q.get_single_mut() {
        let prediction_speed = game_core::stats::base_speed(game_schema::EntityKind::Player);
        tf.translation += world_dir * prediction_speed * time.delta_secs();
        last_move.world = world_dir;
    }

    // Submit movement intent at up to 20 Hz.
    if throttle.0.just_finished() {
        tick_counter.intent_seq += 1;

        let move_dir = MoveDir {
            dir_x: world_dir.x,
            dir_y: 0.0,
            dir_z: world_dir.z,
        };
        submit_intent_logged(
            &stdb,
            &ack,
            entity_id,
            tick_counter.intent_seq,
            IntentAction::Move(move_dir),
            tick_counter.last_tick,
            "Move",
        );

        diag.record_intent();
        log::debug!(
            "Move intent sent (seq={}, tick={})",
            tick_counter.intent_seq,
            tick_counter.last_tick
        );
    }
}

fn movement_input_vector(keyboard: &ButtonInput<KeyCode>) -> Vec3 {
    let mut dir = Vec3::ZERO;
    if keyboard.pressed(KeyCode::KeyE) {
        dir.z -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyD) {
        dir.z += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyS) {
        dir.x -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyF) {
        dir.x += 1.0;
    }
    dir
}

fn rotated_move_dir(dir: Vec3, yaw: f32) -> Vec3 {
    let cos_yaw = yaw.cos();
    let sin_yaw = yaw.sin();
    Vec3::new(
        dir.x * cos_yaw + dir.z * sin_yaw,
        0.0,
        -dir.x * sin_yaw + dir.z * cos_yaw,
    )
}

/// Space = Jump intent (instant, no throttle).
fn handle_jump(
    keyboard: Res<ButtonInput<KeyCode>>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    mut tick_counter: ResMut<TickCounter>,
    ack: Res<IntentAckStats>,
) {
    if !keyboard.just_pressed(KeyCode::Space) {
        return;
    }
    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else {
        return;
    };

    tick_counter.intent_seq += 1;
    submit_intent_logged(
        &stdb,
        &ack,
        entity_id,
        tick_counter.intent_seq,
        IntentAction::Jump,
        tick_counter.last_tick,
        "Jump",
    );
    log::info!(
        "Jump intent sent (seq={}, tick={})",
        tick_counter.intent_seq,
        tick_counter.last_tick
    );
}

/// Tilde (`~`) = WeaponSwap intent.
fn handle_weapon_swap(
    keyboard: Res<ButtonInput<KeyCode>>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    mut tick_counter: ResMut<TickCounter>,
    ack: Res<IntentAckStats>,
) {
    if !keyboard.just_pressed(KeyCode::Backquote) {
        return;
    }
    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else {
        return;
    };

    tick_counter.intent_seq += 1;
    submit_intent_logged(
        &stdb,
        &ack,
        entity_id,
        tick_counter.intent_seq,
        IntentAction::WeaponSwap,
        tick_counter.last_tick,
        "WeaponSwap",
    );
    log::info!(
        "WeaponSwap intent sent (seq={}, tick={})",
        tick_counter.intent_seq,
        tick_counter.last_tick
    );
}

/// Soft-target sphere radius for entity hit testing.
const SOFT_TARGET_RADIUS: f32 = 0.8;
/// Maximum raycast distance for soft-targeting.
const SOFT_TARGET_MAX_DIST: f32 = 60.0;
/// Ground plane Y coordinate (server ground surface is at Y ≈ 0.1).
const GROUND_Y: f32 = 0.1;
const AIM_FALLBACK_DIST: f32 = 50.0;

fn make_vec3f(v: Vec3) -> Vec3F {
    Vec3F {
        x: v.x,
        y: v.y,
        z: v.z,
    }
}

fn select_lock_on_tag_target(crosshair: &CrosshairAim) -> Option<u64> {
    crosshair.soft_target
}

fn resolve_crosshair_aim_state(
    ray_origin: Vec3,
    ray_dir: Vec3,
    entity_samples: impl IntoIterator<Item = (u64, Vec3)>,
    local_id: u64,
) -> CrosshairAim {
    let mut crosshair = CrosshairAim::default();
    let mut best_entity: Option<(u64, f32, Vec3)> = None;
    for (entity_id, entity_pos) in entity_samples {
        if entity_id == local_id {
            continue;
        }
        let to_entity = entity_pos - ray_origin;
        let t = to_entity.dot(ray_dir);
        if t < 0.0 || t > SOFT_TARGET_MAX_DIST {
            continue;
        }
        let closest = ray_origin + ray_dir * t;
        let dist_sq = (closest - entity_pos).length_squared();
        if dist_sq < SOFT_TARGET_RADIUS * SOFT_TARGET_RADIUS {
            if best_entity.is_none() || t < best_entity.expect("checked is_some").1 {
                best_entity = Some((entity_id, t, closest));
            }
        }
    }

    let ground_pos = if ray_dir.y.abs() > 1e-6 {
        let t = (GROUND_Y - ray_origin.y) / ray_dir.y;
        if t > 0.0 && t < SOFT_TARGET_MAX_DIST {
            Some(make_vec3f(ray_origin + ray_dir * t))
        } else {
            None
        }
    } else {
        None
    };

    if let Some((entity_id, _, hit_point)) = best_entity {
        crosshair.world_aim_point = Some(make_vec3f(hit_point));
        crosshair.soft_target = Some(entity_id);
        crosshair.aim_source = AimPointSource::EntityHit;
    } else if let Some(point) = ground_pos.as_ref() {
        crosshair.world_aim_point = Some(point.clone());
        crosshair.aim_source = AimPointSource::WorldHit;
    } else {
        crosshair.world_aim_point = Some(make_vec3f(ray_origin + ray_dir * AIM_FALLBACK_DIST));
        crosshair.aim_source = AimPointSource::Fallback;
    }
    crosshair.ground_position = ground_pos;
    crosshair.camera_ray_dir = Some(make_vec3f(ray_dir));
    crosshair.camera_ray_origin = Some(make_vec3f(ray_origin));
    crosshair
}

/// TERA-style reticle: cast a ray from the camera through screen center.
/// Tests against nearby entity capsule approximations first (sphere test),
/// then falls back to ground-plane intersection.
fn update_crosshair(
    cam_q: Query<(&GlobalTransform, &Camera), With<GameCamera>>,
    player_q: Query<&Transform, (With<LocalPlayer>, Without<GameCamera>)>,
    entity_q: Query<(&Transform, &crate::sync::ServerEntity), Without<GameCamera>>,
    local_player: Res<LocalPlayerEntity>,
    active_dungeon: Option<Res<crate::dungeon_geometry::ActiveDungeon>>,
    mut crosshair: ResMut<CrosshairAim>,
) {
    let Ok((cam_gtf, camera)) = cam_q.get_single() else {
        *crosshair = CrosshairAim::default();
        return;
    };
    let Ok(_player_tf) = player_q.get_single() else {
        *crosshair = CrosshairAim::default();
        return;
    };

    // Ray from camera through the same reticle anchor the HUD uses.
    let Some(viewport_size) = camera.logical_viewport_size() else {
        *crosshair = CrosshairAim::default();
        return;
    };
    let reticle = reticle_viewport_position(viewport_size);
    let Ok(ray) = camera.viewport_to_world(cam_gtf, reticle) else {
        *crosshair = CrosshairAim::default();
        return;
    };
    *crosshair = resolve_crosshair_aim_state(
        ray.origin,
        ray.direction.as_vec3(),
        entity_q
            .iter()
            .map(|(tf, se)| (se.entity_id, tf.translation)),
        local_player.entity_id.unwrap_or(u64::MAX),
    );

    // When the player is inside a dungeon, refine ground Y from the
    // embedded heightfield. The plane-intersect above gave XZ at
    // `GROUND_Y`; we re-sample the true surface at that XZ so AoE rings,
    // ground-target reticles, and HUD debug lines stop floating through
    // uneven terrain. The server re-validates GroundTarget placement
    // regardless, so a miss here is cosmetic.
    if let Some(active) = active_dungeon.as_deref() {
        if let Some(g) = &mut crosshair.ground_position {
            if let Some(y) = crate::dungeon_geometry::sample_terrain_y(active.template, g.x, g.z) {
                g.y = y;
                if matches!(crosshair.aim_source, AimPointSource::WorldHit) {
                    if let Some(w) = &mut crosshair.world_aim_point {
                        w.y = y;
                    }
                }
            }
        }
    }
}

/// Lock-on input: ability key presses (open/fire), left-click to tag,
/// right-click or Escape to cancel.
fn handle_lock_on_input(
    mouse: Res<ButtonInput<MouseButton>>,
    keyboard: Res<ButtonInput<KeyCode>>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    mut tick_counter: ResMut<TickCounter>,
    mut cooldowns: ResMut<AbilityCooldowns>,
    mut diag: ResMut<DiagnosticsState>,
    ack: Res<IntentAckStats>,
    crosshair: Res<CrosshairAim>,
    bindings: Res<SkillBindings>,
    mut lock_on: ResMut<LockOnSession>,
) {
    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else {
        return;
    };
    cooldowns.current_tick = tick_counter.last_tick;

    if let (Some(ability_id), Some(expires_at_tick)) =
        (lock_on.active_ability, lock_on.expires_at_tick)
    {
        if tick_counter.last_tick >= expires_at_tick {
            clear_lock_on_session(&mut lock_on);
            log::info!("Lock-on timeout: ability {ability_id} expired at tick {expires_at_tick}");
        }
    }

    let keys = [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ];

    // Detect lock-on ability key presses → open session or fire.
    for (i, key) in keys.iter().enumerate() {
        if keyboard.just_pressed(*key) {
            let id = bindings.slots[i];
            let Some(def) = all_abilities().iter().find(|a| a.id == id) else {
                break;
            };
            if def.targeting != ClientTargetingMode::LockOn {
                break;
            }

            if lock_on.active_ability == Some(id) {
                // Second press of same lock-on ability → fire on tagged targets.
                if !cooldowns.activate_local(id, tick_counter.last_tick, def.cooldown_ticks) {
                    log::debug!("Lock-on FIRE suppressed: ability {id} is on cooldown");
                    return;
                }
                tick_counter.intent_seq += 1;
                submit_intent_logged(
                    &stdb,
                    &ack,
                    entity_id,
                    tick_counter.intent_seq,
                    IntentAction::UseAbility(UseAbilityData {
                        ability_id: id,
                        target: AbilityTarget::None,
                        target_hint: None,
                    }),
                    tick_counter.last_tick,
                    "LockOnFire",
                );
                clear_lock_on_session(&mut lock_on);
                diag.record_intent();
                log::info!("Lock-on FIRE: ability {id}");
            } else {
                if cooldowns.remaining(id, def.cooldown_ticks) > 0 {
                    log::debug!("Lock-on OPEN suppressed: ability {id} is on cooldown");
                    return;
                }
                // Cancel any existing lock-on session for a different ability.
                if let Some(old) = lock_on.active_ability.take() {
                    tick_counter.intent_seq += 1;
                    submit_intent_logged(
                        &stdb,
                        &ack,
                        entity_id,
                        tick_counter.intent_seq,
                        IntentAction::ReleaseAbility(old),
                        tick_counter.last_tick,
                        "LockOnCancelOld",
                    );
                    lock_on.expires_at_tick = None;
                    log::info!("Lock-on cancel (switching): ability {old}");
                }
                // First press → open lock-on session.
                tick_counter.intent_seq += 1;
                submit_intent_logged(
                    &stdb,
                    &ack,
                    entity_id,
                    tick_counter.intent_seq,
                    IntentAction::UseAbility(UseAbilityData {
                        ability_id: id,
                        target: AbilityTarget::None,
                        target_hint: None,
                    }),
                    tick_counter.last_tick,
                    "LockOnOpen",
                );
                lock_on.active_ability = Some(id);
                lock_on.expires_at_tick =
                    Some(tick_counter.last_tick + lock_on_timeout_ticks_for(id));
                diag.record_intent();
                log::info!("Lock-on OPEN: ability {id} — click targets to tag");
            }
            return;
        }
    }

    // Everything below requires an active lock-on session.
    let Some(ability_id) = lock_on.active_ability else {
        return;
    };

    // Cancel: right-click or Escape.
    if mouse.just_pressed(MouseButton::Right) || keyboard.just_pressed(KeyCode::Escape) {
        tick_counter.intent_seq += 1;
        submit_intent_logged(
            &stdb,
            &ack,
            entity_id,
            tick_counter.intent_seq,
            IntentAction::ReleaseAbility(ability_id),
            tick_counter.last_tick,
            "LockOnCancel",
        );
        clear_lock_on_session(&mut lock_on);
        log::info!("Lock-on CANCEL: ability {ability_id}");
        return;
    }

    // Tag: left-click on a target (hard-lock or soft-target).
    if mouse.just_pressed(MouseButton::Left) {
        let target = select_lock_on_tag_target(&crosshair);
        if let Some(target_id) = target {
            tick_counter.intent_seq += 1;
            submit_intent_logged(
                &stdb,
                &ack,
                entity_id,
                tick_counter.intent_seq,
                IntentAction::TagTarget(target_id),
                tick_counter.last_tick,
                "TagTarget",
            );
            log::info!("Lock-on TAG: target #{target_id}");
        } else {
            log::info!("Lock-on tag: no target under crosshair");
        }
    }
}

/// E = Interact with target-locked entity.
fn handle_interact(
    keyboard: Res<ButtonInput<KeyCode>>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    mut tick_counter: ResMut<TickCounter>,
    ack: Res<IntentAckStats>,
    lock: Res<TargetLockState>,
) {
    if !keyboard.just_pressed(KeyCode::KeyB) {
        return;
    }
    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else {
        return;
    };
    let Some(target) = lock.target_entity else {
        log::info!("Interact: no target locked");
        return;
    };

    tick_counter.intent_seq += 1;
    submit_intent_logged(
        &stdb,
        &ack,
        entity_id,
        tick_counter.intent_seq,
        IntentAction::Interact(target),
        tick_counter.last_tick,
        "Interact",
    );
}

/// Ability keys 1–9 (skill slots). Also handles Block when bound to a slot
/// (hold semantics: Block while held, Stop on release).
fn handle_abilities(
    mut commands: Commands,
    keyboard: Res<ButtonInput<KeyCode>>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    mut tick_counter: ResMut<TickCounter>,
    mut cooldowns: ResMut<AbilityCooldowns>,
    mut diag: ResMut<DiagnosticsState>,
    ack: Res<IntentAckStats>,
    lock: Res<TargetLockState>,
    crosshair: Res<CrosshairAim>,
    bindings: Res<SkillBindings>,
    throttle: Res<IntentThrottle>,
    orbit: Res<crate::camera::OrbitState>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    player_q: Query<
        (bevy::ecs::entity::Entity, &MeshMaterial3d<StandardMaterial>),
        With<LocalPlayer>,
    >,
    player_q_aim: Query<&Transform, (With<LocalPlayer>, Without<GameCamera>)>,
) {
    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else {
        return;
    };
    cooldowns.current_tick = tick_counter.last_tick;

    let keys = [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ];

    // 1. Handle releases: ReleaseAbility for normal skills, Stop for block.
    //    Lock-on abilities never send ReleaseAbility on key-up (two-press flow).
    for (i, key) in keys.iter().enumerate() {
        if keyboard.just_released(*key) {
            let id = bindings.slots[i];
            if id == 0 {
                break;
            }
            let is_lock_on = all_abilities()
                .iter()
                .any(|a| a.id == id && a.targeting == ClientTargetingMode::LockOn);
            if is_lock_on {
                break;
            }
            tick_counter.intent_seq += 1;
            if id == BLOCK_ABILITY_ID {
                submit_intent_logged(
                    &stdb,
                    &ack,
                    entity_id,
                    tick_counter.intent_seq,
                    IntentAction::Stop,
                    tick_counter.last_tick,
                    "BlockRelease",
                );
            } else {
                submit_intent_logged(
                    &stdb,
                    &ack,
                    entity_id,
                    tick_counter.intent_seq,
                    IntentAction::ReleaseAbility(id),
                    tick_counter.last_tick,
                    "ReleaseAbility",
                );
            }
            break;
        }
    }

    // 2. Handle block hold: continuous Block at throttle rate.
    if throttle.0.just_finished() {
        for (i, key) in keys.iter().enumerate() {
            if bindings.slots[i] == BLOCK_ABILITY_ID && keyboard.pressed(*key) {
                let face_dir = orbit_forward(orbit.yaw);
                tick_counter.intent_seq += 1;
                submit_intent_logged(
                    &stdb,
                    &ack,
                    entity_id,
                    tick_counter.intent_seq,
                    IntentAction::Block(BlockData {
                        look_dir: MoveDir {
                            dir_x: face_dir.x,
                            dir_y: face_dir.y,
                            dir_z: face_dir.z,
                        },
                    }),
                    tick_counter.last_tick,
                    "Block",
                );
                break;
            }
        }
    }

    // 3. Handle ability presses (skip block and lock-on slots — handled elsewhere).
    let mut pressed_id = None;
    for (i, key) in keys.iter().enumerate() {
        if keyboard.just_pressed(*key) {
            let id = bindings.slots[i];
            if id != 0 && id != BLOCK_ABILITY_ID {
                let is_lock_on = all_abilities()
                    .iter()
                    .any(|a| a.id == id && a.targeting == ClientTargetingMode::LockOn);
                if !is_lock_on {
                    pressed_id = Some(id);
                }
            }
            break;
        }
    }

    let Some(ability_id) = pressed_id else {
        return;
    };

    let Some(ability_def) = all_abilities().iter().find(|a| a.id == ability_id).cloned() else {
        return;
    };
    if cooldowns.remaining(ability_id, ability_def.cooldown_ticks) > 0 {
        log::debug!("UseAbility suppressed: ability {ability_id} is on cooldown");
        return;
    }
    let ability_targeting = ability_def.targeting;

    let target_and_hint = match ability_targeting {
        ClientTargetingMode::SelfOnly | ClientTargetingMode::CasterOffset => {
            Some((AbilityTarget::None, None))
        }
        ClientTargetingMode::LockOn => Some((AbilityTarget::None, None)), // handled by lock-on system
        ClientTargetingMode::EntityTarget => lock
            .target_entity
            .map(|target_id| (AbilityTarget::Entity(target_id), None)),
        ClientTargetingMode::GroundTarget => {
            // Ground-target needs a ground world position.
            // Prefer the ground-plane intersection so hovering over an NPC does not
            // replace the intended placement point with the NPC's body hit point.
            // Clamp on the XZ plane so TERA-style reticle placement still lands at
            // max range even when the third-person camera ray hits the floor farther out.
            if let Some(pos) = crosshair
                .ground_position
                .as_ref()
                .or(crosshair.world_aim_point.as_ref())
            {
                let clamped = if let (Some(max_range), Ok(player_tf)) =
                    (ability_def.max_range, player_q_aim.get_single())
                {
                    clamp_ground_target_position(player_tf.translation, pos, max_range)
                } else {
                    pos.clone()
                };
                Some((AbilityTarget::Position(clamped), None))
            } else {
                None
            }
        }
        ClientTargetingMode::DirectionTarget | ClientTargetingMode::RaycastStrict => player_q_aim
            .get_single()
            .ok()
            .and_then(|player_tf| resolve_aim_direction(player_tf, &crosshair, orbit.yaw))
            .map(|dir| (AbilityTarget::Direction(dir), None)),
        ClientTargetingMode::AimAssist => {
            // AimAssist keeps targeting spatial too, but can provide a
            // non-authoritative hint from the current lock/soft target.
            let hint = lock.target_entity.or(crosshair.soft_target);
            player_q_aim
                .get_single()
                .ok()
                .and_then(|player_tf| resolve_aim_direction(player_tf, &crosshair, orbit.yaw))
                .map(|dir| (AbilityTarget::Direction(dir), hint))
        }
    };

    let Some((target, hint)) = target_and_hint else {
        return;
    };

    // FaceTo is no longer sent — character facing is driven by WASD movement
    // direction on the server, so we only need to send UseAbility here.
    if !cooldowns.activate_local(
        ability_id,
        tick_counter.last_tick,
        ability_def.cooldown_ticks,
    ) {
        log::debug!("UseAbility suppressed: ability {ability_id} is on cooldown");
        return;
    }
    tick_counter.intent_seq += 1;
    submit_intent_logged(
        &stdb,
        &ack,
        entity_id,
        tick_counter.intent_seq,
        IntentAction::UseAbility(UseAbilityData {
            ability_id,
            target,
            target_hint: hint,
        }),
        tick_counter.last_tick,
        "UseAbility",
    );

    // Record diagnostics + VFX flash. Cooldown was recorded immediately
    // before submit so duplicate presses cannot restart the local timer.
    diag.record_intent();
    if let Ok((bevy_entity, mat_handle)) = player_q.get_single() {
        crate::vfx::trigger_flash(
            &mut commands,
            &mut materials,
            bevy_entity,
            &mat_handle.0,
            ability_id,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hud::{RETICLE_LEFT, RETICLE_TOP};

    #[test]
    fn clamp_ground_target_position_limits_horizontal_range() {
        let player = Vec3::new(0.0, 5.0, 0.0);
        let point = Vec3F {
            x: 40.0,
            y: 0.1,
            z: 0.0,
        };
        let clamped = clamp_ground_target_position(player, &point, 30.0);

        assert!(
            (clamped.x - 30.0).abs() < 0.01,
            "Expected X to clamp to 30, got {}",
            clamped.x
        );
        assert!(
            (clamped.z - 0.0).abs() < 0.01,
            "Expected Z to stay 0, got {}",
            clamped.z
        );
        assert!(
            (clamped.y - 0.1).abs() < 0.01,
            "Expected Y to stay on ground plane, got {}",
            clamped.y
        );
    }

    #[test]
    fn movement_direction_matches_edsf_layout() {
        let mut keyboard = ButtonInput::<KeyCode>::default();
        keyboard.press(KeyCode::KeyE);
        keyboard.press(KeyCode::KeyF);
        let dir = movement_input_vector(&keyboard).normalize();
        let rotated = rotated_move_dir(dir, 0.0);

        assert!(rotated.x > 0.0);
        assert!(rotated.z < 0.0);
    }

    #[test]
    fn movement_direction_rotates_with_camera_yaw() {
        let mut keyboard = ButtonInput::<KeyCode>::default();
        keyboard.press(KeyCode::KeyE);
        let dir = movement_input_vector(&keyboard).normalize();
        let rotated = rotated_move_dir(dir, std::f32::consts::FRAC_PI_2);

        assert!(rotated.x < -0.99);
        assert!(rotated.z.abs() < 0.01);
    }

    #[test]
    fn reticle_sampling_uses_same_anchor_as_hud() {
        let viewport = Vec2::new(1920.0, 1080.0);
        let reticle = crate::hud::reticle_viewport_position(viewport);

        assert!((reticle.x - viewport.x * (RETICLE_LEFT * 0.01)).abs() < 0.01);
        assert!((reticle.y - viewport.y * (RETICLE_TOP * 0.01)).abs() < 0.01);
    }

    #[test]
    fn lock_on_tag_uses_crosshair_target_only() {
        let crosshair = CrosshairAim {
            soft_target: Some(42),
            ..default()
        };
        assert_eq!(select_lock_on_tag_target(&crosshair), Some(42));
    }

    #[test]
    fn resolve_aim_direction_preserves_vertical_pitch() {
        let player_tf = Transform::from_xyz(0.0, 0.0, 0.0);
        let crosshair = CrosshairAim {
            world_aim_point: Some(Vec3F {
                x: 0.0,
                y: 10.0,
                z: 10.0,
            }),
            ..default()
        };

        let dir = resolve_aim_direction(&player_tf, &crosshair, 0.0).expect("aim direction");
        assert!(
            dir.y > 0.6,
            "Expected upward pitch to be preserved, got {}",
            dir.y
        );
        assert!(
            dir.z > 0.6,
            "Expected forward component to be preserved, got {}",
            dir.z
        );
    }

    fn ring_with_capacity_marker() -> IntentRingBuffer {
        IntentRingBuffer::default()
    }

    #[test]
    fn ring_snapshot_stale_excludes_fresh_entries() {
        let ring = ring_with_capacity_marker();
        // Push at "now" — snapshot_stale called immediately must skip it.
        ring.push(1, 0, IntentAction::Stop);
        let snap = ring.snapshot_stale(RESEND_MIN_AGE);
        assert!(
            snap.is_empty(),
            "fresh entry should not appear in stale snapshot, got {} entries",
            snap.len()
        );
    }

    #[test]
    fn ring_snapshot_stale_includes_aged_entries() {
        let ring = ring_with_capacity_marker();
        let now = Instant::now();
        // Backdate one entry past the threshold; leave a fresh one too.
        ring.push_at(
            1,
            10,
            IntentAction::Stop,
            now - RESEND_MIN_AGE - Duration::from_millis(10),
        );
        ring.push_at(2, 11, IntentAction::Stop, now);

        let snap = ring.snapshot_stale(RESEND_MIN_AGE);
        assert_eq!(snap.len(), 1, "only the aged entry should be resent");
        assert_eq!(snap[0].sequence_id, 1);
        assert_eq!(snap[0].client_observed_tick, 10);
    }

    #[test]
    fn ring_mark_acked_prunes_regardless_of_age() {
        let ring = ring_with_capacity_marker();
        let aged = Instant::now() - RESEND_MIN_AGE - Duration::from_millis(50);
        ring.push_at(1, 0, IntentAction::Stop, aged);
        ring.push_at(2, 1, IntentAction::Stop, aged);
        ring.push_at(3, 2, IntentAction::Stop, aged);

        ring.mark_acked(2);
        let snap = ring.snapshot_stale(RESEND_MIN_AGE);
        assert_eq!(
            snap.len(),
            1,
            "entries up to the acked seq should be pruned"
        );
        assert_eq!(snap[0].sequence_id, 3);
    }

    #[test]
    fn ring_capacity_evicts_oldest_unacked() {
        let ring = ring_with_capacity_marker();
        let aged = Instant::now() - RESEND_MIN_AGE - Duration::from_millis(50);
        for seq in 1..=(INTENT_RING_CAPACITY as u64 + 3) {
            ring.push_at(seq, seq, IntentAction::Stop, aged);
        }
        let snap = ring.snapshot_stale(RESEND_MIN_AGE);
        assert_eq!(snap.len(), INTENT_RING_CAPACITY);
        // First three sequences should have been evicted.
        assert_eq!(snap.first().unwrap().sequence_id, 4);
        assert_eq!(
            snap.last().unwrap().sequence_id,
            INTENT_RING_CAPACITY as u64 + 3
        );
    }

    #[test]
    fn ring_full_snapshot_ignores_age_gate() {
        let ring = ring_with_capacity_marker();
        ring.push(1, 0, IntentAction::Stop);
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 1, "snapshot() must not apply the age filter");
    }
}
