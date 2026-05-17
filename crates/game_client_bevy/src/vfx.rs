use bevy::prelude::*;

use crate::sync::ServerEntity;
#[allow(unused_imports)]
#[cfg(feature = "connected")]
use game_client::module_bindings::spawn_player;
#[cfg(feature = "connected")]
use game_client::module_bindings::respawn_player;

pub struct VfxPlugin;

impl Plugin for VfxPlugin {
    fn build(&self, app: &mut App) {
        app.add_event::<DeathNotification>();
        app.add_event::<DamageNumberEvent>();
        app.add_event::<ChargeStartEvent>();
        app.add_event::<ChargeTierReachedEvent>();
        app.add_event::<ProjectileLaunchEvent>();
        app.add_event::<SkillObjectRemoveEvent>();
        app.add_event::<HazardSpawnEvent>();
        app.add_event::<HitboxSpawnedEvent>();
        app.add_event::<HitboxDamageFrameEvent>();
        app.add_event::<HitboxRemovedEvent>();
        app.add_systems(Update, (
            update_skill_flash,
            spawn_death_effects,
            update_death_markers,
            update_death_screen,
            handle_respawn,
            spawn_damage_numbers,
            update_damage_numbers,
            spawn_charge_effects,
            update_charge_effects,
            spawn_client_projectiles,
            update_client_projectiles,
            despawn_client_projectiles,
            spawn_hazard_visuals,
            spawn_hitbox_visuals,
            update_hitbox_visuals,
            remove_hitbox_visuals,
            despawn_hazard_visuals,
        ));
    }
}

// ── Death effects ────────────────────────────────────

/// Emitted when any entity dies (detected from combat_event table).
#[derive(Event)]
pub struct DeathNotification {
    pub entity_id: u64,
    pub is_local_player: bool,
}

/// Brief red sphere marker at the death location.
#[derive(Component)]
struct DeathMarker {
    remaining: f32,
}

/// Container node for the "YOU DIED" overlay.
#[derive(Component)]
struct DeathScreen {
    age: f32,
}

/// Tag on the text child so we can update its color for fade.
#[derive(Component)]
struct DeathScreenText;

/// Tag on the "Press R to Respawn" hint text.
#[derive(Component)]
struct RespawnHintText;

/// Attached to an entity to make it flash a color briefly.
#[derive(Component)]
pub struct SkillFlash {
    pub remaining: f32,
    #[allow(dead_code)] // reserved for intensity lerp
    pub color: Color,
    pub original: Handle<StandardMaterial>,
}

/// Trigger a flash on the local player entity for the given ability.
pub fn trigger_flash(
    commands: &mut Commands,
    materials: &mut Assets<StandardMaterial>,
    entity: bevy::ecs::entity::Entity,
    current_mat: &Handle<StandardMaterial>,
    ability_id: u32,
) {
    let flash_color = match ability_id {
        1 => Color::srgb(1.0, 1.0, 0.3),   // Slash — gold flash
        2 => Color::srgb(1.0, 0.5, 0.0),   // Fireball — orange flash
        3 => Color::srgb(0.7, 0.3, 1.0),   // Smash — purple flash
        _ => Color::srgb(1.0, 1.0, 1.0),
    };

    let flash_mat = materials.add(StandardMaterial {
        base_color: flash_color,
        emissive: flash_color.into(),
        ..default()
    });

    commands.entity(entity).insert((
        MeshMaterial3d(flash_mat),
        SkillFlash {
            remaining: 0.15,
            color: flash_color,
            original: current_mat.clone(),
        },
    ));
}

/// Tick flash timers and restore original material when done.
fn update_skill_flash(
    mut commands: Commands,
    time: Res<Time>,
    mut query: Query<(bevy::ecs::entity::Entity, &mut SkillFlash), With<ServerEntity>>,
) {
    for (entity, mut flash) in query.iter_mut() {
        flash.remaining -= time.delta_secs();
        if flash.remaining <= 0.0 {
            let original = flash.original.clone();
            commands.entity(entity).remove::<SkillFlash>();
            commands.entity(entity).insert(MeshMaterial3d(original));
        }
    }
}

// ── Death VFX systems ────────────────────────────────

/// React to DeathNotification events: spawn red death marker + "YOU DIED" overlay.
fn spawn_death_effects(
    mut commands: Commands,
    mut events: EventReader<DeathNotification>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    server_entities: Query<(&ServerEntity, &Transform)>,
    existing_screen: Query<bevy::ecs::entity::Entity, With<DeathScreen>>,
) {
    for ev in events.read() {
        // Spawn a shrinking red sphere at the entity's last known position.
        let pos = server_entities
            .iter()
            .find(|(se, _)| se.entity_id == ev.entity_id)
            .map(|(_, tf)| tf.translation);

        if let Some(pos) = pos {
            let death_mat = materials.add(StandardMaterial {
                base_color: Color::srgba(1.0, 0.0, 0.0, 0.8),
                emissive: LinearRgba::new(2.0, 0.0, 0.0, 1.0),
                alpha_mode: AlphaMode::Blend,
                ..default()
            });
            commands.spawn((
                Mesh3d(meshes.add(Sphere::new(0.5))),
                MeshMaterial3d(death_mat),
                Transform::from_translation(pos + Vec3::Y * 0.5),
                DeathMarker { remaining: 1.2 },
            ));
            log::info!("Death marker spawned for entity #{} at {:?}", ev.entity_id, pos);
        }

        // Show "YOU DIED" overlay for local player with respawn hint.
        if ev.is_local_player && existing_screen.is_empty() {
            commands
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        top: Val::Percent(35.0),
                        width: Val::Percent(100.0),
                        height: Val::Px(150.0),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        flex_direction: FlexDirection::Column,
                        row_gap: Val::Px(20.0),
                        ..default()
                    },
                    DeathScreen { age: 0.0 },
                ))
                .with_children(|parent| {
                    parent.spawn((
                        Text::new("YOU DIED"),
                        TextFont {
                            font_size: 72.0,
                            ..default()
                        },
                        TextColor(Color::srgba(1.0, 0.0, 0.0, 1.0)),
                        DeathScreenText,
                    ));
                    parent.spawn((
                        Text::new("Press R to Respawn"),
                        TextFont {
                            font_size: 24.0,
                            ..default()
                        },
                        TextColor(Color::srgba(1.0, 1.0, 1.0, 0.0)),
                        RespawnHintText,
                    ));
                });
            log::info!("Local player died — showing death screen (press R to respawn)");
        }
    }
}

/// Shrink death markers over time and despawn when done.
fn update_death_markers(
    mut commands: Commands,
    time: Res<Time>,
    mut query: Query<(bevy::ecs::entity::Entity, &mut DeathMarker, &mut Transform)>,
) {
    let dt = time.delta_secs();
    for (entity, mut marker, mut tf) in query.iter_mut() {
        marker.remaining -= dt;
        let frac = (marker.remaining / 1.2).max(0.0);
        tf.scale = Vec3::splat(frac);
        if marker.remaining <= 0.0 {
            commands.entity(entity).despawn();
        }
    }
}

/// Fade the "YOU DIED" text and show respawn hint after 2s.
fn update_death_screen(
    time: Res<Time>,
    mut screen_q: Query<&mut DeathScreen>,
    mut hint_q: Query<&mut TextColor, With<RespawnHintText>>,
) {
    let dt = time.delta_secs();
    for mut screen in screen_q.iter_mut() {
        screen.age += dt;

        // Fade in the respawn hint after 2 seconds.
        let hint_alpha = ((screen.age - 2.0) / 1.0).clamp(0.0, 1.0);
        for mut color in hint_q.iter_mut() {
            color.0 = Color::srgba(1.0, 1.0, 1.0, hint_alpha);
        }
    }
}

/// Handle R key press to respawn: despawn death screen, call respawn_player.
fn handle_respawn(
    mut commands: Commands,
    keyboard: Res<ButtonInput<KeyCode>>,
    screen_q: Query<(bevy::ecs::entity::Entity, &DeathScreen)>,
    #[cfg(feature = "connected")]
    stdb: Option<Res<crate::spacetime::StdbConnection>>,
    #[cfg(feature = "connected")]
    mut local_player: ResMut<crate::spacetime::LocalPlayerEntity>,
) {
    if !keyboard.just_pressed(KeyCode::KeyR) {
        return;
    }

    let mut had_screen = false;
    for (entity, screen) in screen_q.iter() {
        // Only allow respawn after 2 seconds.
        if screen.age < 2.0 {
            return;
        }
        commands.entity(entity).despawn_recursive();
        had_screen = true;
    }

    if !had_screen {
        return;
    }

    #[cfg(feature = "connected")]
    {
        // Reset local player tracking so detect_local_player re-detects.
        local_player.entity_id = None;
        local_player.spawned = false;

        if let Some(stdb) = stdb {
            // TODO: After next `dev-deploy.ps1 -Clean`, switch to:
            //   stdb.conn.reducers.respawn_player()
            // which properly handles dead players without the "already spawned" error.
            if let Err(e) = stdb.conn.reducers.respawn_player() {
                log::error!("Respawn failed: {e}");
            } else {
                log::info!("Respawn: respawn_player called");
            }
        }
    }
}

// ── Floating damage numbers ──────────────────────────

/// Emitted when damage is dealt to spawn a floating number at the target.
#[derive(Event)]
pub struct DamageNumberEvent {
    pub target_entity_id: u64,
    pub amount: f32,
    pub is_crit: bool,
    pub is_self: bool,
}

/// Emitted when a chargeable ability begins charging.
#[allow(dead_code)]
#[derive(Event)]
pub struct ChargeStartEvent {
    pub source_entity_id: u64,
    pub ability_id: u32,
    pub max_ticks: u32,
}

/// Emitted when a charge tier is reached during charging.
#[allow(dead_code)]
#[derive(Event)]
pub struct ChargeTierReachedEvent {
    pub source_entity_id: u64,
    pub ability_id: u32,
    pub tier: u32,
}

/// Floating text that drifts upward and fades.
#[derive(Component)]
struct DamageNumber {
    remaining: f32,
    velocity: Vec3,
}

fn spawn_damage_numbers(
    mut commands: Commands,
    mut events: EventReader<DamageNumberEvent>,
    server_entities: Query<(&ServerEntity, &Transform)>,
) {
    for ev in events.read() {
        let pos = server_entities
            .iter()
            .find(|(se, _)| se.entity_id == ev.target_entity_id)
            .map(|(_, tf)| tf.translation);

        let Some(_pos) = pos else { continue };

        let color = if ev.is_self {
            Color::srgb(1.0, 0.2, 0.2)
        } else if ev.is_crit {
            Color::srgb(1.0, 0.8, 0.0)
        } else {
            Color::srgb(1.0, 1.0, 1.0)
        };

        let font_size = if ev.is_crit { 28.0 } else { 22.0 };

        // Spawn as a 2D text overlay anchored above the entity.
        // We use a Node-based text since billboard text requires extra setup.
        commands.spawn((
            Text::new(format!("{:.0}", ev.amount)),
            TextFont { font_size, ..default() },
            TextColor(color),
            Node {
                position_type: PositionType::Absolute,
                // Approximate screen position — will be updated by update_damage_numbers
                left: Val::Percent(50.0),
                top: Val::Percent(40.0),
                ..default()
            },
            DamageNumber {
                remaining: 1.5,
                velocity: Vec3::new(0.0, -60.0, 0.0), // drift upward in screen space (top decreases)
            },
        ));
    }
}

fn update_damage_numbers(
    mut commands: Commands,
    time: Res<Time>,
    mut query: Query<(bevy::ecs::entity::Entity, &mut DamageNumber, &mut Node, &mut TextColor)>,
) {
    let dt = time.delta_secs();
    for (entity, mut dmg, mut node, mut color) in query.iter_mut() {
        dmg.remaining -= dt;

        // Drift upward.
        if let Val::Percent(top) = node.top {
            node.top = Val::Percent(top + dmg.velocity.y * dt);
        }

        // Fade out.
        let alpha = (dmg.remaining / 0.5).clamp(0.0, 1.0);
        let Color::Srgba(c) = color.0 else { continue };
        color.0 = Color::srgba(c.red, c.green, c.blue, alpha);

        if dmg.remaining <= 0.0 {
            commands.entity(entity).despawn();
        }
    }
}

// ── Client-predicted projectile visuals ──────────────

/// Emitted when the server reports a projectile was launched.
#[derive(Event)]
pub struct ProjectileLaunchEvent {
    pub execution_id: u64,
    pub origin: Vec3,
    pub direction: Vec3,
    /// World-units per second (server speed × tick rate).
    pub speed: f32,
    pub max_range: f32,
}

/// Emitted when the server reports a projectile was removed (hit or expired).
#[derive(Event)]
pub struct SkillObjectRemoveEvent {
    pub execution_id: u64,
}

/// Marks a client-predicted projectile entity.
#[derive(Component)]
struct ClientProjectile {
    execution_id: u64,
    direction: Vec3,
    speed: f32,
    remaining_range: f32,
}

fn spawn_client_projectiles(
    mut commands: Commands,
    mut events: EventReader<ProjectileLaunchEvent>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for ev in events.read() {
        let mesh = meshes.add(Sphere::new(0.15));
        let mat = materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.9, 0.2),
            emissive: LinearRgba::new(2.0, 1.8, 0.0, 1.0),
            ..default()
        });
        commands.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::from_translation(ev.origin),
            ClientProjectile {
                execution_id: ev.execution_id,
                direction: ev.direction,
                speed: ev.speed,
                remaining_range: ev.max_range,
            },
        ));
    }
}

fn update_client_projectiles(
    mut commands: Commands,
    time: Res<Time>,
    mut query: Query<(bevy::ecs::entity::Entity, &mut ClientProjectile, &mut Transform)>,
    targets: Query<&Transform, (With<ServerEntity>, Without<crate::camera::LocalPlayer>, Without<ClientProjectile>)>,
) {
    // Collision radius: projectile visual (0.15) + character capsule (~0.8).
    const HIT_RADIUS_SQ: f32 = 0.95 * 0.95;

    let dt = time.delta_secs();
    for (entity, mut proj, mut tf) in query.iter_mut() {
        let step = proj.speed * dt;
        tf.translation += proj.direction * step;
        proj.remaining_range -= step;
        if proj.remaining_range <= 0.0 {
            commands.entity(entity).despawn();
            continue;
        }

        // Client-side hit prediction: despawn when near any non-local entity.
        let pos = tf.translation;
        let mut hit = false;
        for target_tf in targets.iter() {
            let diff = pos - target_tf.translation;
            if diff.length_squared() < HIT_RADIUS_SQ {
                hit = true;
                break;
            }
        }
        if hit {
            commands.entity(entity).despawn();
        }
    }
}

fn despawn_client_projectiles(
    mut commands: Commands,
    mut events: EventReader<SkillObjectRemoveEvent>,
    query: Query<(bevy::ecs::entity::Entity, &ClientProjectile)>,
) {
    for ev in events.read() {
        for (entity, proj) in query.iter() {
            if proj.execution_id == ev.execution_id {
                commands.entity(entity).despawn();
            }
        }
    }
}

// ── Hazard zone visuals ─────────────────────────────────────

/// Emitted when the server reports a world-space hazard was spawned.
#[allow(dead_code)]
#[derive(Event)]
pub struct HazardSpawnEvent {
    pub execution_id: u64,
    pub ability_id: u32,
    pub position: Vec3,
    pub radius: f32,
}

/// Marks a client-side hazard zone visual (ground circle).
#[derive(Component)]
struct HazardVisual {
    execution_id: u64,
}

fn spawn_hazard_visuals(
    mut commands: Commands,
    mut events: EventReader<HazardSpawnEvent>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for ev in events.read() {
        // Flat disc at the hazard position.
        let mesh = meshes.add(Circle::new(ev.radius));
        let mat = materials.add(StandardMaterial {
            base_color: Color::srgba(1.0, 0.3, 0.0, 0.4),
            emissive: LinearRgba::new(1.5, 0.3, 0.0, 1.0),
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            cull_mode: None,
            ..default()
        });
        let mut tf = Transform::from_translation(ev.position + Vec3::Y * 0.05);
        tf.rotation = Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2);
        commands.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            tf,
            HazardVisual {
                execution_id: ev.execution_id,
            },
        ));
    }
}

fn despawn_hazard_visuals(
    mut commands: Commands,
    mut events: EventReader<SkillObjectRemoveEvent>,
    query: Query<(bevy::ecs::entity::Entity, &HazardVisual)>,
) {
    for ev in events.read() {
        for (entity, hv) in query.iter() {
            if hv.execution_id == ev.execution_id {
                commands.entity(entity).despawn();
            }
        }
    }
}

// ── Hitbox 3D Visualization ─────────────────────────────────

/// Emitted when the server reports a hitbox was spawned.
#[derive(Event)]
pub struct HitboxSpawnedEvent {
    pub source_entity_id: u64,
    pub ability_id: u32,
}

/// Emitted when the damage frame fires. Brief red flash on the hitbox.
#[derive(Event)]
pub struct HitboxDamageFrameEvent {
    pub source_entity_id: u64,
    pub ability_id: u32,
}

/// Emitted when the server reports the hitbox was removed.
#[allow(dead_code)]
#[derive(Event)]
pub struct HitboxRemovedEvent {
    pub source_entity_id: u64,
    pub ability_id: u32,
}

/// A live client-side hitbox visual. Fades out over `fade_total` seconds.
#[derive(Component)]
struct HitboxVisual {
    source_entity_id: u64,
    ability_id: u32,
    /// Remaining fade-out time in seconds. Starts at `HITBOX_FADE_SECS`.
    fade_remaining: f32,
    /// Whether the damage frame has already been applied (controls flash state).
    damage_flashed: bool,
}

/// Hold-time for the hitbox visual after removal (500ms as requested).
const HITBOX_FADE_SECS: f32 = 0.5;

fn spawn_hitbox_visuals(
    mut commands: Commands,
    mut events: EventReader<HitboxSpawnedEvent>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    server_entities: Query<(&ServerEntity, &Transform)>,
) {
    use crate::ability_bar::{ALL_ABILITIES, AbilityShape};

    for ev in events.read() {
        // Find caster position and rotation.
        let caster_tf = server_entities
            .iter()
            .find(|(se, _)| se.entity_id == ev.source_entity_id)
            .map(|(_, tf)| *tf);
        let Some(caster_tf) = caster_tf else { continue };

        let def = ALL_ABILITIES.iter().find(|a| a.id == ev.ability_id);
        let Some(def) = def else { continue };

        let mesh = match def.shape {
            AbilityShape::Capsule { radius, half_height } => {
                meshes.add(Capsule3d::new(radius, half_height * 2.0))
            }
            AbilityShape::Sphere { radius } => {
                meshes.add(Sphere::new(radius))
            }
            AbilityShape::None => continue,
        };

        // Offset in entity-local space, rotated by caster facing.
        let local_offset = Vec3::new(def.offset[0], def.offset[1], def.offset[2]);
        let origin = caster_tf.translation + caster_tf.rotation * local_offset;

        let material = materials.add(StandardMaterial {
            base_color: Color::srgba(1.0, 0.3, 0.1, 0.35),
            emissive: LinearRgba::new(1.5, 0.4, 0.0, 1.0),
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            ..default()
        });

        let mut spawn_tf = Transform::from_translation(origin);
        spawn_tf.rotation = caster_tf.rotation;

        commands.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(material),
            spawn_tf,
            HitboxVisual {
                source_entity_id: ev.source_entity_id,
                ability_id: ev.ability_id,
                fade_remaining: HITBOX_FADE_SECS,
                damage_flashed: false,
            },
        ));
    }
}

fn update_hitbox_visuals(
    mut events: EventReader<HitboxDamageFrameEvent>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    time: Res<Time>,
    mut query: Query<(
        &mut HitboxVisual,
        &MeshMaterial3d<StandardMaterial>,
        &mut Transform,
    )>,
    server_entities: Query<(&ServerEntity, &Transform), Without<HitboxVisual>>,
) {
    let flashes: Vec<(u64, u32)> = events
        .read()
        .map(|e| (e.source_entity_id, e.ability_id))
        .collect();

    let dt = time.delta_secs();
    for (mut vis, mat_handle, mut vis_tf) in query.iter_mut() {
        // Track source entity so the hitbox visual follows the caster.
        if let Some((_, src_tf)) = server_entities
            .iter()
            .find(|(se, _)| se.entity_id == vis.source_entity_id)
        {
            vis_tf.translation = src_tf.translation;
            vis_tf.rotation = src_tf.rotation;
        }

        let should_flash = flashes.iter().any(|(sid, aid)|
            *sid == vis.source_entity_id && *aid == vis.ability_id
        );
        if should_flash && !vis.damage_flashed {
            vis.damage_flashed = true;
            if let Some(mat) = materials.get_mut(&mat_handle.0) {
                mat.base_color = Color::srgba(1.0, 0.0, 0.0, 0.7);
                mat.emissive = LinearRgba::new(3.0, 0.0, 0.0, 1.0);
            }
        }

        vis.fade_remaining -= dt;
        if let Some(mat) = materials.get_mut(&mat_handle.0) {
            let alpha_frac = (vis.fade_remaining / HITBOX_FADE_SECS).clamp(0.0, 1.0);
            let emissive_scale = alpha_frac;
            if vis.damage_flashed {
                mat.base_color = Color::srgba(1.0, 0.0, 0.0, alpha_frac * 0.7);
                mat.emissive = LinearRgba::new(3.0 * emissive_scale, 0.0, 0.0, 1.0);
            } else {
                mat.base_color = Color::srgba(1.0, 0.3, 0.1, alpha_frac * 0.35);
                mat.emissive = LinearRgba::new(1.5 * emissive_scale, 0.4 * emissive_scale, 0.0, 1.0);
            }
        }
    }
}

fn remove_hitbox_visuals(
    mut commands: Commands,
    query: Query<(bevy::ecs::entity::Entity, &HitboxVisual)>,
) {
    // Despawn any visuals whose fade timer has fully elapsed.
    for (entity, vis) in query.iter() {
        if vis.fade_remaining <= 0.0 {
            commands.entity(entity).despawn();
        }
    }
}

// ── Charge VFX ─────────────────────────────────────

#[derive(Component)]
struct ChargeEffect {
    remaining: f32,
}

fn spawn_charge_effects(
    mut commands: Commands,
    mut events: EventReader<ChargeStartEvent>,
    mut tier_events: EventReader<ChargeTierReachedEvent>,
    server_entities: Query<(&ServerEntity, &Transform)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for ev in events.read() {
        let pos = server_entities
            .iter()
            .find(|(se, _)| se.entity_id == ev.source_entity_id)
            .map(|(_, tf)| tf.translation + Vec3::Y * 0.8);

        if let Some(pos) = pos {
            let mat = materials.add(StandardMaterial {
                base_color: Color::srgb(0.6, 0.4, 1.0),
                emissive: LinearRgba::new(1.5, 1.0, 2.0, 1.0),
                alpha_mode: AlphaMode::Blend,
                ..default()
            });
            commands.spawn((
                Mesh3d(meshes.add(Sphere::new(0.12))),
                MeshMaterial3d(mat),
                Transform::from_translation(pos),
                ChargeEffect { remaining: 0.8 },
            ));
        }
    }

    for ev in tier_events.read() {
        let pos = server_entities
            .iter()
            .find(|(se, _)| se.entity_id == ev.source_entity_id)
            .map(|(_, tf)| tf.translation + Vec3::Y * 1.0);

        if let Some(pos) = pos {
            // Color by tier (brighter for higher tiers)
            let color = match ev.tier {
                0 => Color::srgb(0.8, 0.8, 0.8),
                1 => Color::srgb(1.0, 0.8, 0.2),
                2 => Color::srgb(1.0, 0.4, 0.9),
                _ => Color::srgb(1.0, 1.0, 1.0),
            };
            let mat = {
                let (r, g, b) = if let Color::Srgba(c) = color {
                    (c.red, c.green, c.blue)
                } else {
                    (1.0, 1.0, 1.0)
                };
                materials.add(StandardMaterial {
                    base_color: color,
                    emissive: LinearRgba::new(r * 2.0, g * 2.0, b * 2.0, 1.0),
                    alpha_mode: AlphaMode::Blend,
                    ..default()
                })
            };
            commands.spawn((
                Mesh3d(meshes.add(Sphere::new(0.16 + 0.06 * ev.tier as f32))),
                MeshMaterial3d(mat),
                Transform::from_translation(pos),
                ChargeEffect { remaining: 0.9 },
            ));
        }
    }
}

fn update_charge_effects(
    mut commands: Commands,
    time: Res<Time>,
    mut query: Query<(bevy::ecs::entity::Entity, &mut ChargeEffect, &mut Transform)>,
) {
    let dt = time.delta_secs();
    for (entity, mut effect, mut tf) in query.iter_mut() {
        effect.remaining -= dt;
        let scale = (effect.remaining / 0.9).max(0.0);
        tf.scale = Vec3::splat(scale);
        if effect.remaining <= 0.0 {
            commands.entity(entity).despawn();
        }
    }
}
