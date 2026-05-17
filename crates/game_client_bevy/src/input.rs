use bevy::prelude::*;
use bevy::time::{Timer, TimerMode, Time};
use game_client::module_bindings::*;
use spacetimedb_sdk::Table;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::ability_bar::AbilityCooldowns;
use crate::diagnostics::DiagnosticsState;
use crate::spacetime::{LocalPlayerEntity, StdbConnection, TickCounter};
use crate::camera::LocalPlayer;

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
    pub fn sent(&self) -> u64 { self.sent.load(Ordering::Relaxed) }
    pub fn accepted(&self) -> u64 { self.accepted.load(Ordering::Relaxed) }
    pub fn rejected(&self) -> u64 { self.rejected.load(Ordering::Relaxed) }
}

/// Current target-lock state used for cast-at-target intent generation.
#[derive(Resource, Default)]
pub struct TargetLockState {
    pub target_entity: Option<u64>,
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
        app.add_systems(Update, (handle_target_lock, handle_movement, handle_abilities, handle_block, handle_interact, handle_debug_keys));
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
    let Some(local_id) = local_player.entity_id else { return };

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
            let idx = candidates.iter().position(|id| *id == current).unwrap_or(usize::MAX);
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
) {
    // Always tick the throttle timer so it keeps a consistent cadence.
    throttle.0.tick(time.delta());

    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else { return };

    let mut dir = Vec3::ZERO;
    if keyboard.pressed(KeyCode::KeyW) { dir.z -= 1.0; }
    if keyboard.pressed(KeyCode::KeyS) { dir.z += 1.0; }
    if keyboard.pressed(KeyCode::KeyA) { dir.x -= 1.0; }
    if keyboard.pressed(KeyCode::KeyD) { dir.x += 1.0; }

    if dir == Vec3::ZERO {
        // Send Stop on key release (immediate, no throttle).
        if keyboard.any_just_released([KeyCode::KeyW, KeyCode::KeyS, KeyCode::KeyA, KeyCode::KeyD]) {
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
            log::info!("Stop intent sent (seq={}, tick={})", tick_counter.intent_seq, tick_counter.last_tick);
        }
        return;
    }

    let dir = dir.normalize();
    // Submit movement intent at up to 20 Hz.
    if throttle.0.just_finished() {
        tick_counter.intent_seq += 1;
        let move_dir = MoveDir { dir_x: dir.x, dir_y: 0.0, dir_z: dir.z };
        submit_intent_logged(
            &stdb,
            &ack,
            entity_id,
            tick_counter.intent_seq,
            IntentAction::Move(move_dir.clone()),
            tick_counter.last_tick,
            "Move",
        );

        // Also send FaceTo so the server orients the character.
        tick_counter.intent_seq += 1;
        submit_intent_logged(
            &stdb,
            &ack,
            entity_id,
            tick_counter.intent_seq,
            IntentAction::FaceTo(move_dir),
            tick_counter.last_tick,
            "FaceTo",
        );

        diag.record_intent();
        log::debug!("Move+FaceTo intent sent (seq={}, tick={})", tick_counter.intent_seq, tick_counter.last_tick);
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

/// Q = Block stance. Sends Block + FaceTo every throttle tick while held, Stop on release.
/// FaceTo is derived from the camera orbit yaw so the player blocks in the
/// direction they're looking — without this, the server-side facing would be
/// frozen at the last movement direction and the directional block check would
/// likely fail.
fn handle_block(
    keyboard: Res<ButtonInput<KeyCode>>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    mut tick_counter: ResMut<TickCounter>,
    ack: Res<IntentAckStats>,
    throttle: Res<IntentThrottle>,
    orbit: Res<crate::camera::OrbitState>,
) {
    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else { return };

    if keyboard.pressed(KeyCode::KeyQ) {
        if throttle.0.just_finished() {
            // Face the camera direction so directional block works.
            let face_dir = MoveDir {
                dir_x: -orbit.yaw.sin(),
                dir_y: 0.0,
                dir_z: -orbit.yaw.cos(),
            };
            tick_counter.intent_seq += 1;
            submit_intent_logged(
                &stdb,
                &ack,
                entity_id,
                tick_counter.intent_seq,
                IntentAction::FaceTo(face_dir),
                tick_counter.last_tick,
                "BlockFace",
            );

            tick_counter.intent_seq += 1;
            submit_intent_logged(
                &stdb,
                &ack,
                entity_id,
                tick_counter.intent_seq,
                IntentAction::Block,
                tick_counter.last_tick,
                "Block",
            );
        }
    } else if keyboard.just_released(KeyCode::KeyQ) {
        tick_counter.intent_seq += 1;
        submit_intent_logged(
            &stdb,
            &ack,
            entity_id,
            tick_counter.intent_seq,
            IntentAction::Stop,
            tick_counter.last_tick,
            "BlockRelease",
        );
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
    let Some(entity_id) = local_player.entity_id else { return };
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

/// Ability keys: 1 = Slash, 2 = Fireball, 3 = Smash.
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
    mut materials: ResMut<Assets<StandardMaterial>>,
    player_q: Query<(bevy::ecs::entity::Entity, &MeshMaterial3d<StandardMaterial>), With<LocalPlayer>>,
) {
    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else { return };

    // Smash (Digit3) uses hold-to-charge: UseAbility on press, ReleaseAbility on release.
    if keyboard.just_released(KeyCode::Digit3) {
        tick_counter.intent_seq += 1;
        submit_intent_logged(
            &stdb,
            &ack,
            entity_id,
            tick_counter.intent_seq,
            IntentAction::ReleaseAbility(3),
            tick_counter.last_tick,
            "ReleaseAbility",
        );
        return;
    }

    let ability_id = if keyboard.just_pressed(KeyCode::Digit1) {
        Some(1u32) // Slash
    } else if keyboard.just_pressed(KeyCode::Digit2) {
        Some(2) // Fireball
    } else if keyboard.just_pressed(KeyCode::Digit3) {
        Some(3) // Smash (charge start)
    } else {
        None
    };

    let Some(ability_id) = ability_id else { return };

    let target = match lock.target_entity {
        Some(target_entity) => AbilityTarget::Entity(target_entity),
        None => AbilityTarget::None,
    };

    tick_counter.intent_seq += 1;
    submit_intent_logged(
        &stdb,
        &ack,
        entity_id,
        tick_counter.intent_seq,
        IntentAction::UseAbility(UseAbilityData {
            ability_id,
            target,
        }),
        tick_counter.last_tick,
        "UseAbility",
    );

    // Record cooldown + diagnostics + VFX flash.
    cooldowns.activate(ability_id, tick_counter.last_tick);
    diag.record_intent();
    if let Ok((bevy_entity, mat_handle)) = player_q.get_single() {
        crate::vfx::trigger_flash(&mut commands, &mut materials, bevy_entity, &mat_handle.0, ability_id);
    }
}
