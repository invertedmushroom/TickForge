use std::collections::VecDeque;

use bevy::prelude::*;
use bevy::utils::HashMap;
use game_client::module_bindings::Entity as RemoteEntity;
use game_client::module_bindings::*;
use game_core::physics_backend::BodyShape;
use spacetimedb_sdk::{DbContext, Table};

use crate::camera::GameCamera;
use crate::camera::LocalPlayer;
use crate::input::LastMoveDir;
use crate::spacetime::{LocalPlayerEntity, StdbConnection, TickCounter};

pub struct SyncPlugin;

#[derive(SystemSet, Debug, Hash, PartialEq, Eq, Clone)]
pub enum SyncSet {
    ApplyPresentation,
}

impl Plugin for SyncPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EntityMap>();
        app.init_resource::<NetSmoothingTelemetry>();
        app.add_systems(
            Update,
            (
                detect_local_player,
                sync_entities,
                sync_health,
                sync_health_bars,
                sync_name_tags,
                sync_target_lock_indicator,
                sync_aggro_indicator,
                sync_npc_state_color,
                sync_interactables,
            )
                .chain()
                .in_set(SyncSet::ApplyPresentation),
        );
    }
}

const SIM_TICKS_PER_SECOND: f32 = 20.0;
const REMOTE_BUFFER_TICKS: f32 = 2.0;
const MAX_EXTRAPOLATION_SECS: f32 = 0.15;
const MAX_SNAPSHOT_HISTORY: usize = 12;
const LOCAL_RECONCILE_RATE: f32 = 8.0;
const LOCAL_ROTATE_RATE: f32 = 10.0;
const LOCAL_SNAP_DISTANCE: f32 = 3.0;

/// EWMA decay applied per-sample to reconcile-error and snapshot-gap means.
/// Tuned for ~20 Hz sample rate so the reading reflects ≈ the last second of
/// activity without being drowned by transient spikes.
const TELEMETRY_EWMA_ALPHA: f32 = 0.05;

/// Smoothing/reconcile telemetry. Production client has no local Rapier, so
/// these metrics are the primary feedback channel for tuning REMOTE_BUFFER_TICKS,
/// MAX_EXTRAPOLATION_SECS, and LOCAL_RECONCILE_RATE under live network conditions.
///
/// All counters are cumulative since client start. EWMA fields decay toward
/// recent activity (see TELEMETRY_EWMA_ALPHA).
#[derive(Resource, Default, Debug, Clone)]
pub struct NetSmoothingTelemetry {
    /// How many remote samples needed to extrapolate past the latest snapshot.
    pub extrapolation_events: u64,
    /// Cumulative extrapolation time in seconds (clamped per-event to MAX_EXTRAPOLATION_SECS).
    pub extrapolation_secs_total: f32,
    /// Maximum extrapolation seconds observed in a single sample.
    pub extrapolation_secs_max: f32,
    /// Number of `reconcile_translation` calls on the local player.
    pub reconcile_samples: u64,
    /// EWMA of |authoritative - predicted| meters. Approximates current
    /// prediction-error magnitude for the local player.
    pub reconcile_err_ewma: f32,
    /// Largest single-frame |authoritative - predicted| meters seen.
    pub reconcile_err_max: f32,
    /// Times the LOCAL_SNAP_DISTANCE threshold was crossed (hard correction).
    pub snap_corrections: u64,
    /// EWMA of inter-snapshot tick gap. Steady state should hover near 1.0
    /// (one snapshot per simulation tick); higher values indicate dropped or
    /// merged snapshots between client and server.
    pub snapshot_gap_ewma: f32,
    /// Largest gap (in ticks) observed between successive snapshots for any entity.
    pub snapshot_gap_max: u64,
}

impl NetSmoothingTelemetry {
    fn record_extrapolation(&mut self, secs: f32) {
        self.extrapolation_events += 1;
        self.extrapolation_secs_total += secs;
        if secs > self.extrapolation_secs_max {
            self.extrapolation_secs_max = secs;
        }
    }

    fn record_reconcile(&mut self, error_m: f32, snapped: bool) {
        self.reconcile_samples += 1;
        if error_m > self.reconcile_err_max {
            self.reconcile_err_max = error_m;
        }
        // EWMA: new = α·sample + (1−α)·old.
        self.reconcile_err_ewma =
            TELEMETRY_EWMA_ALPHA * error_m + (1.0 - TELEMETRY_EWMA_ALPHA) * self.reconcile_err_ewma;
        if snapped {
            self.snap_corrections += 1;
        }
    }

    fn record_snapshot_gap(&mut self, gap_ticks: u64) {
        if gap_ticks == 0 {
            // Same-tick replacement, not a true gap.
            return;
        }
        if gap_ticks > self.snapshot_gap_max {
            self.snapshot_gap_max = gap_ticks;
        }
        self.snapshot_gap_ewma = TELEMETRY_EWMA_ALPHA * gap_ticks as f32
            + (1.0 - TELEMETRY_EWMA_ALPHA) * self.snapshot_gap_ewma;
    }
}

/// Visible child mesh that makes the local player's body facing obvious in third-person tests.
#[derive(Component)]
pub struct FacingIndicator;

#[derive(Component)]
pub struct AnimatedVisual;

#[derive(Component, Clone, Copy)]
pub struct VisualOwner(pub bevy::ecs::entity::Entity);

#[derive(Component, Default)]
pub struct PresentationMotion {
    pub velocity: Vec3,
    pub planar_speed: f32,
    pub turn_rate: f32,
}

#[derive(Component)]
struct PresentationBody(pub bevy::ecs::entity::Entity);

#[derive(Clone, Copy, Debug)]
struct TransformSnapshot {
    tick: f32,
    position: Vec3,
    rotation: Quat,
    velocity: Vec3,
}

#[derive(Component)]
pub struct SmoothingState {
    authoritative_pos: Vec3,
    authoritative_rot: Quat,
    authoritative_vel: Vec3,
    last_server_tick: u64,
    snapshots: VecDeque<TransformSnapshot>,
}

impl SmoothingState {
    fn from_snapshot(snapshot: TransformSnapshot) -> Self {
        let mut snapshots = VecDeque::new();
        snapshots.push_back(snapshot);
        Self {
            authoritative_pos: snapshot.position,
            authoritative_rot: snapshot.rotation,
            authoritative_vel: snapshot.velocity,
            last_server_tick: snapshot.tick as u64,
            snapshots,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SampledTransform {
    position: Vec3,
    rotation: Quat,
    velocity: Vec3,
}

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

fn should_show_facing_indicator(kind: EntityKind, is_local: bool) -> bool {
    is_local || matches!(kind, EntityKind::Npc | EntityKind::Boss)
}

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
    /// Per-`BodyShape` mesh handles. When an entity has an authored
    /// `body_shape` (via `NpcConfig` / `InteractableConfig`), look up the
    /// mesh here; otherwise fall back to the kind default above.
    body_shape_meshes: std::collections::HashMap<BodyShape, Handle<Mesh>>,
    player_mat: Handle<StandardMaterial>,
    npc_mat: Handle<StandardMaterial>,
    boss_mat: Handle<StandardMaterial>,
    local_player_mat: Handle<StandardMaterial>,
    npc_combat_mat: Handle<StandardMaterial>,
    npc_flee_mat: Handle<StandardMaterial>,
    projectile_mat: Handle<StandardMaterial>,
    hazard_mat: Handle<StandardMaterial>,
    prop_mat: Handle<StandardMaterial>,
    prop_active_mat: Handle<StandardMaterial>,
}

impl FromWorld for EntityMeshes {
    fn from_world(world: &mut World) -> Self {
        let mut meshes = world.resource_mut::<Assets<Mesh>>();
        let player_mesh = meshes.add(Capsule3d::new(0.3, 1.0));
        let npc_mesh = meshes.add(Capsule3d::new(0.3, 1.0));
        let boss_mesh = meshes.add(Capsule3d::new(0.3, 1.0));
        let projectile_mesh = meshes.add(Sphere::new(0.15));
        let hazard_mesh = meshes.add(Cylinder::new(0.4, 0.1));
        let prop_mesh = meshes.add(Cuboid::new(1.0, 1.0, 1.0));

        // Build a mesh per `BodyShape` variant so spawn sites can pick the
        // correctly-sized visual. Capsule `half_length` follows the
        // existing player-mesh convention (`2.0 * physics_half_height`)
        // so the visual scale stays consistent across body shapes.
        let mut body_shape_meshes: std::collections::HashMap<BodyShape, Handle<Mesh>> =
            std::collections::HashMap::new();
        for shape in BodyShape::ALL {
            let handle = if let Some((hh, r)) = shape.capsule_dims() {
                meshes.add(Capsule3d::new(r, hh * 2.0))
            } else {
                let half = shape
                    .cuboid_half_extents()
                    .expect("BodyShape is either capsule or cuboid");
                meshes.add(Cuboid::new(half.x * 2.0, half.y * 2.0, half.z * 2.0))
            };
            body_shape_meshes.insert(shape, handle);
        }

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
        let prop_active_mat = materials.add(StandardMaterial {
            base_color: Color::srgba(0.6, 0.4, 0.2, 0.2),
            alpha_mode: AlphaMode::Blend,
            ..default()
        });

        EntityMeshes {
            player_mesh,
            npc_mesh,
            boss_mesh,
            projectile_mesh,
            hazard_mesh,
            prop_mesh,
            body_shape_meshes,
            player_mat,
            npc_mat,
            boss_mat,
            local_player_mat,
            npc_combat_mat,
            npc_flee_mat,
            projectile_mat,
            hazard_mat,
            prop_mat,
            prop_active_mat,
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
    bodies: Query<&PresentationBody>,
    mut commands: Commands,
) {
    let Some(stdb) = stdb else { return };
    if local_player.entity_id.is_some() {
        return;
    }
    if !stdb.connected.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }

    let our_identity = stdb.conn.identity();
    for entity in stdb.conn.db.nearby_entities().iter() {
        if entity.owner_identity.as_ref() == Some(&our_identity) {
            log::info!("Local player entity detected: {}", entity.entity_id);
            local_player.entity_id = Some(entity.entity_id);

            if let Some(&bevy_entity) = entity_map.map.get(&entity.entity_id) {
                log::info!("Retroactively adding LocalPlayer to Bevy entity");
                commands
                    .entity(bevy_entity)
                    .insert((LocalPlayer, LastMoveDir::default()));
                if let Some(meshes) = entity_meshes {
                    if let Ok(body) = bodies.get(bevy_entity) {
                        commands
                            .entity(body.0)
                            .insert(MeshMaterial3d(meshes.local_player_mat.clone()));
                        spawn_facing_indicator(&mut commands, body.0, &meshes);
                    } else {
                        commands
                            .entity(bevy_entity)
                            .insert(MeshMaterial3d(meshes.local_player_mat.clone()));
                        spawn_facing_indicator(&mut commands, bevy_entity, &meshes);
                    }
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
    tick_counter: Res<TickCounter>,
    mut entity_map: ResMut<EntityMap>,
    mut commands: Commands,
    mut query: Query<
        (
            &mut Transform,
            &mut SmoothingState,
            &mut PresentationMotion,
            Option<&LocalPlayer>,
        ),
        With<ServerEntity>,
    >,
    entity_meshes: Option<Res<EntityMeshes>>,
    time: Res<Time>,
    mut telemetry: ResMut<NetSmoothingTelemetry>,
) {
    let Some(stdb) = stdb else { return };

    if entity_meshes.is_none() {
        commands.init_resource::<EntityMeshes>();
        return;
    }
    let meshes = entity_meshes.unwrap();

    let mut live_ids: bevy::utils::HashSet<u64> = bevy::utils::HashSet::new();

    let nearby_entities: HashMap<u64, RemoteEntity> = stdb
        .conn
        .db
        .nearby_entities()
        .iter()
        .map(|entity| (entity.entity_id, entity))
        .collect();

    let presentation_tick = tick_counter.last_tick as f32 - REMOTE_BUFFER_TICKS;
    let dt = time.delta_secs();

    for row in stdb.conn.db.nearby_transforms().iter() {
        let entity_row = nearby_entities.get(&row.entity_id);
        if entity_row
            .as_ref()
            .is_some_and(|e| matches!(e.state, EntityState::DespawnPending | EntityState::Removed))
        {
            continue;
        }

        live_ids.insert(row.entity_id);

        let kind = entity_row.map(|e| e.kind).unwrap_or(EntityKind::Player);
        let snapshot = TransformSnapshot {
            tick: row.last_tick as f32,
            position: Vec3::new(row.pos_x, row.pos_y, row.pos_z),
            rotation: Quat::from_xyzw(row.rot_x, row.rot_y, row.rot_z, row.rot_w),
            velocity: Vec3::new(row.vel_x, row.vel_y, row.vel_z),
        };

        if let Some(&bevy_entity) = entity_map.map.get(&row.entity_id) {
            if let Ok((mut tf, mut smoothing, mut motion, is_local)) = query.get_mut(bevy_entity) {
                // Capture gap (server tick - last_seen) before authoritative state advances.
                let prev_server_tick = smoothing.last_server_tick;
                let new_server_tick = snapshot.tick as u64;
                if new_server_tick > prev_server_tick && prev_server_tick != 0 {
                    telemetry.record_snapshot_gap(new_server_tick.saturating_sub(prev_server_tick));
                }
                update_authoritative_state(&mut smoothing, snapshot);

                let previous = tf.translation;
                if is_local.is_some() {
                    // Reconcile error = the divergence the server *just* corrected
                    // on the local predicted transform. Recorded BEFORE the lerp
                    // so the magnitude reflects pre-correction prediction drift.
                    let error_m = (smoothing.authoritative_pos - tf.translation).length();
                    let snapped = error_m > LOCAL_SNAP_DISTANCE;
                    telemetry.record_reconcile(error_m, snapped);

                    tf.translation =
                        reconcile_translation(tf.translation, smoothing.authoritative_pos, dt);
                    tf.rotation = tf.rotation.slerp(
                        smoothing.authoritative_rot,
                        (LOCAL_ROTATE_RATE * dt).min(1.0),
                    );
                    let velocity = if dt > 0.0 {
                        (tf.translation - previous) / dt
                    } else {
                        smoothing.authoritative_vel
                    };
                    update_presentation_motion(&mut motion, velocity, dt);
                } else {
                    // Detect extrapolation: presentation tick is past the latest
                    // snapshot in the buffer → sample_remote_snapshot will fall
                    // through to the velocity-extrapolation branch.
                    if let Some(latest) = smoothing.snapshots.back() {
                        if presentation_tick > latest.tick {
                            let secs = ((presentation_tick - latest.tick) / SIM_TICKS_PER_SECOND)
                                .clamp(0.0, MAX_EXTRAPOLATION_SECS);
                            telemetry.record_extrapolation(secs);
                        }
                    }
                    let sampled = sample_remote_snapshot(&smoothing.snapshots, presentation_tick);
                    tf.translation = sampled.position;
                    tf.rotation = sampled.rotation;
                    update_presentation_motion(&mut motion, sampled.velocity, dt);
                }
            }
        } else {
            let is_local = local_player.entity_id == Some(row.entity_id);
            // Look up an authored `body_shape` for this entity (if any).
            // Capsule entities (Player/Npc/Boss) read from `npc_config`;
            // Prop entities read from `interactable_config`. Missing rows
            // fall back to the kind default mesh below.
            let authored_shape: Option<BodyShape> = match kind {
                EntityKind::Player | EntityKind::Npc | EntityKind::Boss => stdb
                    .conn
                    .db
                    .npc_config()
                    .iter()
                    .find(|c| c.entity_id == row.entity_id)
                    .and_then(|c| c.body_shape)
                    .and_then(BodyShape::from_u8),
                EntityKind::Prop => stdb
                    .conn
                    .db
                    .interactable_config()
                    .iter()
                    .find(|c| c.entity_id == row.entity_id)
                    .and_then(|c| BodyShape::from_u8(c.body_shape)),
                EntityKind::Projectile | EntityKind::Hazard => None,
            };
            let shape_mesh = authored_shape
                .and_then(|s| meshes.body_shape_meshes.get(&s).cloned());
            let (mesh, mat) = match kind {
                EntityKind::Player if is_local => (
                    shape_mesh.unwrap_or_else(|| meshes.player_mesh.clone()),
                    meshes.local_player_mat.clone(),
                ),
                EntityKind::Player => (
                    shape_mesh.unwrap_or_else(|| meshes.player_mesh.clone()),
                    meshes.player_mat.clone(),
                ),
                EntityKind::Npc => (
                    shape_mesh.unwrap_or_else(|| meshes.npc_mesh.clone()),
                    meshes.npc_mat.clone(),
                ),
                EntityKind::Boss => (
                    shape_mesh.unwrap_or_else(|| meshes.boss_mesh.clone()),
                    meshes.boss_mat.clone(),
                ),
                EntityKind::Projectile => (
                    meshes.projectile_mesh.clone(),
                    meshes.projectile_mat.clone(),
                ),
                EntityKind::Hazard => (meshes.hazard_mesh.clone(), meshes.hazard_mat.clone()),
                EntityKind::Prop => (
                    shape_mesh.unwrap_or_else(|| meshes.prop_mesh.clone()),
                    meshes.prop_mat.clone(),
                ),
            };

            let mut entity_cmd = commands.spawn((
                Transform::from_translation(snapshot.position).with_rotation(snapshot.rotation),
                GlobalTransform::default(),
                ServerEntity {
                    entity_id: row.entity_id,
                },
                Health::default(),
                PresentationMotion::default(),
                SmoothingState::from_snapshot(snapshot),
            ));

            if is_local {
                entity_cmd.insert((LocalPlayer, LastMoveDir::default()));
            }

            let bevy_entity = entity_cmd.id();

            if is_character_kind(kind) {
                let body_entity = spawn_character_body(&mut commands, bevy_entity, mesh, mat);
                commands
                    .entity(bevy_entity)
                    .insert(PresentationBody(body_entity));
                if should_show_facing_indicator(kind, is_local) {
                    spawn_facing_indicator(&mut commands, body_entity, &meshes);
                }
            } else {
                entity_cmd.insert((Mesh3d(mesh), MeshMaterial3d(mat)));
            }

            entity_map.map.insert(row.entity_id, bevy_entity);
        }
    }

    let stale: Vec<u64> = entity_map
        .map
        .keys()
        .filter(|id| !live_ids.contains(*id))
        .copied()
        .collect();
    for id in stale {
        if let Some(bevy_entity) = entity_map.map.remove(&id) {
            commands.entity(bevy_entity).despawn_recursive();
        }
    }
}

fn spawn_character_body(
    commands: &mut Commands,
    owner: bevy::ecs::entity::Entity,
    mesh: Handle<Mesh>,
    material: Handle<StandardMaterial>,
) -> bevy::ecs::entity::Entity {
    let body = commands
        .spawn((
            Mesh3d(mesh),
            MeshMaterial3d(material),
            Transform::default(),
            GlobalTransform::default(),
            AnimatedVisual,
            VisualOwner(owner),
        ))
        .id();
    commands.entity(owner).add_child(body);
    body
}

fn is_character_kind(kind: EntityKind) -> bool {
    matches!(
        kind,
        EntityKind::Player | EntityKind::Npc | EntityKind::Boss
    )
}

fn update_authoritative_state(state: &mut SmoothingState, snapshot: TransformSnapshot) {
    state.authoritative_pos = snapshot.position;
    state.authoritative_rot = snapshot.rotation;
    state.authoritative_vel = snapshot.velocity;

    if snapshot.tick as u64 > state.last_server_tick {
        state.last_server_tick = snapshot.tick as u64;
        state.snapshots.push_back(snapshot);
        while state.snapshots.len() > MAX_SNAPSHOT_HISTORY {
            state.snapshots.pop_front();
        }
    } else if let Some(last) = state.snapshots.back_mut() {
        *last = snapshot;
    }
}

fn sample_remote_snapshot(
    snapshots: &VecDeque<TransformSnapshot>,
    presentation_tick: f32,
) -> SampledTransform {
    let Some(first) = snapshots.front().copied() else {
        return SampledTransform {
            position: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            velocity: Vec3::ZERO,
        };
    };
    let latest = snapshots.back().copied().unwrap_or(first);

    if presentation_tick <= first.tick {
        return SampledTransform {
            position: first.position,
            rotation: first.rotation,
            velocity: first.velocity,
        };
    }

    let items: Vec<_> = snapshots.iter().copied().collect();
    for window in items.windows(2) {
        let a = window[0];
        let b = window[1];
        if presentation_tick >= a.tick && presentation_tick <= b.tick {
            let span = (b.tick - a.tick).max(f32::EPSILON);
            let t = ((presentation_tick - a.tick) / span).clamp(0.0, 1.0);
            return SampledTransform {
                position: a.position.lerp(b.position, t),
                rotation: a.rotation.slerp(b.rotation, t),
                velocity: a.velocity.lerp(b.velocity, t),
            };
        }
    }

    let extrapolation_secs = ((presentation_tick - latest.tick) / SIM_TICKS_PER_SECOND)
        .clamp(0.0, MAX_EXTRAPOLATION_SECS);
    SampledTransform {
        position: latest.position + latest.velocity * extrapolation_secs,
        rotation: latest.rotation,
        velocity: latest.velocity,
    }
}

fn reconcile_translation(current: Vec3, authoritative: Vec3, delta_secs: f32) -> Vec3 {
    let delta = authoritative - current;
    if delta.length() > LOCAL_SNAP_DISTANCE {
        return authoritative;
    }
    current.lerp(authoritative, (LOCAL_RECONCILE_RATE * delta_secs).min(1.0))
}

fn update_presentation_motion(
    motion: &mut PresentationMotion,
    new_velocity: Vec3,
    delta_secs: f32,
) {
    let previous = motion.velocity;
    motion.velocity = new_velocity;
    motion.planar_speed = Vec2::new(new_velocity.x, new_velocity.z).length();
    motion.turn_rate = if delta_secs > 0.0 {
        signed_angle_between(
            Vec2::new(previous.x, previous.z),
            Vec2::new(new_velocity.x, new_velocity.z),
        ) / delta_secs
    } else {
        0.0
    };
}

fn signed_angle_between(previous: Vec2, current: Vec2) -> f32 {
    if previous.length_squared() < 1e-4 || current.length_squared() < 1e-4 {
        return 0.0;
    }
    let previous = previous.normalize();
    let current = current.normalize();
    let cross = previous.x * current.y - previous.y * current.x;
    let dot = previous.dot(current).clamp(-1.0, 1.0);
    cross.atan2(dot)
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

#[derive(Component)]
struct HealthBarBg;

#[derive(Component)]
struct HealthBarFill {
    parent_server_entity: u64,
}

fn sync_health_bars(
    mut commands: Commands,
    server_q: Query<(
        bevy::ecs::entity::Entity,
        &ServerEntity,
        &Health,
        Option<&Children>,
    )>,
    mut fill_q: Query<(&HealthBarFill, &mut Node, &mut BackgroundColor)>,
    bg_q: Query<&HealthBarBg>,
) {
    for (bevy_entity, se, hp, children) in server_q.iter() {
        let has_bar = children.map_or(false, |ch| ch.iter().any(|c| bg_q.get(*c).is_ok()));

        if !has_bar && hp.max_hp > 0.0 {
            commands.entity(bevy_entity).with_children(|parent| {
                parent
                    .spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            width: Val::Px(60.0),
                            height: Val::Px(6.0),
                            left: Val::Px(-30.0),
                            top: Val::Px(-84.0),
                            ..default()
                        },
                        BackgroundColor(Color::srgba(0.2, 0.2, 0.2, 0.7)),
                        HealthBarBg,
                    ))
                    .with_children(|bar_parent| {
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

        for (fill, mut node, mut bg) in fill_q.iter_mut() {
            if fill.parent_server_entity == se.entity_id && hp.max_hp > 0.0 {
                let frac = (hp.hp / hp.max_hp).clamp(0.0, 1.0);
                node.width = Val::Percent(frac * 100.0);
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

// ── Name tags ───────────────────────────────────────────────────────

#[derive(Component)]
struct NameTag {
    parent_server_entity: u64,
}

fn sync_name_tags(
    stdb: Option<Res<StdbConnection>>,
    mut commands: Commands,
    server_q: Query<(&ServerEntity, &Transform), Without<NameTag>>,
    camera_q: Query<(&Camera, &GlobalTransform), With<GameCamera>>,
    mut tag_q: Query<(
        bevy::ecs::entity::Entity,
        &NameTag,
        &mut Node,
        &mut Visibility,
    )>,
) {
    let Some(stdb) = stdb else { return };
    let camera = camera_q.get_single().ok();
    let nearby_entities: HashMap<u64, RemoteEntity> = stdb
        .conn
        .db
        .nearby_entities()
        .iter()
        .map(|entity| (entity.entity_id, entity))
        .collect();
    let server_positions: HashMap<u64, Vec3> = server_q
        .iter()
        .map(|(server_entity, tf)| (server_entity.entity_id, tf.translation))
        .collect();

    for (tag_entity, name_tag, mut node, mut visibility) in tag_q.iter_mut() {
        let Some(entity_row) = nearby_entities.get(&name_tag.parent_server_entity) else {
            commands.entity(tag_entity).despawn();
            continue;
        };
        if !is_character_kind(entity_row.kind) {
            commands.entity(tag_entity).despawn();
            continue;
        }
        let Some(position) = server_positions.get(&name_tag.parent_server_entity) else {
            commands.entity(tag_entity).despawn();
            continue;
        };

        if let Some((camera, cam_tf)) = camera {
            let world_pos = *position + Vec3::new(0.0, 2.6, 0.0);
            if let Some(ndc) = camera.world_to_ndc(cam_tf, world_pos) {
                if ndc.z >= 0.0 {
                    node.left = Val::Percent((ndc.x + 1.0) * 50.0);
                    node.top = Val::Percent((1.0 - ndc.y) * 50.0);
                    *visibility = Visibility::Visible;
                } else {
                    *visibility = Visibility::Hidden;
                }
            } else {
                *visibility = Visibility::Hidden;
            }
        } else {
            *visibility = Visibility::Hidden;
        }
    }

    let existing_tags: bevy::utils::HashSet<u64> = tag_q
        .iter()
        .map(|(_, name_tag, _, _)| name_tag.parent_server_entity)
        .collect();

    for (server_entity, _tf) in server_q.iter() {
        let Some(entity_row) = nearby_entities.get(&server_entity.entity_id) else {
            continue;
        };
        if !is_character_kind(entity_row.kind) {
            continue;
        }
        if existing_tags.contains(&server_entity.entity_id) {
            continue;
        }

        commands.spawn((
            Text::new(format!(
                "{} #{}",
                entity_kind_label(entity_row.kind),
                server_entity.entity_id
            )),
            TextFont {
                font_size: 24.0,
                ..default()
            },
            TextColor(Color::srgba(0.98, 0.97, 0.92, 0.98)),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Percent(50.0),
                top: Val::Percent(50.0),
                ..default()
            },
            Visibility::Hidden,
            NameTag {
                parent_server_entity: server_entity.entity_id,
            },
        ));
    }
}

fn entity_kind_label(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Player => "Player",
        EntityKind::Npc => "Npc",
        EntityKind::Boss => "Boss",
        EntityKind::Projectile => "Projectile",
        EntityKind::Hazard => "Hazard",
        EntityKind::Prop => "Prop",
    }
}

// ── Target lock indicator ────────────────────────────────────────────

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

    for ring in existing_rings.iter() {
        commands.entity(ring).despawn();
    }

    let Some(target_id) = target else { return };
    let Some(&bevy_entity) = entity_map.map.get(&target_id) else {
        return;
    };

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

// ── Aggro indicator ──────────────────────────────────────────────────

#[derive(Component)]
pub struct AggroIndicator;

fn sync_aggro_indicator(
    stdb: Option<Res<StdbConnection>>,
    mut commands: Commands,
    entity_map: Res<EntityMap>,
    existing_indicators: Query<bevy::ecs::entity::Entity, With<AggroIndicator>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let Some(stdb) = stdb else { return };

    // Clean up old indicators.
    for indicator in existing_indicators.iter() {
        commands.entity(indicator).despawn();
    }

    // Collect unique entities currently targeted by any NPC in combat.
    let mut aggroed_ids = bevy::utils::HashSet::new();
    for npc in stdb.conn.db.npc_state().iter() {
        if npc.ai_state == NpcAiState::Combat {
            if let Some(target_id) = npc.target_entity {
                aggroed_ids.insert(target_id);
            }
        }
    }

    if aggroed_ids.is_empty() {
        return;
    }

    // Reuse ring assets logic.
    let ring_mesh = meshes.add(Torus::new(0.4, 0.55));
    let ring_mat = materials.add(StandardMaterial {
        base_color: Color::srgba(1.0, 0.8, 0.0, 0.5), // Yellow-orange
        emissive: LinearRgba::new(2.5, 1.5, 0.0, 1.0), // Bright glow
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        ..default()
    });

    for &target_id in aggroed_ids.iter() {
        if let Some(&bevy_entity) = entity_map.map.get(&target_id) {
            commands.entity(bevy_entity).with_children(|parent| {
                parent.spawn((
                    Mesh3d(ring_mesh.clone()),
                    MeshMaterial3d(ring_mat.clone()),
                    Transform::from_xyz(0.0, -0.45, 0.0), // Slightly lower than TargetLockRing
                    AggroIndicator,
                ));
            });
        }
    }
}

// ── NPC AI state color ──────────────────────────────────────────────

fn sync_npc_state_color(
    stdb: Option<Res<StdbConnection>>,
    entity_map: Res<EntityMap>,
    entity_meshes: Option<Res<EntityMeshes>>,
    bodies: Query<&PresentationBody>,
    mut commands: Commands,
    entity_table: Query<&ServerEntity>,
) {
    let Some(stdb) = stdb else { return };
    let Some(meshes) = entity_meshes else { return };
    let nearby_entity_kinds: HashMap<u64, EntityKind> = stdb
        .conn
        .db
        .nearby_entities()
        .iter()
        .map(|entity| (entity.entity_id, entity.kind))
        .collect();

    for npc in stdb.conn.db.npc_state().iter() {
        let Some(&bevy_entity) = entity_map.map.get(&npc.entity_id) else {
            continue;
        };

        let Ok(se) = entity_table.get(bevy_entity) else {
            continue;
        };
        let is_npc = nearby_entity_kinds
            .get(&se.entity_id)
            .is_some_and(|kind| matches!(kind, EntityKind::Npc | EntityKind::Boss));
        if !is_npc {
            continue;
        }

        let mat = match npc.ai_state {
            NpcAiState::Combat => meshes.npc_combat_mat.clone(),
            NpcAiState::Flee => meshes.npc_flee_mat.clone(),
            _ => meshes.npc_mat.clone(),
        };

        if let Ok(body) = bodies.get(bevy_entity) {
            commands.entity(body.0).insert(MeshMaterial3d(mat));
        } else {
            commands.entity(bevy_entity).insert(MeshMaterial3d(mat));
        }
    }
}

// ── Interactable state sync ─────────────────────────────────────────

fn sync_interactables(
    stdb: Option<Res<StdbConnection>>,
    entity_map: Res<EntityMap>,
    entity_meshes: Option<Res<EntityMeshes>>,
    mut commands: Commands,
) {
    let Some(stdb) = stdb else { return };
    let Some(meshes) = entity_meshes else { return };

    for interactable in stdb.conn.db.interactable_config().iter() {
        if let Some(&bevy_entity) = entity_map.map.get(&interactable.entity_id) {
            let mat = match interactable.state {
                game_client::module_bindings::InteractState::Active => {
                    meshes.prop_active_mat.clone()
                }
                _ => meshes.prop_mat.clone(),
            };

            if matches!(
                interactable.interact_kind,
                game_client::module_bindings::InteractKind::Gate
                    | game_client::module_bindings::InteractKind::Chest
            ) {
                commands.entity(bevy_entity).insert(MeshMaterial3d(mat));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(tick: f32, x: f32, velocity_x: f32) -> TransformSnapshot {
        TransformSnapshot {
            tick,
            position: Vec3::new(x, 0.0, 0.0),
            rotation: Quat::IDENTITY,
            velocity: Vec3::new(velocity_x, 0.0, 0.0),
        }
    }

    #[test]
    fn interpolation_buffer_samples_between_snapshots() {
        let snapshots = VecDeque::from([snapshot(10.0, 0.0, 1.0), snapshot(11.0, 1.0, 1.0)]);

        let sampled = sample_remote_snapshot(&snapshots, 10.5);
        assert!((sampled.position.x - 0.5).abs() < 0.001);
    }

    #[test]
    fn extrapolation_is_clamped_when_buffer_runs_dry() {
        let snapshots = VecDeque::from([snapshot(10.0, 0.0, 10.0)]);

        let sampled = sample_remote_snapshot(&snapshots, 20.0);
        assert!((sampled.position.x - (10.0 * MAX_EXTRAPOLATION_SECS)).abs() < 0.001);
    }

    #[test]
    fn local_reconciliation_converges_without_overshoot() {
        let mut current = Vec3::new(0.0, 0.0, 0.0);
        let authoritative = Vec3::new(1.0, 0.0, 0.0);

        for _ in 0..10 {
            current = reconcile_translation(current, authoritative, 0.05);
            assert!(current.x <= authoritative.x + 0.0001);
        }

        assert!(current.x > 0.9);
    }

    #[test]
    fn telemetry_records_extrapolation_reconcile_and_gap() {
        let mut t = NetSmoothingTelemetry::default();

        // Extrapolation event recording.
        t.record_extrapolation(0.05);
        t.record_extrapolation(0.12);
        assert_eq!(t.extrapolation_events, 2);
        assert!((t.extrapolation_secs_total - 0.17).abs() < 1e-5);
        assert!((t.extrapolation_secs_max - 0.12).abs() < 1e-5);

        // Reconcile error: max tracks peak, ewma trends toward sustained value,
        // snap counter increments only when threshold crossed.
        t.record_reconcile(0.5, false);
        t.record_reconcile(2.0, false);
        t.record_reconcile(5.0, true); // exceeds LOCAL_SNAP_DISTANCE → snap
        assert_eq!(t.reconcile_samples, 3);
        assert!((t.reconcile_err_max - 5.0).abs() < 1e-5);
        assert_eq!(t.snap_corrections, 1);
        assert!(
            t.reconcile_err_ewma > 0.0 && t.reconcile_err_ewma < 5.0,
            "ewma should sit between samples and peak; got {}",
            t.reconcile_err_ewma
        );

        // Snapshot gap: zero-gap (same-tick replacement) ignored.
        t.record_snapshot_gap(0);
        assert_eq!(t.snapshot_gap_max, 0);
        t.record_snapshot_gap(1);
        t.record_snapshot_gap(4);
        assert_eq!(t.snapshot_gap_max, 4);
        assert!(t.snapshot_gap_ewma > 0.0);
    }
}
