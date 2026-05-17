use bevy::prelude::*;

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, (spawn_hud, spawn_buff_bar));
        app.add_systems(Update, (update_hud, update_buff_bar));
    }
}

/// Tag for the debug HUD text node.
#[derive(Component)]
struct HudText;

fn spawn_hud(mut commands: Commands) {
    commands.spawn((
        Text::new("Jump Client\nConnecting..."),
        TextFont {
            font_size: 18.0,
            ..default()
        },
        TextColor(Color::WHITE),
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(10.0),
            top: Val::Px(10.0),
            ..default()
        },
        HudText,
    ));
}

fn update_hud(
    mut hud_q: Query<&mut Text, With<HudText>>,
    #[cfg(feature = "connected")]
    player_q: Query<(&Transform, Option<&crate::sync::Health>), With<crate::camera::LocalPlayer>>,
    #[cfg(not(feature = "connected"))]
    player_q: Query<&Transform, With<crate::camera::LocalPlayer>>,
    #[cfg(feature = "connected")]
    ack: Option<Res<crate::input::IntentAckStats>>,
    #[cfg(feature = "connected")]
    lock: Option<Res<crate::input::TargetLockState>>,
) {
    let Ok(mut text) = hud_q.get_single_mut() else { return };

    #[cfg(feature = "connected")]
    let ack_line = match ack {
        Some(ack) => format!(
            "Intents S/A/R: {}/{}/{}",
            ack.sent(),
            ack.accepted(),
            ack.rejected()
        ),
        None => "Intents S/A/R: -/-/-".to_string(),
    };

    #[cfg(feature = "connected")]
    let lock_line = match lock.and_then(|l| l.target_entity) {
        Some(entity_id) => format!("Target Lock: #{entity_id}"),
        None => "Target Lock: none".to_string(),
    };

    #[cfg(not(feature = "connected"))]
    let ack_line = "Intents S/A/R: offline".to_string();
    #[cfg(not(feature = "connected"))]
    let lock_line = "Target Lock: offline".to_string();

    #[cfg(feature = "connected")]
    match player_q.get_single() {
        Ok((tf, hp)) => {
            let hp_line = match hp {
                Some(h) => format!("HP: {:.0}/{:.0}", h.hp, h.max_hp),
                None => "HP: --/--".to_string(),
            };
            **text = format!(
                "Jump Client\nPos: ({:.1}, {:.1}, {:.1})\n{}\n{}\n{}",
                tf.translation.x, tf.translation.y, tf.translation.z,
                hp_line,
                ack_line,
                lock_line,
            );
        }
        Err(_) => {
            **text = format!("Jump Client\nWaiting for player...\n{}\n{}", ack_line, lock_line);
        }
    }

    #[cfg(not(feature = "connected"))]
    match player_q.get_single() {
        Ok(tf) => {
            **text = format!(
                "Jump Client\nPos: ({:.1}, {:.1}, {:.1})\n{}\n{}",
                tf.translation.x, tf.translation.y, tf.translation.z,
                ack_line,
                lock_line,
            );
        }
        Err(_) => {
            **text = format!("Jump Client\nWaiting for player...\n{}\n{}", ack_line, lock_line);
        }
    }
}

// ── Buff bar ─────────────────────────────────────────────────────────

/// Tag for the buff bar text node.
#[derive(Component)]
struct BuffBarText;

fn spawn_buff_bar(mut commands: Commands) {
    commands.spawn((
        Text::new(""),
        TextFont {
            font_size: 14.0,
            ..default()
        },
        TextColor(Color::srgba(0.5, 1.0, 0.5, 0.9)),
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(10.0),
            top: Val::Px(120.0),
            max_width: Val::Px(300.0),
            ..default()
        },
        BuffBarText,
    ));
}

/// Update the buff bar text with active buffs on the local player.
fn update_buff_bar(
    mut text_q: Query<&mut Text, With<BuffBarText>>,
    #[cfg(feature = "connected")]
    stdb: Option<Res<crate::spacetime::StdbConnection>>,
    #[cfg(feature = "connected")]
    local_player: Option<Res<crate::spacetime::LocalPlayerEntity>>,
    #[cfg(feature = "connected")]
    tick_counter: Option<Res<crate::spacetime::TickCounter>>,
) {
    let Ok(mut text) = text_q.get_single_mut() else { return };

    #[cfg(feature = "connected")]
    {
        use game_client::module_bindings::*;
        use spacetimedb_sdk::Table;

        let Some(stdb) = stdb else {
            **text = String::new();
            return;
        };
        let Some(local) = local_player else {
            **text = String::new();
            return;
        };
        let Some(entity_id) = local.entity_id else {
            **text = String::new();
            return;
        };
        let current_tick = tick_counter.map(|tc| tc.last_tick).unwrap_or(0);

        let mut lines = Vec::new();
        for buff in stdb.conn.db.active_buff().iter() {
            if buff.entity_id != entity_id {
                continue;
            }
            let remaining = match buff.expires_at_tick {
                Some(exp) if exp > current_tick => {
                    let ticks_left = exp - current_tick;
                    format!("{}s", ticks_left / 20) // 20 Hz tick rate
                }
                Some(_) => "expiring".into(),
                None => "∞".into(),
            };
            lines.push(format!(
                "Buff #{} x{} ({})",
                buff.buff_id, buff.stacks, remaining
            ));
        }

        if lines.is_empty() {
            **text = String::new();
        } else {
            **text = format!("Buffs:\n{}", lines.join("\n"));
        }
    }

    #[cfg(not(feature = "connected"))]
    {
        **text = String::new();
    }
}
