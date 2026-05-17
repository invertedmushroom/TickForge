use bevy::prelude::*;
use bevy::time::{Time, Timer, TimerMode};
use game_client::module_bindings::*;
use spacetimedb_sdk::Table;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ability_bar::{all_abilities, AbilityCooldowns, BLOCK_ABILITY_ID, ClientTargetingMode};
#[allow(unused)]
use crate::camera::CursorCaptured;
use crate::camera::GameCamera;
use crate::camera::LocalPlayer;
use crate::diagnostics::DiagnosticsState;
use crate::spacetime::{LocalPlayerEntity, StdbConnection, TickCounter};

pub struct InputPlugin;

#[derive(Resource)]
pub struct IntentThrottle(pub Timer);

/// Async reducer acknowledgement counters for quick on-screen debugging.
#[derive(Resource)]
pub struct IntentAckStats {
    sent: Arc<AtomicU64>,
    accepted: Arc<AtomicU64>,
    rejected: Arc<AtomicU64>,
}

impl Default for IntentAckStats {
    fn default() -> Self {
        Self {
            sent: Arc::new(AtomicU64::new(0)),
            accepted: Arc::new(AtomicU64::new(0)),
            rejected: Arc::new(AtomicU64::new(0)),
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
#[derive(Resource, Default)]
pub struct CrosshairAim {
    /// World-space aim point (entity hit or ground intersection).
    pub aim_position: Option<Vec3F>,
    /// Ground-plane intersection point (Y≈0). Always set when looking down.
    pub ground_position: Option<Vec3F>,
    /// Soft-target: entity the crosshair is hovering over.
    pub soft_target: Option<u64>,
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
    // Two-stage solve (Production Action-Combat Standard):
    // 1. The camera trace provides the desired impact point (`aim_position`).
    //    This is either a true target hit point, or a far convergence fallback.
    // 2. We solve the vector from the cast origin (the character) to that exact point.
    if let Some(aim) = &crosshair.aim_position {
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

    if let Err(e) = stdb.conn.reducers.submit_intent_then(
        entity_id,
        sequence_id,
        action,
        client_observed_tick,
        move |_ctx, result| match result {
            Ok(Ok(())) => {
                accepted.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Err(msg)) => {
                rejected.fetch_add(1, Ordering::Relaxed);
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
            ),
        );
        app.add_systems(Update, handle_debug_keys);
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
) {
    // Always tick the throttle timer so it keeps a consistent cadence.
    throttle.0.tick(time.delta());

    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else {
        return;
    };

    let mut dir = Vec3::ZERO;
    if keyboard.pressed(KeyCode::KeyW) {
        dir.z -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyS) {
        dir.z += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyA) {
        dir.x -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyD) {
        dir.x += 1.0;
    }

    if dir == Vec3::ZERO {
        // Send Stop on key release (immediate, no throttle).
        if keyboard.any_just_released([KeyCode::KeyW, KeyCode::KeyS, KeyCode::KeyA, KeyCode::KeyD])
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
    // Submit movement intent at up to 20 Hz.
    if throttle.0.just_finished() {
        tick_counter.intent_seq += 1;

        let yaw = orbit.yaw;
        let cos_yaw = yaw.cos();
        let sin_yaw = yaw.sin();
        let rot_x = dir.x * cos_yaw + dir.z * sin_yaw;
        let rot_z = -dir.x * sin_yaw + dir.z * cos_yaw;

        let move_dir = MoveDir {
            dir_x: rot_x,
            dir_y: 0.0,
            dir_z: rot_z,
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

/// TERA-style reticle: cast a ray from the camera through screen center.
/// Tests against nearby entity capsule approximations first (sphere test),
/// then falls back to ground-plane intersection.
fn update_crosshair(
    cam_q: Query<(&GlobalTransform, &Camera), With<GameCamera>>,
    player_q: Query<&Transform, (With<LocalPlayer>, Without<GameCamera>)>,
    entity_q: Query<(&Transform, &crate::sync::ServerEntity), Without<GameCamera>>,
    local_player: Res<LocalPlayerEntity>,
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

    // Ray from camera through the reticle pixel (matches RETICLE_TOP offset in hud.rs).
    let Some(viewport_size) = camera.logical_viewport_size() else {
        *crosshair = CrosshairAim::default();
        return;
    };
    let reticle = Vec2::new(viewport_size.x * 0.5, viewport_size.y * 0.465);
    let Ok(ray) = camera.viewport_to_world(cam_gtf, reticle) else {
        *crosshair = CrosshairAim::default();
        return;
    };

    let ray_origin = ray.origin;
    let ray_dir = ray.direction.as_vec3();

    let local_id = local_player.entity_id.unwrap_or(u64::MAX);

    // 1. Test against entity bounding spheres (capsule approximation).
    let mut best_entity: Option<(u64, f32, Vec3)> = None;
    for (tf, se) in entity_q.iter() {
        if se.entity_id == local_id {
            continue;
        }
        let to_entity = tf.translation - ray_origin;
        let t = to_entity.dot(ray_dir);
        if t < 0.0 || t > SOFT_TARGET_MAX_DIST {
            continue;
        }
        let closest = ray_origin + ray_dir * t;
        let dist_sq = (closest - tf.translation).length_squared();
        if dist_sq < SOFT_TARGET_RADIUS * SOFT_TARGET_RADIUS {
            if best_entity.is_none() || t < best_entity.unwrap().1 {
                best_entity = Some((se.entity_id, t, closest));
            }
        }
    }

    // 2. Ground plane intersection (Y = GROUND_Y).
    let ground_pos = if ray_dir.y.abs() > 1e-6 {
        let t = (GROUND_Y - ray_origin.y) / ray_dir.y;
        if t > 0.0 && t < SOFT_TARGET_MAX_DIST {
            let p = ray_origin + ray_dir * t;
            Some(Vec3F {
                x: p.x,
                y: p.y,
                z: p.z,
            })
        } else {
            None
        }
    } else {
        None
    };

    // 3. Resolve aim point: entity hit takes priority.
    // Note: We deliberately exclude the infinite mathematical ground plane (`ground_pos`)
    // from this step. If we treated the floor as a raycast hit for directional abilities,
    // aiming at the ground 5 meters away would cause the character to shoot steeply
    // downward into the floor (parallax dipping). Instead, if no entity is hit, we
    // fall back to a far convergence point to keep trajectory parallel with the horizon.
    if let Some((eid, _t, hit_point)) = best_entity {
        crosshair.aim_position = Some(Vec3F {
            x: hit_point.x,
            y: hit_point.y,
            z: hit_point.z,
        });
        crosshair.soft_target = Some(eid);
    } else {
        // Fallback: convergence point 50 units down the camera ray
        let aim = ray_origin + ray_dir * 50.0;
        crosshair.aim_position = Some(Vec3F {
            x: aim.x,
            y: aim.y,
            z: aim.z,
        });
        crosshair.soft_target = None;
    }
    crosshair.ground_position = ground_pos;
    crosshair.camera_ray_dir = Some(Vec3F {
        x: ray_dir.x,
        y: ray_dir.y,
        z: ray_dir.z,
    });
    crosshair.camera_ray_origin = Some(Vec3F {
        x: ray_origin.x,
        y: ray_origin.y,
        z: ray_origin.z,
    });
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
    lock: Res<TargetLockState>,
    bindings: Res<SkillBindings>,
    mut lock_on: ResMut<LockOnSession>,
) {
    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else {
        return;
    };

    if let (Some(ability_id), Some(expires_at_tick)) =
        (lock_on.active_ability, lock_on.expires_at_tick)
    {
        if tick_counter.last_tick >= expires_at_tick {
            clear_lock_on_session(&mut lock_on);
            log::info!(
                "Lock-on timeout: ability {ability_id} expired at tick {expires_at_tick}"
            );
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
            let is_lock_on = all_abilities()
                .iter()
                .any(|a| a.id == id && a.targeting == ClientTargetingMode::LockOn);
            if !is_lock_on {
                break;
            }

            if lock_on.active_ability == Some(id) {
                // Second press of same lock-on ability → fire on tagged targets.
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
                cooldowns.activate(id, tick_counter.last_tick);
                diag.record_intent();
                log::info!("Lock-on FIRE: ability {id}");
            } else {
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
                lock_on.expires_at_tick = Some(
                    tick_counter.last_tick + lock_on_timeout_ticks_for(id),
                );
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
        let target = lock.target_entity.or(crosshair.soft_target);
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

/// Debug: F9 triggers fake local-player death to test the death overlay.
fn handle_debug_keys(
    keyboard: Res<ButtonInput<KeyCode>>,
    local_player: Res<LocalPlayerEntity>,
    mut death_events: EventWriter<crate::vfx::DeathNotification>,
) {
    if keyboard.just_pressed(KeyCode::F9) {
        let entity_id = local_player.entity_id.unwrap_or(0);
        death_events.send(crate::vfx::DeathNotification {
            entity_id,
            is_local_player: true,
        });
        log::info!("Debug: F9 — fake death triggered");
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
    if !keyboard.just_pressed(KeyCode::KeyE) {
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

    let ability_def = all_abilities().iter().find(|a| a.id == ability_id).cloned();
    let ability_targeting = ability_def
        .as_ref()
        .map(|a| a.targeting)
        .unwrap_or(ClientTargetingMode::DirectionTarget);

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
                .or(crosshair.aim_position.as_ref())
            {
                let clamped = if let (Some(max_range), Ok(player_tf)) = (
                    ability_def.and_then(|a| a.max_range),
                    player_q_aim.get_single(),
                ) {
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

    // Record cooldown + diagnostics + VFX flash.
    cooldowns.activate(ability_id, tick_counter.last_tick);
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
}
