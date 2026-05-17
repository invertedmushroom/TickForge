use bevy::prelude::*;
use game_client::module_bindings::*;

use crate::spacetime::{LocalPlayerEntity, StdbConnection, TickCounter};

pub struct InputPlugin;

impl Plugin for InputPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (handle_movement, handle_abilities));
    }
}

/// WASD movement → submit_intent(Move) calls.
fn handle_movement(
    keyboard: Res<ButtonInput<KeyCode>>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    mut tick_counter: ResMut<TickCounter>,
) {
    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else { return };

    let mut dir = Vec3::ZERO;
    if keyboard.pressed(KeyCode::KeyW) { dir.z -= 1.0; }
    if keyboard.pressed(KeyCode::KeyS) { dir.z += 1.0; }
    if keyboard.pressed(KeyCode::KeyA) { dir.x -= 1.0; }
    if keyboard.pressed(KeyCode::KeyD) { dir.x += 1.0; }

    if dir == Vec3::ZERO {
        // Send Stop on key release.
        if keyboard.any_just_released([KeyCode::KeyW, KeyCode::KeyS, KeyCode::KeyA, KeyCode::KeyD]) {
            tick_counter.intent_seq += 1;
            let _ = stdb.conn.reducers.submit_intent(
                entity_id,
                tick_counter.intent_seq,
                IntentAction::Stop,
                tick_counter.last_tick,
            );
        }
        return;
    }

    let dir = dir.normalize();
    tick_counter.intent_seq += 1;
    let _ = stdb.conn.reducers.submit_intent(
        entity_id,
        tick_counter.intent_seq,
        IntentAction::Move(MoveDir { dir_x: dir.x, dir_y: 0.0, dir_z: dir.z }),
        tick_counter.last_tick,
    );
}

/// Ability keys: 1 = Slash, 2 = Fireball, 3 = Smash.
fn handle_abilities(
    keyboard: Res<ButtonInput<KeyCode>>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    mut tick_counter: ResMut<TickCounter>,
) {
    let Some(stdb) = stdb else { return };
    let Some(entity_id) = local_player.entity_id else { return };

    let ability_id = if keyboard.just_pressed(KeyCode::Digit1) {
        Some(1u32) // Slash
    } else if keyboard.just_pressed(KeyCode::Digit2) {
        Some(2) // Fireball
    } else if keyboard.just_pressed(KeyCode::Digit3) {
        Some(3) // Smash
    } else {
        None
    };

    let Some(ability_id) = ability_id else { return };

    tick_counter.intent_seq += 1;
    let _ = stdb.conn.reducers.submit_intent(
        entity_id,
        tick_counter.intent_seq,
        IntentAction::UseAbility(UseAbilityData {
            ability_id,
            target: AbilityTarget::None, // Self-cast / forward direction for now.
        }),
        tick_counter.last_tick,
    );
}
