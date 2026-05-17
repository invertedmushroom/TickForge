use bevy::prelude::*;
use bevy::utils::HashMap;
use game_client::module_bindings::*;
use spacetimedb_sdk::{DbContext, Table};

use crate::camera::LocalPlayer;
use crate::spacetime::{LocalPlayerEntity, StdbConnection};

pub struct SyncPlugin;

impl Plugin for SyncPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EntityMap>();
        app.add_systems(Update, (detect_local_player, sync_entities, sync_health).chain());
    }
}

/// Maps SpacetimeDB entity_id → Bevy Entity.
#[derive(Resource, Default)]
struct EntityMap {
    map: HashMap<u64, bevy::ecs::entity::Entity>,
}

/// Tag linking a Bevy entity to its SpacetimeDB entity_id.
#[allow(dead_code)]
#[derive(Component)]
pub struct ServerEntity {
    pub entity_id: u64,
}

/// Health component synced from the server.
#[derive(Component, Default)]
pub struct Health {
    pub hp: f32,
    pub max_hp: f32,
}

/// Cached mesh/material handles so we don't recreate them every frame.
#[derive(Resource)]
struct EntityMeshes {
    player_mesh: Handle<Mesh>,
    npc_mesh: Handle<Mesh>,
    boss_mesh: Handle<Mesh>,
    player_mat: Handle<StandardMaterial>,
    npc_mat: Handle<StandardMaterial>,
    boss_mat: Handle<StandardMaterial>,
    local_player_mat: Handle<StandardMaterial>,
}

impl FromWorld for EntityMeshes {
    fn from_world(world: &mut World) -> Self {
        let mut meshes = world.resource_mut::<Assets<Mesh>>();
        let player_mesh = meshes.add(Capsule3d::new(0.3, 1.0));
        let npc_mesh = meshes.add(Capsule3d::new(0.3, 1.0));
        let boss_mesh = meshes.add(Capsule3d::new(0.5, 1.5));

        let mut materials = world.resource_mut::<Assets<StandardMaterial>>();
        let player_mat = materials.add(StandardMaterial {
            base_color: Color::srgb(0.2, 0.5, 1.0),
            ..default()
        });
        let npc_mat = materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.3, 0.3),
            ..default()
        });
        let boss_mat = materials.add(StandardMaterial {
            base_color: Color::srgb(0.8, 0.1, 0.8),
            ..default()
        });
        let local_player_mat = materials.add(StandardMaterial {
            base_color: Color::srgb(0.1, 1.0, 0.3),
            ..default()
        });

        EntityMeshes {
            player_mesh,
            npc_mesh,
            boss_mesh,
            player_mat,
            npc_mat,
            boss_mat,
            local_player_mat,
        }
    }
}

/// Detect which entity belongs to the local player (by checking entity table for our identity).
fn detect_local_player(
    stdb: Option<Res<StdbConnection>>,
    mut local_player: ResMut<LocalPlayerEntity>,
) {
    let Some(stdb) = stdb else { return };
    if local_player.entity_id.is_some() {
        return;
    }
    if !stdb.connected.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }

    // Find our entity by matching owner_identity to our connection identity.
    let our_identity = stdb.conn.identity();
    for entity in stdb.conn.db.entity().iter() {
        if entity.owner_identity.as_ref() == Some(&our_identity) {
            log::info!("Local player entity detected: {}", entity.entity_id);
            local_player.entity_id = Some(entity.entity_id);
            return;
        }
    }
}

/// Sync SpacetimeDB transform rows → Bevy entities.
fn sync_entities(
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    mut entity_map: ResMut<EntityMap>,
    mut commands: Commands,
    mut query: Query<&mut Transform, With<ServerEntity>>,
    entity_meshes: Option<Res<EntityMeshes>>,
) {
    let Some(stdb) = stdb else { return };

    // Initialize mesh resources on first access.
    if entity_meshes.is_none() {
        commands.init_resource::<EntityMeshes>();
        return;
    }
    let meshes = entity_meshes.unwrap();

    // Collect current server entity IDs.
    let mut live_ids: bevy::utils::HashSet<u64> = bevy::utils::HashSet::new();

    for row in stdb.conn.db.nearby_transforms().iter() {
        live_ids.insert(row.entity_id);

        // Look up entity kind from entity table.
        let kind = stdb.conn.db.entity().entity_id().find(&row.entity_id)
            .map(|e| e.kind)
            .unwrap_or(EntityKind::Player);

        let server_pos = Vec3::new(row.pos_x, row.pos_y, row.pos_z);
        let server_rot = Quat::from_xyzw(row.rot_x, row.rot_y, row.rot_z, row.rot_w);

        if let Some(&bevy_entity) = entity_map.map.get(&row.entity_id) {
            // Update existing Bevy entity.
            if let Ok(mut tf) = query.get_mut(bevy_entity) {
                // Smooth interpolation toward server position.
                tf.translation = tf.translation.lerp(server_pos, 0.3);
                tf.rotation = tf.rotation.slerp(server_rot, 0.3);
            }
        } else {
            // Spawn new Bevy entity for this server entity.
            let is_local = local_player.entity_id == Some(row.entity_id);
            let (mesh, mat) = match kind {
                EntityKind::Player if is_local => (meshes.player_mesh.clone(), meshes.local_player_mat.clone()),
                EntityKind::Player => (meshes.player_mesh.clone(), meshes.player_mat.clone()),
                EntityKind::Npc => (meshes.npc_mesh.clone(), meshes.npc_mat.clone()),
                EntityKind::Boss => (meshes.boss_mesh.clone(), meshes.boss_mat.clone()),
                _ => (meshes.npc_mesh.clone(), meshes.npc_mat.clone()),
            };

            let mut entity_cmd = commands.spawn((
                Mesh3d(mesh),
                MeshMaterial3d(mat),
                Transform::from_translation(server_pos).with_rotation(server_rot),
                ServerEntity { entity_id: row.entity_id },
                Health::default(),
            ));

            if is_local {
                entity_cmd.insert(LocalPlayer);
            }

            let bevy_entity = entity_cmd.id();
            entity_map.map.insert(row.entity_id, bevy_entity);
        }
    }

    // Remove Bevy entities that are no longer in the server's nearby set.
    let stale: Vec<u64> = entity_map.map.keys()
        .filter(|id| !live_ids.contains(*id))
        .copied()
        .collect();
    for id in stale {
        if let Some(bevy_entity) = entity_map.map.remove(&id) {
            commands.entity(bevy_entity).despawn();
        }
    }
}

/// Sync health from entity_health table to Bevy Health component.
fn sync_health(
    stdb: Option<Res<StdbConnection>>,
    entity_map: Res<EntityMap>,
    mut query: Query<&mut Health, With<ServerEntity>>,
) {
    let Some(stdb) = stdb else { return };

    for row in stdb.conn.db.entity_health().iter() {
        if let Some(&bevy_entity) = entity_map.map.get(&row.entity_id) {
            if let Ok(mut hp) = query.get_mut(bevy_entity) {
                hp.hp = row.hp;
                hp.max_hp = row.max_hp;
            }
        }
    }
}
