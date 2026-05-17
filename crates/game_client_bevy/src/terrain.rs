//! Client-side terrain visual rendering.
//!
//! Watches the local player's `entity_layer` row and spawns the
//! corresponding glTF scene from `assets/terrain/{set_name}/{set_name}.gltf`
//! (where `set_name` is resolved from `data/layers.ron`).  Despawns
//! and re-spawns when the layer changes.
//!
//! Collision is handled server-side (trimesh on the worker). This
//! module only manages the visual mesh.

use bevy::prelude::*;
use game_client::module_bindings::EntityLayerTableAccess;
use game_schema::dungeon::WorldLayersFile;
use spacetimedb_sdk::Table;

use crate::dungeon_geometry::{ActiveDungeon, DefaultGround};
use crate::spacetime::{LocalPlayerEntity, StdbConnection};

pub struct TerrainPlugin;

impl Plugin for TerrainPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ActiveTerrain>();
        app.add_systems(Update, sync_terrain_visual);
    }
}

/// Tracks the currently-shown terrain set so we don't reload on every frame.
#[derive(Resource, Default)]
struct ActiveTerrain {
    /// Layer the terrain scene was spawned for.
    layer: Option<u32>,
    /// The root entity of the spawned gltf scene, if any.
    scene_entity: Option<Entity>,
}

/// Tag component so we can easily despawn the terrain scene.
#[derive(Component)]
struct TerrainScene;

/// Embedded `layers.ron` — same source the worker uses.
static LAYERS: std::sync::OnceLock<WorldLayersFile> = std::sync::OnceLock::new();

fn all_layers() -> &'static WorldLayersFile {
    LAYERS.get_or_init(|| {
        let src = include_str!("../../../data/layers.ron");
        ron::from_str::<WorldLayersFile>(src)
            .expect("data/layers.ron embedded at compile time must be valid RON")
    })
}

/// Return the client-side visual asset stem for a static layer, if any.
/// Prefers the explicit `client_visual` override; falls back to the
/// `terrain_set` name so layers that don't decouple visual from collision
/// still work without extra config.
fn visual_asset_for_layer(layer_id: u32) -> Option<&'static str> {
    let layer = all_layers().layers.iter().find(|l| l.layer_id == layer_id)?;
    layer
        .client_visual
        .as_deref()
        .or(layer.terrain_set.as_deref())
}

/// Watch for local-player layer changes and swap the terrain glTF scene.
fn sync_terrain_visual(
    stdb: Option<Res<StdbConnection>>,
    active_dungeon: Option<Res<ActiveDungeon>>,
    local_player: Res<LocalPlayerEntity>,
    mut active: ResMut<ActiveTerrain>,
    ground_query: Query<Entity, With<DefaultGround>>,
    mut commands: Commands,
    asset_server: Res<AssetServer>,
) {
    let Some(stdb) = stdb else { return };

    if active_dungeon.is_some() {
        despawn_terrain(&mut active, &mut commands);
        return;
    }

    let Some(entity_id) = local_player.entity_id else {
        // Player left — despawn any terrain scene.
        despawn_terrain(&mut active, &mut commands);
        show_default_ground(&ground_query, &mut commands);
        return;
    };

    // Resolve the layer the local player is currently on.
    let current_layer = stdb
        .conn
        .db
        .entity_layer()
        .iter()
        .find(|el| el.entity_id == entity_id)
        .map(|el| el.layer);

    let Some(layer_id) = current_layer else {
        despawn_terrain(&mut active, &mut commands);
        show_default_ground(&ground_query, &mut commands);
        return;
    };

    // Already showing the right scene — nothing to do.
    if active.layer == Some(layer_id) {
        if active.scene_entity.is_some() {
            hide_default_ground(&ground_query, &mut commands);
        }
        return;
    }

    // Layer changed — despawn old scene.
    despawn_terrain(&mut active, &mut commands);

    // Find the asset stem for this layer (client_visual override, else terrain_set).
    let Some(set_name) = visual_asset_for_layer(layer_id) else {
        // No visual on this layer (e.g. open_world with placeholder cuboid).
        active.layer = Some(layer_id);
        show_default_ground(&ground_query, &mut commands);
        return;
    };

    // Load the glTF scene from assets/terrain/{set_name}/{set_name}.gltf#Scene0
    let asset_path = format!("terrain/{set_name}/{set_name}.gltf#Scene0");
    log::info!("Layer {layer_id} → visual='{set_name}': loading '{asset_path}'");

    hide_default_ground(&ground_query, &mut commands);

    let scene_handle: Handle<Scene> = asset_server.load(asset_path.as_str());
    let entity = commands
        .spawn((
            SceneRoot(scene_handle),
            Transform::IDENTITY,
            TerrainScene,
        ))
        .id();

    active.layer = Some(layer_id);
    active.scene_entity = Some(entity);
}

fn despawn_terrain(active: &mut ActiveTerrain, commands: &mut Commands) {
    if let Some(ent) = active.scene_entity.take() {
        commands.entity(ent).despawn_recursive();
    }
    active.layer = None;
}

fn hide_default_ground(ground_query: &Query<Entity, With<DefaultGround>>, commands: &mut Commands) {
    for entity in ground_query.iter() {
        commands.entity(entity).insert(Visibility::Hidden);
    }
}

fn show_default_ground(ground_query: &Query<Entity, With<DefaultGround>>, commands: &mut Commands) {
    for entity in ground_query.iter() {
        commands.entity(entity).insert(Visibility::Visible);
    }
}
