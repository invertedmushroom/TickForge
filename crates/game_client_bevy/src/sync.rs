use bevy::prelude::*;
use bevy::utils::HashMap;
use game_client::module_bindings::*;
use game_client::module_bindings::Entity as RemoteEntity;
use spacetimedb_sdk::{DbContext, Table};

use crate::camera::LocalPlayer;
use crate::spacetime::{LocalPlayerEntity, StdbConnection};

pub struct SyncPlugin;

impl Plugin for SyncPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EntityMap>();
        app.add_systems(Update, (
            detect_local_player,
            sync_entities,
            sync_health,
            sync_health_bars,
            sync_target_lock_indicator,
            sync_npc_state_color,
        ).chain());
    }
}

/// Visible child mesh that makes the local player's body facing obvious in third-person tests.
#[derive(Component)]
pub struct FacingIndicator;

fn spawn_facing_indicator(
    commands: &mut Commands,
    parent: bevy::ecs::entity::Entity,
    meshes: &EntityMeshes,
) {
    commands.entity(parent).with_children(|child| {
        child.spawn((
            Mesh3d(meshes.hazard_mesh.clone()),
            MeshMaterial3d(meshes.projectile_mat.clone()),
            Transform::from_xyz(0.0, 0.05, -0.6)
                .with_rotation(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2))
                .with_scale(Vec3::new(0.2, 1.6, 0.2)),
            FacingIndicator,
        ));
        child.spawn((
            Mesh3d(meshes.projectile_mesh.clone()),
            MeshMaterial3d(meshes.projectile_mat.clone()),
            Transform::from_xyz(0.0, 0.2, -1.15).with_scale(Vec3::splat(1.6)),
            FacingIndicator,
        ));
    });
}

/// Interpolation speed (units per second). Higher = snappier.
const INTERP_SPEED: f32 = 20.0;

/// Maps SpacetimeDB entity_id → Bevy Entity.
#[derive(Resource, Default)]
pub struct EntityMap {
    pub map: HashMap<u64, bevy::ecs::entity::Entity>,
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
    projectile_mesh: Handle<Mesh>,
    hazard_mesh: Handle<Mesh>,
    prop_mesh: Handle<Mesh>,
    player_mat: Handle<StandardMaterial>,
    npc_mat: Handle<StandardMaterial>,
    boss_mat: Handle<StandardMaterial>,
    local_player_mat: Handle<StandardMaterial>,
    npc_combat_mat: Handle<StandardMaterial>,
    npc_flee_mat: Handle<StandardMaterial>,
    projectile_mat: Handle<StandardMaterial>,
    hazard_mat: Handle<StandardMaterial>,
    prop_mat: Handle<StandardMaterial>,
}

impl FromWorld for EntityMeshes {
    fn from_world(world: &mut World) -> Self {
        let mut meshes = world.resource_mut::<Assets<Mesh>>();
        let player_mesh = meshes.add(Capsule3d::new(0.3, 1.0));
        let npc_mesh = meshes.add(Capsule3d::new(0.3, 1.0));
        let boss_mesh = meshes.add(Capsule3d::new(0.5, 1.5));
        let projectile_mesh = meshes.add(Sphere::new(0.15));
        let hazard_mesh = meshes.add(Cylinder::new(0.4, 0.1));
        let prop_mesh = meshes.add(Cuboid::new(1.0, 1.0, 1.0));

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
        let npc_combat_mat = materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.6, 0.1),
            ..default()
        });
        let npc_flee_mat = materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 1.0, 0.3),
            ..default()
        });
        let projectile_mat = materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.9, 0.2),
            emissive: LinearRgba::new(2.0, 1.8, 0.0, 1.0),
            ..default()
        });
        let hazard_mat = materials.add(StandardMaterial {
            base_color: Color::srgba(1.0, 0.2, 0.0, 0.7),
            emissive: LinearRgba::new(1.5, 0.3, 0.0, 1.0),
            alpha_mode: AlphaMode::Blend,
            ..default()
        });
        let prop_mat = materials.add(StandardMaterial {
            base_color: Color::srgb(0.6, 0.4, 0.2),
            ..default()
        });

        EntityMeshes {
            player_mesh,
            npc_mesh,
            boss_mesh,
            projectile_mesh,
            hazard_mesh,
            prop_mesh,
            player_mat,
            npc_mat,
            boss_mat,
            local_player_mat,
            npc_combat_mat,
            npc_flee_mat,
            projectile_mat,
            hazard_mat,
            prop_mat,
        }
    }
}

/// Detect which entity belongs to the local player (by checking the nearby_entities
/// view for our identity).
/// When detected, retroactively add LocalPlayer + green material to the already-spawned Bevy entity.
fn detect_local_player(
    stdb: Option<Res<StdbConnection>>,
    mut local_player: ResMut<LocalPlayerEntity>,
    entity_map: Res<EntityMap>,
    entity_meshes: Option<Res<EntityMeshes>>,
    mut commands: Commands,
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
    for entity in stdb.conn.db.nearby_entities().iter() {
        if entity.owner_identity.as_ref() == Some(&our_identity) {
            log::info!("Local player entity detected: {}", entity.entity_id);
            local_player.entity_id = Some(entity.entity_id);

            // Retroactively tag the Bevy entity if it was already spawned.
            if let Some(&bevy_entity) = entity_map.map.get(&entity.entity_id) {
                log::info!("Retroactively adding LocalPlayer to Bevy entity");
                commands.entity(bevy_entity).insert(LocalPlayer);
                if let Some(meshes) = entity_meshes {
                    commands.entity(bevy_entity).insert(
                        MeshMaterial3d(meshes.local_player_mat.clone()),
                    );
                    spawn_facing_indicator(&mut commands, bevy_entity, &meshes);
                }
            }
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
    time: Res<Time>,
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

    let nearby_entities: HashMap<u64, RemoteEntity> = stdb.conn.db.nearby_entities()
        .iter()
        .map(|entity| (entity.entity_id, entity))
        .collect();

    for row in stdb.conn.db.nearby_transforms().iter() {
        // Skip entities in terminal states (death cleanup may lag one frame).
        let entity_row = nearby_entities.get(&row.entity_id);
        if entity_row.as_ref().is_some_and(|e| matches!(e.state, EntityState::DespawnPending | EntityState::Removed)) {
            continue;
        }

        live_ids.insert(row.entity_id);

        // Look up entity kind from the nearby_entities view.
        let kind = entity_row
            .map(|e| e.kind)
            .unwrap_or(EntityKind::Player);

        let server_pos = Vec3::new(row.pos_x, row.pos_y, row.pos_z);
        let server_rot = Quat::from_xyzw(row.rot_x, row.rot_y, row.rot_z, row.rot_w);

        if let Some(&bevy_entity) = entity_map.map.get(&row.entity_id) {
            // Update existing Bevy entity.
            if let Ok(mut tf) = query.get_mut(bevy_entity) {
                // Frame-rate independent interpolation toward server position.
                let t = (INTERP_SPEED * time.delta_secs()).min(1.0);
                tf.translation = tf.translation.lerp(server_pos, t);
                tf.rotation = tf.rotation.slerp(server_rot, t);
            }
        } else {
            // Spawn new Bevy entity for this server entity.
            let is_local = local_player.entity_id == Some(row.entity_id);
            let (mesh, mat) = match kind {
                EntityKind::Player if is_local => (meshes.player_mesh.clone(), meshes.local_player_mat.clone()),
                EntityKind::Player => (meshes.player_mesh.clone(), meshes.player_mat.clone()),
                EntityKind::Npc => (meshes.npc_mesh.clone(), meshes.npc_mat.clone()),
                EntityKind::Boss => (meshes.boss_mesh.clone(), meshes.boss_mat.clone()),
                EntityKind::Projectile => (meshes.projectile_mesh.clone(), meshes.projectile_mat.clone()),
                EntityKind::Hazard => (meshes.hazard_mesh.clone(), meshes.hazard_mat.clone()),
                EntityKind::Prop => (meshes.prop_mesh.clone(), meshes.prop_mat.clone()),
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

            if is_local {
                spawn_facing_indicator(&mut commands, bevy_entity, &meshes);
            }
        }
    }

    // Remove Bevy entities that are no longer in the server's nearby set.
    let stale: Vec<u64> = entity_map.map.keys()
        .filter(|id| !live_ids.contains(*id))
        .copied()
        .collect();
    for id in stale {
        if let Some(bevy_entity) = entity_map.map.remove(&id) {
            commands.entity(bevy_entity).despawn_recursive();
        }
    }
}

/// Sync health from the nearby_health view to Bevy Health components.
fn sync_health(
    stdb: Option<Res<StdbConnection>>,
    entity_map: Res<EntityMap>,
    mut query: Query<&mut Health, With<ServerEntity>>,
) {
    let Some(stdb) = stdb else { return };

    for row in stdb.conn.db.nearby_health().iter() {
        if let Some(&bevy_entity) = entity_map.map.get(&row.entity_id) {
            if let Ok(mut hp) = query.get_mut(bevy_entity) {
                hp.hp = row.hp;
                hp.max_hp = row.max_hp;
            }
        }
    }
}

// ── Health bars ──────────────────────────────────────────────────────

/// Tag for the health bar background (child of a 3D entity).
#[derive(Component)]
struct HealthBarBg;

/// Tag for the health bar fill (child of health bar background).
#[derive(Component)]
struct HealthBarFill {
    parent_server_entity: u64,
}

/// Spawn / update floating health bars above entities.
fn sync_health_bars(
    mut commands: Commands,
    server_q: Query<(bevy::ecs::entity::Entity, &ServerEntity, &Health, Option<&Children>)>,
    mut fill_q: Query<(&HealthBarFill, &mut Node, &mut BackgroundColor)>,
    bg_q: Query<&HealthBarBg>,
) {
    for (bevy_entity, se, hp, children) in server_q.iter() {
        // Check if this entity already has a health bar child.
        let has_bar = children.map_or(false, |ch| ch.iter().any(|c| bg_q.get(*c).is_ok()));

        if !has_bar && hp.max_hp > 0.0 {
            // Spawn health bar UI as child of 3D entity.
            commands.entity(bevy_entity).with_children(|parent| {
                parent.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        width: Val::Px(60.0),
                        height: Val::Px(6.0),
                        left: Val::Px(-30.0),
                        top: Val::Px(-80.0),
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.2, 0.2, 0.2, 0.7)),
                    HealthBarBg,
                )).with_children(|bar_parent| {
                    bar_parent.spawn((
                        Node {
                            width: Val::Percent(100.0),
                            height: Val::Percent(100.0),
                            ..default()
                        },
                        BackgroundColor(Color::srgb(0.1, 0.9, 0.1)),
                        HealthBarFill {
                            parent_server_entity: se.entity_id,
                        },
                    ));
                });
            });
        }

        // Update existing health bar fill width.
        for (fill, mut node, mut bg) in fill_q.iter_mut() {
            if fill.parent_server_entity == se.entity_id && hp.max_hp > 0.0 {
                let frac = (hp.hp / hp.max_hp).clamp(0.0, 1.0);
                node.width = Val::Percent(frac * 100.0);
                // Color: green > yellow > red.
                let color = if frac > 0.5 {
                    Color::srgb(0.1, 0.9, 0.1)
                } else if frac > 0.25 {
                    Color::srgb(0.9, 0.9, 0.1)
                } else {
                    Color::srgb(0.9, 0.1, 0.1)
                };
                *bg = BackgroundColor(color);
            }
        }
    }
}

// ── Target lock indicator ────────────────────────────────────────────

/// Visual ring mesh spawned at the feet of the target-locked entity.
#[derive(Component)]
pub struct TargetLockRing;

fn sync_target_lock_indicator(
    mut commands: Commands,
    lock: Option<Res<crate::input::TargetLockState>>,
    entity_map: Res<EntityMap>,
    existing_rings: Query<bevy::ecs::entity::Entity, With<TargetLockRing>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let target = lock.and_then(|l| l.target_entity);

    // Despawn old rings.
    for ring in existing_rings.iter() {
        commands.entity(ring).despawn();
    }

    // Spawn ring under the locked target.
    let Some(target_id) = target else { return };
    let Some(&bevy_entity) = entity_map.map.get(&target_id) else { return };

    let ring_mesh = meshes.add(Torus::new(0.35, 0.5));
    let ring_mat = materials.add(StandardMaterial {
        base_color: Color::srgba(1.0, 0.3, 0.3, 0.6),
        emissive: LinearRgba::new(2.0, 0.4, 0.0, 1.0),
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        ..default()
    });

    commands.entity(bevy_entity).with_children(|parent| {
        parent.spawn((
            Mesh3d(ring_mesh),
            MeshMaterial3d(ring_mat),
            Transform::from_xyz(0.0, -0.4, 0.0),
            TargetLockRing,
        ));
    });
}

// ── NPC AI state color ──────────────────────────────────────────────

/// Tint NPC capsules based on their AI state from the npc_state table.
fn sync_npc_state_color(
    stdb: Option<Res<StdbConnection>>,
    entity_map: Res<EntityMap>,
    entity_meshes: Option<Res<EntityMeshes>>,
    _server_q: Query<(&ServerEntity, &MeshMaterial3d<StandardMaterial>)>,
    mut commands: Commands,
    entity_table: Query<&ServerEntity>,
) {
    let Some(stdb) = stdb else { return };
    let Some(meshes) = entity_meshes else { return };
    let nearby_entity_kinds: HashMap<u64, EntityKind> = stdb.conn.db.nearby_entities()
        .iter()
        .map(|entity| (entity.entity_id, entity.kind))
        .collect();

    for npc in stdb.conn.db.npc_state().iter() {
        let Some(&bevy_entity) = entity_map.map.get(&npc.entity_id) else { continue };

        // Only recolor NPC/Boss entities — skip players.
        let Ok(se) = entity_table.get(bevy_entity) else { continue };
        let is_npc = nearby_entity_kinds
            .get(&se.entity_id)
            .is_some_and(|kind| matches!(kind, EntityKind::Npc | EntityKind::Boss));
        if !is_npc { continue; }

        let mat = match npc.ai_state {
            NpcAiState::Combat => meshes.npc_combat_mat.clone(),
            NpcAiState::Flee => meshes.npc_flee_mat.clone(),
            _ => meshes.npc_mat.clone(),
        };

        commands.entity(bevy_entity).insert(MeshMaterial3d(mat));
    }
}
