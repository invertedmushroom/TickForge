use bevy::prelude::*;
use game_client::module_bindings::*;
use spacetimedb_sdk::Table;

use crate::spacetime::{LocalPlayerEntity, StdbConnection};

pub struct InventoryPlugin;

impl Plugin for InventoryPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InventoryVisible>();
        app.add_systems(Startup, spawn_inventory_panel);
        app.add_systems(Update, (toggle_inventory, update_inventory));
    }
}

#[derive(Resource, Default)]
struct InventoryVisible(bool);

#[derive(Component)]
struct InventoryPanel;

fn spawn_inventory_panel(mut commands: Commands) {
    commands.spawn((
        Text::new(""),
        TextFont {
            font_size: 13.0,
            ..default()
        },
        TextColor(Color::srgba(0.95, 0.9, 0.7, 0.95)),
        Node {
            position_type: PositionType::Absolute,
            right: Val::Px(10.0),
            bottom: Val::Px(120.0),
            max_width: Val::Px(300.0),
            ..default()
        },
        BackgroundColor(Color::srgba(0.08, 0.06, 0.04, 0.85)),
        Visibility::Hidden,
        InventoryPanel,
    ));
}

fn toggle_inventory(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut vis: ResMut<InventoryVisible>,
    mut query: Query<&mut Visibility, With<InventoryPanel>>,
) {
    if keyboard.just_pressed(KeyCode::KeyI) {
        vis.0 = !vis.0;
        if let Ok(mut v) = query.get_single_mut() {
            *v = if vis.0 {
                Visibility::Visible
            } else {
                Visibility::Hidden
            };
        }
    }
}

fn update_inventory(
    vis: Res<InventoryVisible>,
    stdb: Option<Res<StdbConnection>>,
    local_player: Option<Res<LocalPlayerEntity>>,
    mut query: Query<&mut Text, With<InventoryPanel>>,
) {
    if !vis.0 {
        return;
    }
    let Ok(mut text) = query.get_single_mut() else {
        return;
    };
    let Some(stdb) = stdb else {
        **text = "Inventory: not connected".into();
        return;
    };
    let Some(entity_id) = local_player.and_then(|lp| lp.entity_id) else {
        **text = "Inventory: no player".into();
        return;
    };

    let mut lines = vec!["--- Inventory (I) ---".to_string()];

    // Equipment
    let mut equips: Vec<_> = stdb
        .conn
        .db
        .player_equipment()
        .iter()
        .filter(|e| e.owner_entity == entity_id)
        .collect();
    equips.sort_by_key(|e| format!("{:?}", e.slot));

    if equips.is_empty() {
        lines.push("Equipment: (empty)".into());
    } else {
        lines.push("Equipment:".into());
        for e in &equips {
            lines.push(format!("  {:?}: item #{}", e.slot, e.item_id));
        }
    }

    // Inventory bag
    let mut items: Vec<_> = stdb
        .conn
        .db
        .player_inventory()
        .iter()
        .filter(|i| i.owner_entity == entity_id)
        .collect();
    items.sort_by_key(|i| i.slot_index);

    if items.is_empty() {
        lines.push("Bag: (empty)".into());
    } else {
        lines.push(format!("Bag ({} slots):", items.len()));
        for i in &items {
            lines.push(format!(
                "  [{}] item #{} x{}",
                i.slot_index, i.item_id, i.quantity
            ));
        }
    }

    **text = lines.join("\n");
}
