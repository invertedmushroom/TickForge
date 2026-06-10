use bevy::prelude::*;
use game_client::module_bindings::*;
use spacetimedb_sdk::Table;

use crate::spacetime::{LocalPlayerEntity, StdbConnection};

pub struct InstancePanelPlugin;

impl Plugin for InstancePanelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InstancePanelVisible>();
        app.add_systems(Startup, spawn_instance_panel);
        app.add_systems(
            Update,
            (
                toggle_instance_panel,
                update_instance_panel,
                handle_instance_keys,
            ),
        );
    }
}

#[derive(Resource, Default)]
struct InstancePanelVisible(bool);

#[derive(Component)]
struct InstancePanel;

fn spawn_instance_panel(mut commands: Commands) {
    commands.spawn((
        Text::new(""),
        TextFont {
            font_size: 13.0,
            ..default()
        },
        TextColor(Color::srgba(0.85, 0.9, 1.0, 0.95)),
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(10.0),
            bottom: Val::Px(120.0),
            max_width: Val::Px(400.0),
            ..default()
        },
        BackgroundColor(Color::srgba(0.05, 0.05, 0.1, 0.85)),
        Visibility::Hidden,
        InstancePanel,
    ));
}

fn toggle_instance_panel(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut vis_state: ResMut<InstancePanelVisible>,
    mut query: Query<&mut Visibility, With<InstancePanel>>,
) {
    if keyboard.just_pressed(KeyCode::F5) {
        vis_state.0 = !vis_state.0;
        if let Ok(mut vis) = query.get_single_mut() {
            *vis = if vis_state.0 {
                Visibility::Visible
            } else {
                Visibility::Hidden
            };
        }
    }
}

fn update_instance_panel(
    vis_state: Res<InstancePanelVisible>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    mut query: Query<&mut Text, With<InstancePanel>>,
) {
    if !vis_state.0 {
        return;
    }

    let Ok(mut text) = query.get_single_mut() else {
        return;
    };

    let Some(stdb) = stdb else {
        **text = "--- Instance Panel (F5) ---\nNot connected".into();
        return;
    };

    let entity_id = local_player.entity_id.unwrap_or(0);

    // Current membership
    let membership = stdb
        .conn
        .db
        .instance_membership()
        .iter()
        .find(|m| m.entity_id == entity_id);

    // my_region is a server-scoped view — always 0 or 1 rows for the current player.
    let current_layer = stdb
        .conn
        .db
        .my_region()
        .iter()
        .next()
        .map(|r| r.layer)
        .unwrap_or(0);

    let mut lines = vec![
        "--- Instance Panel (F5) ---".to_string(),
        format!("Layer: {current_layer}"),
    ];

    match &membership {
        Some(m) => {
            lines.push(format!("In instance: #{} (F7=Leave)", m.instance_id));
        }
        None => {
            lines.push("Not in instance (F6=Create)".to_string());
        }
    }

    // List available instances
    let instances: Vec<_> = stdb
        .conn
        .db
        .instance()
        .iter()
        .filter(|i| i.state == InstanceState::Active || i.state == InstanceState::Pending)
        .collect();

    if instances.is_empty() {
        lines.push("No active instances".to_string());
    } else {
        lines.push(String::new());
        lines.push("Active instances:".to_string());
        for inst in &instances {
            let member_count = stdb
                .conn
                .db
                .instance_membership()
                .iter()
                .filter(|m| m.instance_id == inst.instance_id)
                .count();
            let state_label = match inst.state {
                InstanceState::Active => "Active",
                InstanceState::Pending => "Pending",
                _ => "?",
            };
            lines.push(format!(
                "  #{} [{}] layer={} {}/{} players  \"{}\"",
                inst.instance_id,
                state_label,
                inst.layer,
                member_count,
                inst.max_players,
                inst.template_id,
            ));
        }
    }

    **text = lines.join("\n");
}

/// F6 = create + join test instance, F7 = leave instance.
fn handle_instance_keys(
    keyboard: Res<ButtonInput<KeyCode>>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
) {
    let Some(stdb) = stdb else { return };

    if keyboard.just_pressed(KeyCode::F6) {
        let Some(entity_id) = local_player.entity_id else {
            log::warn!("debug_create_instance_for_template skipped: local entity not ready");
            return;
        };
        match stdb.conn.reducers.debug_create_instance_for_template(
            entity_id,
            "test_dungeon_01".into(),
            4,
        ) {
            Ok(()) => log::info!("debug_create_instance_for_template called (test_dungeon_01)"),
            Err(e) => log::warn!("debug_create_instance_for_template failed: {e}"),
        }
    }

    if keyboard.just_pressed(KeyCode::F7) {
        match stdb.conn.reducers.leave_instance() {
            Ok(()) => log::info!("leave_instance called"),
            Err(e) => log::warn!("leave_instance failed: {e}"),
        }
    }
}
