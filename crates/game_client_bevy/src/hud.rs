use bevy::prelude::*;

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, (spawn_hud, spawn_buff_bar, spawn_crosshair));
        app.add_systems(Update, (update_hud, update_buff_bar, update_crosshair_color));
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

/// Tag for the crosshair reticle so we can update its color.
#[derive(Component)]
struct CrosshairReticle;

/// TERA-style reticle: small dot crosshair, offset slightly above screen center
/// to match the perceived horizon in a third-person orbit camera.
const RETICLE_TOP: f32 = 46.5; // % from top — ~3.5% above center

fn spawn_crosshair(mut commands: Commands) {
    // Center dot
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(52.0),
            top: Val::Percent(RETICLE_TOP),
            margin: UiRect {
                left: Val::Px(-4.0),
                top: Val::Px(-4.0),
                ..default()
            },
            width: Val::Px(8.0),
            height: Val::Px(8.0),
            border: UiRect::all(Val::Px(1.0)),
            ..default()
        },
        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.9)),
        BorderColor(Color::srgba(0.0, 0.0, 0.0, 0.6)),
        CrosshairReticle,
    ));
    // Horizontal left tick
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(50.0),
            top: Val::Percent(RETICLE_TOP),
            margin: UiRect {
                left: Val::Px(-14.0),
                top: Val::Px(-1.0),
                ..default()
            },
            width: Val::Px(8.0),
            height: Val::Px(2.0),
            ..default()
        },
        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.7)),
        CrosshairReticle,
    ));
    // Horizontal right tick
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(50.0),
            top: Val::Percent(RETICLE_TOP),
            margin: UiRect {
                left: Val::Px(6.0),
                top: Val::Px(-1.0),
                ..default()
            },
            width: Val::Px(8.0),
            height: Val::Px(2.0),
            ..default()
        },
        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.7)),
        CrosshairReticle,
    ));
    // Vertical top tick
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(50.0),
            top: Val::Percent(RETICLE_TOP),
            margin: UiRect {
                left: Val::Px(-1.0),
                top: Val::Px(-14.0),
                ..default()
            },
            width: Val::Px(2.0),
            height: Val::Px(8.0),
            ..default()
        },
        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.7)),
        CrosshairReticle,
    ));
    // Vertical bottom tick
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(50.0),
            top: Val::Percent(RETICLE_TOP),
            margin: UiRect {
                left: Val::Px(-1.0),
                top: Val::Px(6.0),
                ..default()
            },
            width: Val::Px(2.0),
            height: Val::Px(8.0),
            ..default()
        },
        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.7)),
        CrosshairReticle,
    ));
}

/// Update crosshair color: yellow during lock-on session, red when soft-targeting, white otherwise.
#[cfg(feature = "connected")]
fn update_crosshair_color(
    crosshair: Res<crate::input::CrosshairAim>,
    lock_on: Res<crate::input::LockOnSession>,
    mut query: Query<&mut BackgroundColor, With<CrosshairReticle>>,
) {
    let color = if lock_on.active_ability.is_some() {
        // Lock-on tagging mode: yellow with orange tint when hovering a target.
        if crosshair.soft_target.is_some() {
            Color::srgba(1.0, 0.7, 0.1, 0.95)
        } else {
            Color::srgba(1.0, 0.9, 0.2, 0.95)
        }
    } else if crosshair.soft_target.is_some() {
        Color::srgba(1.0, 0.3, 0.3, 0.95)
    } else {
        Color::srgba(1.0, 1.0, 1.0, 0.9)
    };
    for mut bg in query.iter_mut() {
        bg.0 = color;
    }
}

#[cfg(not(feature = "connected"))]
fn update_crosshair_color() {}

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
    #[cfg(feature = "connected")]
    crosshair: Option<Res<crate::input::CrosshairAim>>,
    #[cfg(feature = "connected")]
    lock_on: Option<Res<crate::input::LockOnSession>>,
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
    let aim_line = match &crosshair {
        Some(ch) => match ch.soft_target {
            Some(eid) => format!("Aim: #{eid}"),
            None => match &ch.ground_position {
                Some(p) => format!("Aim: ground ({:.1}, {:.1})", p.x, p.z),
                None => "Aim: --".to_string(),
            },
        },
        None => "Aim: --".to_string(),
    };

    #[cfg(not(feature = "connected"))]
    let aim_line = "Aim: offline".to_string();

    #[cfg(feature = "connected")]
    let lock_on_line = match lock_on.and_then(|lo| lo.active_ability) {
        Some(aid) => {
            let name = crate::ability_bar::all_abilities()
                .iter()
                .find(|a| a.id == aid)
                .map(|a| a.name.as_str())
                .unwrap_or("???");
            format!("⚡ LOCK-ON: {} — click to tag, press again to fire", name)
        }
        None => String::new(),
    };

    #[cfg(not(feature = "connected"))]
    let lock_on_line = String::new();

    #[cfg(feature = "connected")]
    match player_q.get_single() {
        Ok((tf, hp)) => {
            let forward = *tf.forward();
            let facing_yaw_deg = forward.x.atan2(-forward.z).to_degrees();
            let hp_line = match hp {
                Some(h) => format!("HP: {:.0}/{:.0}", h.hp, h.max_hp),
                None => "HP: --/--".to_string(),
            };
            let mut hud = format!(
                "Jump Client\nPos: ({:.1}, {:.1}, {:.1})\nFacing: ({:.2}, {:.2}) yaw {:.0}°\n{}\n{}\n{}\n{}",
                tf.translation.x, tf.translation.y, tf.translation.z,
                forward.x, forward.z, facing_yaw_deg,
                hp_line,
                ack_line,
                lock_line,
                aim_line,
            );
            if !lock_on_line.is_empty() {
                hud.push('\n');
                hud.push_str(&lock_on_line);
            }
            **text = hud;
        }
        Err(_) => {
            **text = format!("Jump Client\nWaiting for player...\n{}\n{}\n{}", ack_line, lock_line, aim_line);
        }
    }

    #[cfg(not(feature = "connected"))]
    match player_q.get_single() {
        Ok(tf) => {
            **text = format!(
                "Jump Client\nPos: ({:.1}, {:.1}, {:.1})\n{}\n{}\n{}",
                tf.translation.x, tf.translation.y, tf.translation.z,
                ack_line,
                lock_line,
                aim_line,
            );
        }
        Err(_) => {
            **text = format!("Jump Client\nWaiting for player...\n{}\n{}\n{}", ack_line, lock_line, aim_line);
        }
    }
}

// ── Buff bar ─────────────────────────────────────────────────────────

/// Parsed buff templates from `data/buffs.ron`, cached at first access.
pub fn all_buffs() -> &'static [game_core::combat::status::BuffTemplate] {
    use game_core::combat::status::BuffFile;
    static BUFF_DEFS: std::sync::OnceLock<Vec<game_core::combat::status::BuffTemplate>> =
        std::sync::OnceLock::new();
    BUFF_DEFS.get_or_init(|| {
        let src = include_str!("../../../data/buffs.ron");
        ron::from_str::<BuffFile>(src)
            .expect("data/buffs.ron embedded at compile time must be valid RON")
            .buffs
    })
}

/// Look up a buff's display name from the embedded definitions.
fn buff_name(id: u32) -> &'static str {
    all_buffs()
        .iter()
        .find(|b| b.buff_id == id)
        .map(|b| b.name.as_str())
        .unwrap_or("???")
}

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
    mut text_q: Query<(&mut Text, &mut TextColor), With<BuffBarText>>,
    #[cfg(feature = "connected")]
    stdb: Option<Res<crate::spacetime::StdbConnection>>,
    #[cfg(feature = "connected")]
    local_player: Option<Res<crate::spacetime::LocalPlayerEntity>>,
    #[cfg(feature = "connected")]
    tick_counter: Option<Res<crate::spacetime::TickCounter>>,
) {
    let Ok((mut text, mut text_color)) = text_q.get_single_mut() else { return };

    #[cfg(feature = "connected")]
    {
        use game_client::module_bindings::*;
        use game_core::combat::status::BuffKind;
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

        let mut boons = Vec::new();
        let mut conditions = Vec::new();

        for buff in stdb.conn.db.active_buff().iter() {
            if buff.entity_id != entity_id {
                continue;
            }
            let remaining = match buff.expires_at_tick {
                Some(exp) if exp > current_tick => {
                    let secs_left = (exp - current_tick) / 20; // 20 Hz tick rate
                    format!("{secs_left}s")
                }
                Some(_) => "expiring".into(),
                None => "∞".into(),
            };

            let name = buff_name(buff.buff_id);
            let stacks = if buff.stacks > 1 {
                format!(" x{}", buff.stacks)
            } else {
                String::new()
            };
            let line = format!("{name}{stacks} ({remaining})");

            // Classify by buff_kind from the template definitions.
            let kind = all_buffs()
                .iter()
                .find(|b| b.buff_id == buff.buff_id)
                .map(|b| b.buff_kind)
                .unwrap_or(BuffKind::Boon);

            match kind {
                BuffKind::Boon => boons.push(line),
                BuffKind::Condition => conditions.push(line),
            }
        }

        if boons.is_empty() && conditions.is_empty() {
            **text = String::new();
        } else {
            let mut parts = Vec::new();
            if !boons.is_empty() {
                parts.push(format!("Boons: {}", boons.join(", ")));
            }
            if !conditions.is_empty() {
                parts.push(format!("Conditions: {}", conditions.join(", ")));
            }
            **text = parts.join("\n");

            // Tint the text: green if only boons, red if only conditions, yellow if both.
            if boons.is_empty() {
                text_color.0 = Color::srgba(1.0, 0.4, 0.4, 0.9);
            } else if conditions.is_empty() {
                text_color.0 = Color::srgba(0.5, 1.0, 0.5, 0.9);
            } else {
                text_color.0 = Color::srgba(1.0, 0.9, 0.4, 0.9);
            }
        }
    }

    #[cfg(not(feature = "connected"))]
    {
        **text = String::new();
    }
}
