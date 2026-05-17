use bevy::prelude::*;
use game_client::module_bindings::*;
use spacetimedb_sdk::Table;

use crate::spacetime::{LocalPlayerEntity, StdbConnection};

pub struct EncounterPanelPlugin;

impl Plugin for EncounterPanelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EncounterPanelVisible>();
        app.add_systems(Startup, spawn_encounter_panel);
        app.add_systems(
            Update,
            (toggle_encounter_panel, update_encounter_panel),
        );
    }
}

#[derive(Resource, Default)]
struct EncounterPanelVisible(bool);

#[derive(Component)]
struct EncounterPanel;

fn spawn_encounter_panel(mut commands: Commands) {
    commands.spawn((
        Text::new(""),
        TextFont {
            font_size: 13.0,
            ..default()
        },
        TextColor(Color::srgba(1.0, 0.85, 0.7, 0.95)),
        Node {
            position_type: PositionType::Absolute,
            right: Val::Px(10.0),
            top: Val::Px(120.0),
            max_width: Val::Px(400.0),
            ..default()
        },
        BackgroundColor(Color::srgba(0.1, 0.05, 0.05, 0.85)),
        Visibility::Hidden,
        EncounterPanel,
    ));
}

fn toggle_encounter_panel(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut vis_state: ResMut<EncounterPanelVisible>,
    mut query: Query<&mut Visibility, With<EncounterPanel>>,
) {
    if keyboard.just_pressed(KeyCode::F8) {
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

fn update_encounter_panel(
    vis_state: Res<EncounterPanelVisible>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    mut query: Query<&mut Text, With<EncounterPanel>>,
) {
    if !vis_state.0 {
        return;
    }

    let Ok(mut text) = query.get_single_mut() else {
        return;
    };

    let Some(stdb) = stdb else {
        **text = "--- Encounter Panel (F8) ---\nNot connected".into();
        return;
    };

    let _entity_id = local_player.entity_id.unwrap_or(0);

    let current_layer = stdb
        .conn
        .db
        .my_region()
        .iter()
        .next()
        .map(|r| r.layer)
        .unwrap_or(0);

    let mut lines = vec![
        "--- Encounter Panel (F8) ---".to_string(),
        format!("Layer: {current_layer}"),
    ];

    // Find all bosses in the local layer
    // We only want to show bosses that are in our layer (if we can infer it, otherwise show all and filter later)
    // boss_phase doesn't have layer, but entity_layer does.
    let mut active_bosses = Vec::new();
    for phase_info in stdb.conn.db.boss_phase().iter() {
        let is_in_my_layer = stdb.conn.db.entity_layer().entity_id().find(&phase_info.boss_entity_id).map(|el| el.layer) == Some(current_layer);
        
        if is_in_my_layer {
            active_bosses.push(phase_info);
        }
    }

    if active_bosses.is_empty() {
        lines.push("No active bosses in current layer.".to_string());
    } else {
        for boss_phase in active_bosses {
            lines.push(String::new());
            
            // Get HP
            let hp_str = match stdb.conn.db.entity_health().entity_id().find(&boss_phase.boss_entity_id) {
                Some(h) => format!("{:.0}/{:.0}", h.hp, h.max_hp),
                None => "Dead/Missing".to_string(),
            };

            // Format Phase Name
            let phase_name = match boss_phase.phase {
                1 => "Phase 1",
                2 => "Phase 2",
                3 => "Phase 3",
                99 => "Enrage",
                _ => "Custom",
            };

            lines.push(format!(
                "BOSS #{} [{}] HP: {}",
                boss_phase.boss_entity_id, phase_name, hp_str
            ));

            // Find Adds
            let adds: Vec<_> = stdb.conn.db.encounter_add().iter().filter(|a| a.boss_entity == boss_phase.boss_entity_id).collect();
            
            if adds.is_empty() {
                lines.push("  Adds: None".to_string());
            } else {
                lines.push(format!("  Adds ({}):", adds.len()));
                for add in adds {
                    // Filter dead adds if they aren't removed immediately, but usually health is a good indicator
                    let add_hp = match stdb.conn.db.entity_health().entity_id().find(&add.add_entity) {
                        Some(h) => format!("{:.0}/{:.0}", h.hp, h.max_hp),
                        None => "Dead".to_string(),
                    };
                    let tags_str = if add.tags.is_empty() { "[]".to_string() } else { format!("{:?}", add.tags) };
                    lines.push(format!(
                        "    - #{} {} HP: {} {}",
                        add.add_entity, add.archetype, add_hp, tags_str
                    ));
                }
            }
        }
    }

    **text = lines.join("\n");
}
