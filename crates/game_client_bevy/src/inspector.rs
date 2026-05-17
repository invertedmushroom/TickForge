use bevy::prelude::*;
use game_client::module_bindings::*;
use spacetimedb_sdk::Table;

use crate::input::TargetLockState;
use crate::spacetime::StdbConnection;

pub struct InspectorPlugin;

impl Plugin for InspectorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InspectorVisible>();
        app.add_systems(Startup, spawn_inspector_panel);
        app.add_systems(Update, (toggle_inspector, update_inspector));
    }
}

#[derive(Resource, Default)]
struct InspectorVisible(bool);

#[derive(Component)]
struct InspectorPanel;

fn spawn_inspector_panel(mut commands: Commands) {
    commands.spawn((
        Text::new(""),
        TextFont {
            font_size: 13.0,
            ..default()
        },
        TextColor(Color::srgba(0.9, 0.95, 1.0, 0.95)),
        Node {
            position_type: PositionType::Absolute,
            right: Val::Px(10.0),
            top: Val::Px(200.0),
            max_width: Val::Px(320.0),
            ..default()
        },
        BackgroundColor(Color::srgba(0.05, 0.05, 0.1, 0.85)),
        Visibility::Hidden,
        InspectorPanel,
    ));
}

fn toggle_inspector(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut vis_state: ResMut<InspectorVisible>,
    mut query: Query<&mut Visibility, With<InspectorPanel>>,
) {
    if keyboard.just_pressed(KeyCode::F4) {
        vis_state.0 = !vis_state.0;
        if let Ok(mut vis) = query.get_single_mut() {
            *vis = if vis_state.0 { Visibility::Visible } else { Visibility::Hidden };
        }
    }
}

fn update_inspector(
    vis: Res<InspectorVisible>,
    lock: Option<Res<TargetLockState>>,
    stdb: Option<Res<StdbConnection>>,
    mut query: Query<&mut Text, With<InspectorPanel>>,
) {
    if !vis.0 {
        return;
    }
    let Ok(mut text) = query.get_single_mut() else { return };
    let Some(stdb) = stdb else {
        **text = "Inspector: not connected".into();
        return;
    };
    let Some(target_id) = lock.and_then(|l| l.target_entity) else {
        **text = "--- Inspector (F4) ---\nNo target locked (Tab)".into();
        return;
    };

    let mut lines = vec![format!("--- Inspector (F4) ---\nEntity #{target_id}")];

    // Entity row
    if let Some(entity) = stdb.conn.db.nearby_entities().iter().find(|entity| entity.entity_id == target_id) {
        lines.push(format!("Kind: {:?}", entity.kind));
        lines.push(format!("State: {:?}", entity.state));
        lines.push(format!("Spawned tick: {}", entity.spawned_at_tick));
        if let Some(ref owner) = entity.owner_identity {
            let id_str = format!("{owner:?}");
            let short = if id_str.len() > 16 { &id_str[..16] } else { &id_str };
            lines.push(format!("Owner: {short}.."));
        }
    } else {
        lines.push("(no nearby entity row)".into());
    }

    // Transform
    if let Some(tf) = stdb.conn.db.nearby_transforms().iter().find(|t| t.entity_id == target_id) {
        lines.push(format!(
            "Pos: ({:.1}, {:.1}, {:.1})",
            tf.pos_x, tf.pos_y, tf.pos_z
        ));
        let speed = (tf.vel_x * tf.vel_x + tf.vel_y * tf.vel_y + tf.vel_z * tf.vel_z).sqrt();
        if speed > 0.01 {
            lines.push(format!("Speed: {speed:.1}"));
        }
    }

    // Health
    if let Some(hp) = stdb.conn.db.nearby_health().iter().find(|hp| hp.entity_id == target_id) {
        lines.push(format!("HP: {:.0}/{:.0}", hp.hp, hp.max_hp));
    }

    // NPC state
    if let Some(npc) = stdb.conn.db.npc_state().entity_id().find(&target_id) {
        lines.push(format!("AI: {:?}", npc.ai_state));
        if let Some(npc_target) = npc.target_entity {
            lines.push(format!("Aggro target: #{npc_target}"));
        }
    }

    // Buffs
    let buffs: Vec<_> = stdb.conn.db.active_buff().iter()
        .filter(|b| b.entity_id == target_id)
        .collect();
    if !buffs.is_empty() {
        lines.push(format!("Buffs ({})", buffs.len()));
        for b in &buffs {
            let exp = match b.expires_at_tick {
                Some(t) => format!("tick {t}"),
                None => "permanent".into(),
            };
            lines.push(format!("  #{} x{} ({exp})", b.buff_id, b.stacks));
        }
    }

    **text = lines.join("\n");
}
