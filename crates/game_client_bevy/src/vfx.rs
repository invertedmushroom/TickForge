use bevy::prelude::*;

use crate::sync::ServerEntity;
#[cfg(feature = "connected")]
use game_client::module_bindings::spawn_player;

pub struct VfxPlugin;

impl Plugin for VfxPlugin {
    fn build(&self, app: &mut App) {
        app.add_event::<DeathNotification>();
        app.add_event::<DamageNumberEvent>();
        app.add_event::<ChargeStartEvent>();
        app.add_event::<ChargeTierReachedEvent>();
        app.add_event::<ProjectileLaunchEvent>();
        app.add_event::<ProjectileRemoveEvent>();
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

/// Handle R key press to respawn: despawn death screen, call spawn_player.
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
            if let Err(e) = stdb.conn.reducers.spawn_player() {
                log::error!("Respawn failed: {e}");
            } else {
                log::info!("Respawn: spawn_player called");
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
#[derive(Event)]
pub struct ChargeStartEvent {
    pub source_entity_id: u64,
    pub ability_id: u32,
    pub max_ticks: u32,
}

/// Emitted when a charge tier is reached during charging.
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
pub struct ProjectileRemoveEvent {
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
) {
    let dt = time.delta_secs();
    for (entity, mut proj, mut tf) in query.iter_mut() {
        let step = proj.speed * dt;
        tf.translation += proj.direction * step;
        proj.remaining_range -= step;
        if proj.remaining_range <= 0.0 {
            commands.entity(entity).despawn();
        }
    }
}

fn despawn_client_projectiles(
    mut commands: Commands,
    mut events: EventReader<ProjectileRemoveEvent>,
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
