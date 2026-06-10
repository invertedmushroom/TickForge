use bevy::prelude::*;
use game_client::module_bindings::*;
use spacetimedb_sdk::Table;

use crate::encounter_cues::ActiveEncounterCues;
use crate::spacetime::{LocalPlayerEntity, StdbConnection, TickCounter};

/// Legacy F8 debug panel for boss/encounter visibility.
///
/// This panel is intentionally lightweight and hidden by default. Remove it
/// once the Bevy client has a more complete runtime diagnostics flow.
pub struct EncounterPanelPlugin;

impl Plugin for EncounterPanelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EncounterPanelVisible>();
        app.add_systems(Startup, spawn_encounter_panel);
        app.add_systems(Update, (toggle_encounter_panel, update_encounter_panel));
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
        TextColor(Color::srgba(0.85, 0.9, 1.0, 0.95)),
        Node {
            position_type: PositionType::Absolute,
            right: Val::Px(10.0),
            top: Val::Px(120.0),
            max_width: Val::Px(400.0),
            ..default()
        },
        BackgroundColor(Color::srgba(0.05, 0.05, 0.1, 0.85)),
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
    active_cues: Res<ActiveEncounterCues>,
    tick_counter: Option<Res<TickCounter>>,
    mut query: Query<&mut Text, With<EncounterPanel>>,
) {
    if !vis_state.0 {
        return;
    }

    let Ok(mut text) = query.get_single_mut() else {
        return;
    };

    let Some(stdb) = stdb else {
        **text = "--- F8 Debug Panel ---\nNot connected".into();
        return;
    };

    let entity_id = local_player.entity_id.unwrap_or(0);
    let my_region = stdb.conn.db.my_region().iter().next();
    let current_layer = my_region.as_ref().map(|r| r.layer).unwrap_or(0);
    let region_desc = my_region
        .as_ref()
        .map(|r| format!("L{} [{},{}]", r.layer, r.region_x, r.region_z))
        .unwrap_or_else(|| "unknown".to_string());

    let nearby_entities = stdb.conn.db.nearby_entities().count();
    let nearby_health = stdb.conn.db.nearby_health().count();
    let active_buff_count = stdb.conn.db.active_buff().count();
    let npc_state_count = stdb.conn.db.npc_state().count();
    let player_inventory_count = stdb.conn.db.player_inventory().count();
    let player_equipment_count = stdb.conn.db.player_equipment().count();
    let module_config_count = stdb.conn.db.module_config().count();
    let interactable_count = stdb.conn.db.interactable_config().count();
    let death_state_count = stdb.conn.db.death_state().count();
    let boss_phase_count = stdb.conn.db.boss_phase().count();
    let world_phase_count = stdb.conn.db.world_phase().count();
    let zone_counter_count = stdb.conn.db.zone_counter().count();
    let instance_count = stdb.conn.db.instance().count();
    let membership_count = stdb.conn.db.instance_membership().count();
    let entity_layer_count = stdb.conn.db.entity_layer().count();
    let encounter_add_count = stdb.conn.db.encounter_add().count();
    let entity_health_count = stdb.conn.db.entity_health().count();

    let mut lines = vec![
        "--- F8 Debug Panel ---".to_string(),
        format!("Local entity: {}", entity_id),
        format!("Region: {}", region_desc),
        format!("Layer: {}", current_layer),
        format!(
            "Nearby rows: entities={} health={}",
            nearby_entities, nearby_health
        ),
        format!(
            "Global rows: bosses={} worlds={} zones={} instances={} memberships={}",
            boss_phase_count,
            world_phase_count,
            zone_counter_count,
            instance_count,
            membership_count,
        ),
        format!(
            "Game state rows: buffs={} npcs={} deaths={} interact={} layers={}",
            active_buff_count,
            npc_state_count,
            death_state_count,
            interactable_count,
            entity_layer_count,
        ),
        format!(
            "Player rows: inv={} equip={} module_cfg={} encounter_add={} health={}",
            player_inventory_count,
            player_equipment_count,
            module_config_count,
            encounter_add_count,
            entity_health_count,
        ),
    ];

    let membership = stdb
        .conn
        .db
        .instance_membership()
        .iter()
        .find(|m| m.entity_id == entity_id);

    match membership {
        Some(m) => lines.push(format!("Member of instance #{}", m.instance_id)),
        None => lines.push("Not in instance".to_string()),
    }

    if boss_phase_count > 0 {
        lines.push(String::new());
        lines.push("Boss phases:".to_string());
        for boss_phase in stdb.conn.db.boss_phase().iter() {
            let hp_str = match stdb
                .conn
                .db
                .entity_health()
                .entity_id()
                .find(&boss_phase.boss_entity_id)
            {
                Some(h) => format!("{:.0}/{:.0}", h.hp, h.max_hp),
                None => "Dead/Missing".to_string(),
            };
            let phase_name = match boss_phase.phase {
                1 => "Phase 1",
                2 => "Phase 2",
                3 => "Phase 3",
                99 => "Enrage",
                _ => "Custom",
            };
            lines.push(format!(
                "  BOSS #{} {} HP={} entered={}",
                boss_phase.boss_entity_id, phase_name, hp_str, boss_phase.entered_at_tick,
            ));

            let adds: Vec<_> = stdb
                .conn
                .db
                .encounter_add()
                .iter()
                .filter(|a| a.boss_entity == boss_phase.boss_entity_id)
                .collect();

            if adds.is_empty() {
                lines.push("    Adds: None".to_string());
            } else {
                lines.push(format!("    Adds ({}):", adds.len()));
                for add in adds {
                    let add_hp = match stdb
                        .conn
                        .db
                        .entity_health()
                        .entity_id()
                        .find(&add.add_entity)
                    {
                        Some(h) => format!("{:.0}/{:.0}", h.hp, h.max_hp),
                        None => "Dead".to_string(),
                    };
                    let tags_str = if add.tags.is_empty() {
                        "[]".to_string()
                    } else {
                        format!("{:?}", add.tags)
                    };
                    lines.push(format!(
                        "      - #{} {} HP={} {}",
                        add.add_entity, add.archetype, add_hp, tags_str
                    ));
                }
            }
        }
    }

    if world_phase_count > 0 {
        lines.push(String::new());
        lines.push("World phases:".to_string());
        for wp in stdb.conn.db.world_phase().iter() {
            let layer = wp.zone_id / 1_000_000;
            let rem = wp.zone_id % 1_000_000;
            let rx = (rem / 1000) as i32 - 500;
            let rz = (rem % 1000) as i32 - 500;
            lines.push(format!("  L{} ({},{}) -> {}", layer, rx, rz, wp.phase_name));
        }
    }

    if zone_counter_count > 0 {
        lines.push(String::new());
        lines.push("Zone counters:".to_string());
        if let Some(region) = my_region {
            for zc in stdb.conn.db.zone_counter().iter().filter(|zc| {
                zc.layer == region.layer
                    && zc.region_x == region.region_x
                    && zc.region_z == region.region_z
            }) {
                lines.push(format!("  {} = {}", zc.counter_name, zc.value));
            }
        } else {
            for zc in stdb.conn.db.zone_counter().iter() {
                lines.push(format!(
                    "  L{} ({},{}) {} = {}",
                    zc.layer, zc.region_x, zc.region_z, zc.counter_name, zc.value
                ));
            }
        }
    }

    // ── Active encounter cues ──────────────────────────────
    let current_tick = tick_counter.map(|tc| tc.last_tick).unwrap_or(0);
    if !active_cues.cues.is_empty() {
        lines.push(String::new());
        lines.push(format!("Encounter Cues ({}):", active_cues.cues.len()));
        let mut sorted: Vec<_> = active_cues.cues.values().collect();
        sorted.sort_by(|a, b| a.cue_id.cmp(&b.cue_id));
        for cue in sorted {
            let ticks_left = cue.expires_at_tick.saturating_sub(current_tick);
            let anchor_str = match cue.anchor_entity {
                Some(id) => format!("#{id}"),
                None => "fixed".to_string(),
            };
            lines.push(format!(
                "  ✧ {} r={:.0}-{:.0}m anch={} -{:.1}s",
                cue.cue_id,
                cue.inner_radius,
                cue.outer_radius,
                anchor_str,
                ticks_left as f32 / 20.0,
            ));
        }
    }

    **text = lines.join("\n");
}
