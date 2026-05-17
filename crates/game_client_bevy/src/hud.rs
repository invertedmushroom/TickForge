use bevy::prelude::*;

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_hud);
        app.add_systems(Update, update_hud);
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
    player_q: Query<&Transform, With<crate::camera::LocalPlayer>>,
) {
    let Ok(mut text) = hud_q.get_single_mut() else { return };
    if let Ok(tf) = player_q.get_single() {
        **text = format!(
            "Jump Client\nPos: ({:.1}, {:.1}, {:.1})",
            tf.translation.x, tf.translation.y, tf.translation.z,
        );
    }
}
