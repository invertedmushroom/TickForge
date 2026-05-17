use bevy::prelude::*;
use game_client::module_bindings::*;

use crate::camera::LocalPlayer;
use crate::spacetime::{LocalPlayerEntity, StdbConnection};

pub struct AdminPlugin;

impl Plugin for AdminPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_admin_hint);
        app.add_systems(Update, (admin_spawn_npc, admin_status_line));
    }
}

/// One-line help text for admin hotkeys (top-center).
#[derive(Component)]
struct AdminHint;

/// Transient status line for admin action feedback.
#[derive(Component)]
struct AdminStatusText;

#[derive(Resource)]
struct AdminStatus {
    message: String,
    remaining: f32,
}

fn spawn_admin_hint(mut commands: Commands) {
    // Hint bar at top center
    commands.spawn((
        Text::new("Admin: F10=NPC  F11=Boss  F12=Heal"),
        TextFont { font_size: 12.0, ..default() },
        TextColor(Color::srgba(0.7, 0.7, 0.7, 0.5)),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(2.0),
            left: Val::Percent(30.0),
            ..default()
        },
        AdminHint,
    ));

    // Status feedback line below hint
    commands.spawn((
        Text::new(""),
        TextFont { font_size: 14.0, ..default() },
        TextColor(Color::srgba(0.3, 1.0, 0.5, 0.9)),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(18.0),
            left: Val::Percent(30.0),
            ..default()
        },
        AdminStatusText,
    ));

    commands.insert_resource(AdminStatus {
        message: String::new(),
        remaining: 0.0,
    });
}

/// Admin hotkeys — spawn NPC/Boss, heal self.
fn admin_spawn_npc(
    keyboard: Res<ButtonInput<KeyCode>>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    player_q: Query<&Transform, With<LocalPlayer>>,
    mut status: ResMut<AdminStatus>,
) {
    let action = if keyboard.just_pressed(KeyCode::F10) {
        Some("npc")
    } else if keyboard.just_pressed(KeyCode::F11) {
        Some("boss")
    } else if keyboard.just_pressed(KeyCode::F12) {
        Some("heal")
    } else {
        None
    };
    let Some(action) = action else { return };

    let Some(stdb) = stdb else {
        status.message = "Not connected".into();
        status.remaining = 3.0;
        return;
    };

    match action {
        "npc" | "boss" => {
            // Position: 5 units in front of the player
            let (px, py, pz) = if let Ok(tf) = player_q.get_single() {
                let forward = tf.forward();
                let spawn_pos = tf.translation + *forward * 5.0;
                (spawn_pos.x, 1.0_f32, spawn_pos.z)
            } else {
                (0.0, 1.0, -5.0)
            };

            let (label, max_hp) = if action == "boss" {
                ("Boss", 500.0_f32)
            } else {
                ("NPC", 100.0_f32)
            };

            let result = stdb.conn.reducers.spawn_npc(px, py, pz, max_hp);
            // TODO: After next deploy with debug feature, use debug_spawn_boss
            // for boss spawns: stdb.conn.reducers.debug_spawn_boss(px, py, pz, max_hp)
            match result {
                Ok(()) => {
                    status.message = format!("Spawned {label} at ({px:.1}, {py:.1}, {pz:.1}) HP={max_hp}");
                    log::info!("{}", status.message);
                }
                Err(e) => {
                    status.message = format!("spawn failed: {e}");
                    log::warn!("{}", status.message);
                }
            }
        }
        "heal" => {
            // TODO: After next deploy with debug feature, call:
            //   stdb.conn.reducers.debug_set_hp(entity_id, 100.0, 100.0)
            // For now, log that the feature requires a redeploy.
            let Some(entity_id) = local_player.entity_id else {
                status.message = "No local player to heal".into();
                status.remaining = 3.0;
                return;
            };
            status.message = format!("Heal: debug_set_hp not yet deployed (entity #{entity_id})");
            log::info!("{}", status.message);
        }
        _ => {}
    }
    status.remaining = 4.0;
}

/// Fade admin status messages over time.
fn admin_status_line(
    time: Res<Time>,
    mut status: ResMut<AdminStatus>,
    mut text_q: Query<(&mut Text, &mut TextColor), With<AdminStatusText>>,
) {
    if status.remaining > 0.0 {
        status.remaining -= time.delta_secs();
    }

    let Ok((mut text, mut color)) = text_q.get_single_mut() else { return };
    if status.remaining > 0.0 {
        **text = status.message.clone();
        let alpha = status.remaining.min(1.0);
        color.0 = Color::srgba(0.3, 1.0, 0.5, alpha);
    } else {
        **text = String::new();
    }
}
