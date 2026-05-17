use bevy::prelude::*;

use crate::sync::ServerEntity;
#[cfg(feature = "connected")]
use game_client::module_bindings::respawn_player;
#[allow(unused_imports)]
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
        app.add_event::<SkillObjectRemoveEvent>();
        app.add_event::<HazardSpawnEvent>();
        app.add_event::<ContactHitboxSpawnEvent>();
        app.add_event::<HitboxSpawnedEvent>();
        app.add_event::<HitboxDamageFrameEvent>();
        app.add_event::<HitboxRemovedEvent>();
        app.add_event::<BuffAppliedVfxEvent>();
        app.add_event::<TeleportVfxEvent>();
        app.add_event::<TelegraphVfxEvent>();
        app.add_event::<AreaTelegraphVfxEvent>();
        app.add_event::<EncounterCueVfxEvent>();
        app.add_systems(
            Update,
            (
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
                update_hazard_visuals,
                spawn_contact_hitbox_visuals,
                update_contact_hitbox_visuals,
                spawn_hitbox_visuals,
                update_hitbox_visuals,
                remove_hitbox_visuals,
            ),
        );
        app.add_systems(
            Update,
            (
                despawn_hazard_visuals,
                despawn_stale_hazards_on_layer_change,
                spawn_buff_applied_effects,
                update_buff_flash_effects,
                spawn_teleport_effects,
                update_teleport_effects,
                spawn_telegraph_visuals,
                update_telegraph_visuals,
                spawn_area_telegraph_visuals,
                update_area_telegraph_visuals,
                spawn_encounter_cue_visuals,
                update_encounter_cue_visuals,
            ),
        );
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
        1 => Color::srgb(1.0, 1.0, 0.3), // Slash — gold flash
        2 => Color::srgb(1.0, 0.5, 0.0), // Fireball — orange flash
        3 => Color::srgb(0.7, 0.3, 1.0), // Smash — purple flash
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
            log::info!(
                "Death marker spawned for entity #{} at {:?}",
                ev.entity_id,
                pos
            );
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
    #[cfg(feature = "connected")] stdb: Option<Res<crate::spacetime::StdbConnection>>,
    #[cfg(feature = "connected")] mut local_player: ResMut<crate::spacetime::LocalPlayerEntity>,
) {
    if !keyboard.just_pressed(KeyCode::KeyR) {
        return;
    }

    // Two paths trigger a respawn:
    //   1. Death screen up → standard "press R to respawn" after 2s grace.
    //   2. No death screen → live self-rescue (e.g. fell out of the map,
    //      stuck on geometry, stranded on an orphan dungeon layer).
    //      The server's `respawn_player` reducer accepts both.
    let mut had_screen = false;
    for (entity, screen) in screen_q.iter() {
        // Only allow respawn after 2 seconds.
        if screen.age < 2.0 {
            return;
        }
        commands.entity(entity).despawn_recursive();
        had_screen = true;
    }
    let _ = had_screen; // both paths fall through to the reducer call.

    #[cfg(feature = "connected")]
    {
        // Reset local player tracking so detect_local_player re-detects.
        local_player.entity_id = None;
        local_player.spawned = false;

        if let Some(stdb) = stdb {
            // Call the dedicated respawn reducer after local death-screen
            // teardown so the client re-enters the normal spawn flow.
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
/// Stores the world-space origin so it can be projected to screen coordinates.
#[derive(Component)]
struct DamageNumber {
    remaining: f32,
    /// Upward drift speed in world-space units/sec.
    velocity_y: f32,
    /// World position above the target at spawn time; drifts up over time.
    world_pos: Vec3,
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
            .map(|(_, tf)| tf.translation + Vec3::Y * 1.8);

        let Some(world_pos) = pos else { continue };

        let (color, label) = if ev.amount < 0.0 {
            // Heal — show green with "+" prefix.
            (
                Color::srgb(0.2, 1.0, 0.3),
                format!("+{:.0}", ev.amount.abs()),
            )
        } else if ev.is_self {
            (Color::srgb(1.0, 0.2, 0.2), format!("{:.0}", ev.amount))
        } else if ev.is_crit {
            (Color::srgb(1.0, 0.8, 0.0), format!("{:.0}", ev.amount))
        } else {
            (Color::srgb(1.0, 1.0, 1.0), format!("{:.0}", ev.amount))
        };

        let font_size = if ev.is_crit { 28.0 } else { 22.0 };

        commands.spawn((
            Text::new(label),
            TextFont {
                font_size,
                ..default()
            },
            TextColor(color),
            Node {
                position_type: PositionType::Absolute,
                left: Val::Percent(50.0),
                top: Val::Percent(40.0),
                ..default()
            },
            DamageNumber {
                remaining: 1.5,
                velocity_y: 1.5,
                world_pos,
            },
        ));
    }
}

fn update_damage_numbers(
    mut commands: Commands,
    time: Res<Time>,
    camera_q: Query<(&Camera, &GlobalTransform), With<crate::camera::GameCamera>>,
    mut query: Query<(
        bevy::ecs::entity::Entity,
        &mut DamageNumber,
        &mut Node,
        &mut TextColor,
    )>,
) {
    let dt = time.delta_secs();
    let cam = camera_q.get_single().ok();

    for (entity, mut dmg, mut node, mut color) in query.iter_mut() {
        dmg.remaining -= dt;
        dmg.world_pos.y += dmg.velocity_y * dt;

        // Project world position to screen coordinates.
        if let Some((camera, cam_tf)) = cam {
            if let Some(ndc) = camera.world_to_ndc(cam_tf, dmg.world_pos) {
                // NDC ranges from -1..1; convert to 0..100 percent.
                let screen_x = (ndc.x + 1.0) * 50.0;
                let screen_y = (1.0 - ndc.y) * 50.0;
                node.left = Val::Percent(screen_x);
                node.top = Val::Percent(screen_y);
            }
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
    mut query: Query<(
        bevy::ecs::entity::Entity,
        &mut ClientProjectile,
        &mut Transform,
    )>,
    targets: Query<
        &Transform,
        (
            With<ServerEntity>,
            Without<crate::camera::LocalPlayer>,
            Without<ClientProjectile>,
        ),
    >,
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
#[derive(Event)]
pub struct HazardSpawnEvent {
    pub execution_id: u64,
    pub ability_id: u32,
    pub source_entity_id: u64,
    pub position: Vec3,
    pub radius: f32,
}

/// Marks a client-side hazard zone visual (ground circle).
#[derive(Component)]
struct HazardVisual {
    execution_id: u64,
    ability_id: u32,
    source_entity_id: u64,
    /// Brief red flash on periodic damage — resets automatically.
    flash_cooldown: f32,
}

const HAZARD_BASE_ALPHA: f32 = 0.4;
const HAZARD_BASE_EMISSIVE_RED: f32 = 1.5;
const HAZARD_BASE_EMISSIVE_GREEN: f32 = 0.6;
const HAZARD_BASE_EMISSIVE_BLUE: f32 = 0.2;
const HAZARD_FLASH_SECS: f32 = 0.15;
const HAZARD_FLASH_ALPHA: f32 = 0.6;
const HAZARD_FLASH_EMISSIVE_RED: f32 = 3.0;
const HAZARD_FLASH_EMISSIVE_GREEN: f32 = 0.2;

fn spawn_hazard_visuals(
    mut commands: Commands,
    mut events: EventReader<HazardSpawnEvent>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for ev in events.read() {
        // Use ability color for consistent visuals.
        let (_, ability_color) = crate::ability_visuals::visual_for(ev.ability_id);
        let c = ability_color.to_srgba();

        // Flat disc at the hazard position.
        let mesh = meshes.add(Circle::new(ev.radius));
        let mat = materials.add(StandardMaterial {
            base_color: Color::srgba(c.red, c.green, c.blue, HAZARD_BASE_ALPHA),
            emissive: LinearRgba::new(
                c.red * HAZARD_BASE_EMISSIVE_RED,
                c.green * HAZARD_BASE_EMISSIVE_GREEN,
                c.blue * HAZARD_BASE_EMISSIVE_BLUE,
                1.0,
            ),
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
                ability_id: ev.ability_id,
                source_entity_id: ev.source_entity_id,
                flash_cooldown: 0.0,
            },
        ));
    }
}

/// Flash hazard zone discs red on periodic damage frames.
fn update_hazard_visuals(
    mut events: EventReader<HitboxDamageFrameEvent>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    time: Res<Time>,
    mut query: Query<(&mut HazardVisual, &MeshMaterial3d<StandardMaterial>)>,
) {
    let flashes: Vec<(u64, u32)> = events
        .read()
        .map(|e| (e.source_entity_id, e.ability_id))
        .collect();

    let dt = time.delta_secs();
    for (mut hv, mat_handle) in query.iter_mut() {
        // Check for matching damage frame events.
        let should_flash = flashes
            .iter()
            .any(|(sid, aid)| *sid == hv.source_entity_id && *aid == hv.ability_id);
        if should_flash {
            hv.flash_cooldown = HAZARD_FLASH_SECS;
            if let Some(mat) = materials.get_mut(&mat_handle.0) {
                mat.base_color = Color::srgba(1.0, 0.1, 0.0, HAZARD_FLASH_ALPHA);
                mat.emissive = LinearRgba::new(
                    HAZARD_FLASH_EMISSIVE_RED,
                    HAZARD_FLASH_EMISSIVE_GREEN,
                    0.0,
                    1.0,
                );
            }
        }
        // Reset flash after cooldown.
        if hv.flash_cooldown > 0.0 {
            hv.flash_cooldown -= dt;
            if hv.flash_cooldown <= 0.0 {
                let (_, ability_color) = crate::ability_visuals::visual_for(hv.ability_id);
                let c = ability_color.to_srgba();
                if let Some(mat) = materials.get_mut(&mat_handle.0) {
                    mat.base_color = Color::srgba(c.red, c.green, c.blue, HAZARD_BASE_ALPHA);
                    mat.emissive = LinearRgba::new(
                        c.red * HAZARD_BASE_EMISSIVE_RED,
                        c.green * HAZARD_BASE_EMISSIVE_GREEN,
                        c.blue * HAZARD_BASE_EMISSIVE_BLUE,
                        1.0,
                    );
                }
            }
        }
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

// ── Contact-hitbox flash visuals ──────────────────────────────────────────────
//
// Emitted by the server's `on_contact -> SpawnHitbox` follow-up. Renders a
// short-lived expanding sphere at the spawn position that fades out over
// `duration_ticks` server ticks (server runs at 20Hz). Because the underlying
// hitbox is one-shot/short-lived, we drive the lifetime entirely from
// `duration_ticks` and do not require a corresponding remove event.
#[allow(dead_code)]
#[derive(Event)]
pub struct ContactHitboxSpawnEvent {
    pub execution_id: u64,
    pub parent_execution_id: u64,
    pub ability_id: u32,
    pub source_entity_id: u64,
    pub position: Vec3,
    pub radius: f32,
    pub duration_ticks: u32,
}

#[derive(Component)]
struct ContactHitboxVisual {
    age: f32,
    /// Total visible lifetime in seconds. Visual fades to zero alpha over this.
    lifetime: f32,
}

const SERVER_TICK_RATE_HZ: f32 = 20.0;
const CONTACT_HITBOX_MIN_LIFETIME_SECS: f32 = 0.20;
const CONTACT_HITBOX_BASE_ALPHA: f32 = 0.55;

fn spawn_contact_hitbox_visuals(
    mut commands: Commands,
    mut events: EventReader<ContactHitboxSpawnEvent>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for ev in events.read() {
        let (_, ability_color) = crate::ability_visuals::visual_for(ev.ability_id);
        let c = ability_color.to_srgba();

        let mesh = meshes.add(Sphere::new(ev.radius.max(0.1)));
        let mat = materials.add(StandardMaterial {
            base_color: Color::srgba(c.red, c.green, c.blue, CONTACT_HITBOX_BASE_ALPHA),
            emissive: LinearRgba::new(c.red * 2.0, c.green * 2.0, c.blue * 2.0, 1.0),
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            cull_mode: None,
            ..default()
        });

        let lifetime = ((ev.duration_ticks.max(1) as f32) / SERVER_TICK_RATE_HZ)
            .max(CONTACT_HITBOX_MIN_LIFETIME_SECS);

        commands.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::from_translation(ev.position),
            ContactHitboxVisual { age: 0.0, lifetime },
        ));
    }
}

fn update_contact_hitbox_visuals(
    mut commands: Commands,
    time: Res<Time>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut query: Query<(
        bevy::ecs::entity::Entity,
        &mut ContactHitboxVisual,
        &MeshMaterial3d<StandardMaterial>,
    )>,
) {
    let dt = time.delta_secs();
    for (e, mut v, mat_handle) in query.iter_mut() {
        v.age += dt;
        let t = (v.age / v.lifetime).clamp(0.0, 1.0);
        let alpha = CONTACT_HITBOX_BASE_ALPHA * (1.0 - t);
        if let Some(mat) = materials.get_mut(&mat_handle.0) {
            let base = mat.base_color.to_srgba();
            mat.base_color = Color::srgba(base.red, base.green, base.blue, alpha);
        }
        if v.age >= v.lifetime {
            commands.entity(e).despawn();
        }
    }
}

/// Clears ground-placed hazard discs when the local player's layer changes.
///
/// Hazard zones on the server are layer-scoped (their sensor user_data is
/// stamped with the layer where they were spawned) and only damage entities
/// on the same layer. But `HazardSpawnEvent` is one-shot — the client has
/// no way to "see" hazards that were spawned before it joined a new layer,
/// and conversely any stale disc from the previous layer will keep rendering
/// forever unless it's explicitly removed. On a layer transition, drop every
/// local hazard visual so the view matches the server authority for the
/// current layer.
fn despawn_stale_hazards_on_layer_change(
    mut commands: Commands,
    stdb: Option<Res<crate::spacetime::StdbConnection>>,
    local_player: Option<Res<crate::spacetime::LocalPlayerEntity>>,
    hazards: Query<bevy::ecs::entity::Entity, With<HazardVisual>>,
    mut last_layer: Local<Option<u32>>,
) {
    use game_client::module_bindings::*;
    use spacetimedb_sdk::Table;
    let Some(stdb) = stdb else { return };
    let Some(lp) = local_player else { return };
    let Some(entity_id) = lp.entity_id else {
        return;
    };
    let current = stdb
        .conn
        .db
        .my_region()
        .iter()
        .find(|r| r.entity_id == entity_id)
        .map(|r| r.layer);
    let Some(cur) = current else { return };
    match *last_layer {
        Some(prev) if prev == cur => {}
        _ => {
            if last_layer.is_some() {
                // Layer changed — clear every hazard disc we currently have.
                for e in hazards.iter() {
                    commands.entity(e).despawn();
                }
            }
            *last_layer = Some(cur);
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

/// A live client-side hitbox visual. Fades out over `HITBOX_FADE_SECS` at end of life.
#[derive(Component)]
struct HitboxVisual {
    source_entity_id: u64,
    ability_id: u32,
    /// Total remaining lifetime in seconds (includes fade-out period at the end).
    remaining: f32,
    /// Whether the damage frame has already been applied (controls flash state).
    damage_flashed: bool,
    /// Time of last damage flash — resets for periodic abilities.
    flash_cooldown: f32,
    /// Periodic abilities (auras) pulse on damage ticks instead of being always visible.
    is_periodic: bool,
    /// Configured pulse interval for periodic visuals, derived from `damage_interval_ticks`.
    pulse_interval: f32,
    /// Countdown until the next ambient pulse starts.
    pulse_cooldown: f32,
}

/// Fade-out duration at the end of a hitbox visual's life.
const HITBOX_FADE_SECS: f32 = 0.5;
const HITBOX_BASE_ALPHA: f32 = 0.35;
const HITBOX_BASE_EMISSIVE_RED: f32 = 1.5;
const HITBOX_BASE_EMISSIVE_GREEN: f32 = 1.5;
const HITBOX_BASE_EMISSIVE_BLUE: f32 = 0.5;
const HITBOX_PERIODIC_IDLE_ALPHA: f32 = 0.08;
const HITBOX_PERIODIC_IDLE_EMISSIVE_RED: f32 = 0.25;
const HITBOX_PERIODIC_IDLE_EMISSIVE_GREEN: f32 = 0.25;
const HITBOX_PERIODIC_IDLE_EMISSIVE_BLUE: f32 = 0.1;
const HITBOX_CONTACT_FLASH_SECS: f32 = 0.15;
const HITBOX_PERIODIC_CONTACT_FLASH_SECS: f32 = 0.35;
const HITBOX_CONTACT_FLASH_ALPHA: f32 = 0.7;
const HITBOX_CONTACT_FLASH_EMISSIVE: f32 = 3.0;
const HITBOX_PERIODIC_PULSE_WINDOW_SECS: f32 = 0.35;
const HITBOX_PERIODIC_PULSE_ALPHA_BOOST: f32 = 0.32;
const HITBOX_PERIODIC_PULSE_EMISSIVE_BASE: f32 = 0.2;
const HITBOX_PERIODIC_PULSE_EMISSIVE_BOOST: f32 = 1.4;
const HITBOX_PERIODIC_PULSE_BLUE_SCALE: f32 = 0.4;

fn spawn_hitbox_visuals(
    mut commands: Commands,
    mut events: EventReader<HitboxSpawnedEvent>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    server_entities: Query<(&ServerEntity, &Transform)>,
) {
    use crate::ability_bar::all_abilities;
    use crate::ability_visuals::{AbilityShape, visual_for};

    for ev in events.read() {
        // Find caster position and rotation.
        let caster_tf = server_entities
            .iter()
            .find(|(se, _)| se.entity_id == ev.source_entity_id)
            .map(|(_, tf)| *tf);
        let Some(caster_tf) = caster_tf else { continue };

        let def = all_abilities().iter().find(|a| a.id == ev.ability_id);
        let Some(def) = def else { continue };

        let mesh = match def.shape {
            AbilityShape::Capsule {
                radius,
                half_height,
            } => meshes.add(Capsule3d::new(radius, half_height * 2.0)),
            AbilityShape::Sphere { radius } => meshes.add(Sphere::new(radius)),
            AbilityShape::None => continue,
        };

        // Offset in entity-local space, rotated by caster facing.
        let local_offset = Vec3::new(def.offset[0], def.offset[1], def.offset[2]);
        let origin = caster_tf.translation + caster_tf.rotation * local_offset;

        // Use ability color for consistent visuals.
        let (_, ability_color) = visual_for(ev.ability_id);
        let base = ability_color.to_srgba();
        let material = materials.add(StandardMaterial {
            base_color: Color::srgba(base.red, base.green, base.blue, HITBOX_BASE_ALPHA),
            emissive: LinearRgba::new(
                base.red * HITBOX_BASE_EMISSIVE_RED,
                base.green * HITBOX_BASE_EMISSIVE_GREEN,
                base.blue * HITBOX_BASE_EMISSIVE_BLUE,
                1.0,
            ),
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            ..default()
        });

        let mut spawn_tf = Transform::from_translation(origin);
        spawn_tf.rotation = caster_tf.rotation;

        // Lingering abilities persist for their full duration; single-frame ones fade quickly.
        let lifetime = if def.linger_ticks > 0 {
            def.linger_ticks as f32 / 20.0 // 20 Hz tick rate
        } else {
            HITBOX_FADE_SECS
        };

        let is_periodic = def.damage_interval_ticks > 0;
        let pulse_interval = if is_periodic {
            (def.damage_interval_ticks as f32 / 20.0).max(0.1)
        } else {
            0.0
        };

        // Periodic abilities keep a faint idle presence and brighten on each pulse.
        let start_alpha = if is_periodic {
            HITBOX_PERIODIC_IDLE_ALPHA
        } else {
            HITBOX_BASE_ALPHA
        };
        let material = if is_periodic {
            materials.add(StandardMaterial {
                base_color: Color::srgba(base.red, base.green, base.blue, start_alpha),
                emissive: LinearRgba::new(
                    base.red * HITBOX_PERIODIC_IDLE_EMISSIVE_RED,
                    base.green * HITBOX_PERIODIC_IDLE_EMISSIVE_GREEN,
                    base.blue * HITBOX_PERIODIC_IDLE_EMISSIVE_BLUE,
                    1.0,
                ),
                alpha_mode: AlphaMode::Blend,
                unlit: true,
                ..default()
            })
        } else {
            material
        };

        commands.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(material),
            spawn_tf,
            HitboxVisual {
                source_entity_id: ev.source_entity_id,
                ability_id: ev.ability_id,
                remaining: lifetime,
                damage_flashed: false,
                flash_cooldown: 0.0,
                is_periodic,
                pulse_interval,
                pulse_cooldown: 0.0,
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

        // Flash on damage frame, then reset after a short cooldown.
        let should_flash = flashes
            .iter()
            .any(|(sid, aid)| *sid == vis.source_entity_id && *aid == vis.ability_id);
        if should_flash {
            vis.damage_flashed = true;
            vis.flash_cooldown = if vis.is_periodic {
                HITBOX_PERIODIC_CONTACT_FLASH_SECS
            } else {
                HITBOX_CONTACT_FLASH_SECS
            };
            if let Some(mat) = materials.get_mut(&mat_handle.0) {
                mat.base_color = Color::srgba(1.0, 0.0, 0.0, HITBOX_CONTACT_FLASH_ALPHA);
                mat.emissive = LinearRgba::new(HITBOX_CONTACT_FLASH_EMISSIVE, 0.0, 0.0, 1.0);
            }
        }

        if vis.is_periodic {
            vis.pulse_cooldown -= dt;
            while vis.pulse_cooldown <= 0.0 {
                vis.pulse_cooldown += vis.pulse_interval;
            }
        }

        // Reset flash after cooldown so periodic abilities can flash again.
        if vis.damage_flashed && vis.flash_cooldown > 0.0 {
            vis.flash_cooldown -= dt;
            if vis.flash_cooldown <= 0.0 {
                vis.damage_flashed = false;
            }
        }

        vis.remaining -= dt;

        // Fade-out only during the last HITBOX_FADE_SECS.
        let alpha_frac = (vis.remaining / HITBOX_FADE_SECS).clamp(0.0, 1.0);

        if let Some(mat) = materials.get_mut(&mat_handle.0) {
            if vis.damage_flashed {
                // Real contact flash: bright red and short-lived.
                let flash_frac = if vis.is_periodic {
                    (vis.flash_cooldown / HITBOX_PERIODIC_CONTACT_FLASH_SECS).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                mat.base_color = Color::srgba(
                    1.0,
                    0.0,
                    0.0,
                    alpha_frac * HITBOX_CONTACT_FLASH_ALPHA * flash_frac,
                );
                mat.emissive = LinearRgba::new(
                    HITBOX_CONTACT_FLASH_EMISSIVE * alpha_frac * flash_frac,
                    0.0,
                    0.0,
                    1.0,
                );
            } else if vis.is_periodic {
                // Ambient aura pulse runs on the configured damage interval even if nothing is hit.
                let (_, ability_color) = crate::ability_visuals::visual_for(vis.ability_id);
                let c = ability_color.to_srgba();
                let pulse_window = vis.pulse_interval.min(HITBOX_PERIODIC_PULSE_WINDOW_SECS);
                let pulse_frac = if vis.pulse_cooldown >= vis.pulse_interval - pulse_window {
                    ((vis.pulse_cooldown - (vis.pulse_interval - pulse_window)) / pulse_window)
                        .clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let pulse_strength = 1.0 - pulse_frac;
                let aura_alpha = (HITBOX_PERIODIC_IDLE_ALPHA
                    + HITBOX_PERIODIC_PULSE_ALPHA_BOOST * pulse_strength)
                    * alpha_frac;
                let emissive_strength = (HITBOX_PERIODIC_PULSE_EMISSIVE_BASE
                    + HITBOX_PERIODIC_PULSE_EMISSIVE_BOOST * pulse_strength)
                    * alpha_frac;
                mat.base_color = Color::srgba(c.red, c.green, c.blue, aura_alpha);
                mat.emissive = LinearRgba::new(
                    c.red * emissive_strength,
                    c.green * emissive_strength,
                    c.blue * emissive_strength * HITBOX_PERIODIC_PULSE_BLUE_SCALE,
                    1.0,
                );
            } else {
                // Non-periodic: steady ability-colored glow with alpha fade at end of life.
                let (_, ability_color) = crate::ability_visuals::visual_for(vis.ability_id);
                let c = ability_color.to_srgba();
                mat.base_color =
                    Color::srgba(c.red, c.green, c.blue, alpha_frac * HITBOX_BASE_ALPHA);
                mat.emissive = LinearRgba::new(
                    c.red * HITBOX_BASE_EMISSIVE_RED * alpha_frac,
                    c.green * HITBOX_BASE_EMISSIVE_GREEN * alpha_frac,
                    c.blue * HITBOX_BASE_EMISSIVE_BLUE * alpha_frac,
                    1.0,
                );
            }
        }
    }
}

fn remove_hitbox_visuals(
    mut commands: Commands,
    mut events: EventReader<HitboxRemovedEvent>,
    mut query: Query<(bevy::ecs::entity::Entity, &mut HitboxVisual)>,
) {
    // Server-authoritative early removal (stunbreak interrupts, etc.).
    let removals: Vec<(u64, u32)> = events
        .read()
        .map(|e| (e.source_entity_id, e.ability_id))
        .collect();
    for (entity, mut vis) in query.iter_mut() {
        if removals
            .iter()
            .any(|(sid, aid)| *sid == vis.source_entity_id && *aid == vis.ability_id)
        {
            // Force immediate fade-out instead of waiting for full linger timer.
            vis.remaining = vis.remaining.min(HITBOX_FADE_SECS);
        }
        // Despawn any visuals whose lifetime has fully elapsed.
        if vis.remaining <= 0.0 {
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

// ── Buff/Debuff application VFX ──────────────────────────

/// Emitted when a buff or debuff is applied to an entity.
#[derive(Event)]
pub struct BuffAppliedVfxEvent {
    pub target_entity_id: u64,
    #[allow(dead_code)] // reserved for per-buff VFX differentiation
    pub buff_id: u32,
    pub is_boon: bool,
}

/// Brief colored flash at the target: green ring for boons, red for conditions.
#[derive(Component)]
struct BuffFlashEffect {
    remaining: f32,
}

#[derive(Event)]
pub struct TeleportVfxEvent {
    #[allow(dead_code)] // reserved for observer-specific teleport effects
    pub entity_id: u64,
    pub from: Vec3,
    pub to: Vec3,
}

#[derive(Component)]
struct TeleportFlashEffect {
    remaining: f32,
}

fn spawn_buff_applied_effects(
    mut commands: Commands,
    mut events: EventReader<BuffAppliedVfxEvent>,
    server_entities: Query<(&ServerEntity, &Transform)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for ev in events.read() {
        let pos = server_entities
            .iter()
            .find(|(se, _)| se.entity_id == ev.target_entity_id)
            .map(|(_, tf)| tf.translation + Vec3::Y * 0.1);

        let Some(pos) = pos else { continue };

        let (color, emissive) = if ev.is_boon {
            (
                Color::srgba(0.2, 1.0, 0.4, 0.5),
                LinearRgba::new(0.5, 2.0, 0.8, 1.0),
            )
        } else {
            (
                Color::srgba(1.0, 0.2, 0.2, 0.5),
                LinearRgba::new(2.0, 0.3, 0.3, 1.0),
            )
        };

        let mesh = meshes.add(Torus::new(0.6, 0.8));
        let mat = materials.add(StandardMaterial {
            base_color: color,
            emissive,
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            ..default()
        });
        commands.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::from_translation(pos),
            BuffFlashEffect { remaining: 0.6 },
        ));
    }
}

fn update_buff_flash_effects(
    mut commands: Commands,
    time: Res<Time>,
    mut query: Query<(
        bevy::ecs::entity::Entity,
        &mut BuffFlashEffect,
        &mut Transform,
        &MeshMaterial3d<StandardMaterial>,
    )>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let dt = time.delta_secs();
    for (entity, mut effect, mut tf, mat_handle) in query.iter_mut() {
        effect.remaining -= dt;
        let frac = (effect.remaining / 0.6).max(0.0);
        tf.scale = Vec3::splat(1.0 + (1.0 - frac) * 0.5); // expand outward
        if let Some(mat) = materials.get_mut(&mat_handle.0) {
            let c = mat.base_color.to_srgba();
            mat.base_color = Color::srgba(c.red, c.green, c.blue, frac * 0.5);
        }
        if effect.remaining <= 0.0 {
            commands.entity(entity).despawn();
        }
    }
}

fn spawn_teleport_effects(
    mut commands: Commands,
    mut events: EventReader<TeleportVfxEvent>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for ev in events.read() {
        for (position, color, emissive) in [
            (
                ev.from + Vec3::Y * 0.2,
                Color::srgba(0.4, 0.9, 1.0, 0.45),
                LinearRgba::new(0.5, 1.5, 2.0, 1.0),
            ),
            (
                ev.to + Vec3::Y * 0.2,
                Color::srgba(1.0, 0.95, 0.5, 0.55),
                LinearRgba::new(2.0, 1.8, 0.6, 1.0),
            ),
        ] {
            let mesh = meshes.add(Torus::new(0.45, 0.65));
            let mat = materials.add(StandardMaterial {
                base_color: color,
                emissive,
                alpha_mode: AlphaMode::Blend,
                unlit: true,
                ..default()
            });
            commands.spawn((
                Mesh3d(mesh),
                MeshMaterial3d(mat),
                Transform::from_translation(position),
                TeleportFlashEffect { remaining: 0.45 },
            ));
        }
    }
}

fn update_teleport_effects(
    mut commands: Commands,
    time: Res<Time>,
    mut query: Query<(
        bevy::ecs::entity::Entity,
        &mut TeleportFlashEffect,
        &mut Transform,
        &MeshMaterial3d<StandardMaterial>,
    )>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let dt = time.delta_secs();
    for (entity, mut effect, mut tf, mat_handle) in query.iter_mut() {
        effect.remaining -= dt;
        let frac = (effect.remaining / 0.45).max(0.0);
        tf.scale = Vec3::splat(1.0 + (1.0 - frac) * 0.8);
        if let Some(mat) = materials.get_mut(&mat_handle.0) {
            let color = mat.base_color.to_srgba();
            mat.base_color = Color::srgba(color.red, color.green, color.blue, frac * color.alpha);
        }
        if effect.remaining <= 0.0 {
            commands.entity(entity).despawn();
        }
    }
}

// ── Visual Telegraphs ───────────────────────────────────────────────────

#[derive(Event)]
pub struct TelegraphVfxEvent {
    pub target: u64,
    pub impact_tick: u64,
}

#[derive(Component)]
struct TelegraphVisual {
    target: u64,
    impact_tick: u64,
}

fn spawn_telegraph_visuals(mut commands: Commands, mut events: EventReader<TelegraphVfxEvent>) {
    for ev in events.read() {
        commands.spawn(TelegraphVisual {
            target: ev.target,
            impact_tick: ev.impact_tick,
        });
    }
}

fn update_telegraph_visuals(
    mut commands: Commands,
    mut gizmos: Gizmos,
    tick_counter: Option<Res<crate::spacetime::TickCounter>>,
    query: Query<(bevy::ecs::entity::Entity, &TelegraphVisual)>,
    targets: Query<(&ServerEntity, &Transform)>,
) {
    let current_tick = tick_counter.map(|tc| tc.last_tick).unwrap_or(0);

    for (entity, visual) in query.iter() {
        if current_tick >= visual.impact_tick {
            commands.entity(entity).despawn();
            continue;
        }

        let ticks_left = visual.impact_tick - current_tick;
        let progress = 1.0 - (ticks_left as f32 / 40.0).clamp(0.0, 1.0); // Assuming 2s cast time = 40 ticks max

        if let Some((_, target_tf)) = targets.iter().find(|(se, _)| se.entity_id == visual.target) {
            let pos = target_tf.translation + Vec3::Y * 0.1;
            // Draw a red ring that grows inward
            let radius = 2.0 - progress * 1.5;
            let alpha = 0.2 + progress * 0.8;
            gizmos.circle(
                Isometry3d::new(pos, Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)),
                radius,
                Color::srgba(1.0, 0.1, 0.1, alpha),
            );
        }
    }
}

#[derive(Event)]
pub struct AreaTelegraphVfxEvent {
    pub position: Vec3,
    pub radius: f32,
    pub shape: String,
    pub impact_tick: u64,
}

#[derive(Component)]
struct AreaTelegraphVisual {
    position: Vec3,
    radius: f32,
    #[allow(dead_code)]
    shape: String,
    impact_tick: u64,
}

fn spawn_area_telegraph_visuals(
    mut commands: Commands,
    mut events: EventReader<AreaTelegraphVfxEvent>,
) {
    for ev in events.read() {
        commands.spawn(AreaTelegraphVisual {
            position: ev.position,
            radius: ev.radius,
            shape: ev.shape.clone(),
            impact_tick: ev.impact_tick,
        });
    }
}

fn update_area_telegraph_visuals(
    mut commands: Commands,
    mut gizmos: Gizmos,
    tick_counter: Option<Res<crate::spacetime::TickCounter>>,
    query: Query<(bevy::ecs::entity::Entity, &AreaTelegraphVisual)>,
) {
    let current_tick = tick_counter.map(|tc| tc.last_tick).unwrap_or(0);

    for (entity, visual) in query.iter() {
        if current_tick >= visual.impact_tick {
            commands.entity(entity).despawn();
            continue;
        }

        let ticks_left = visual.impact_tick - current_tick;
        // Assume 2s (40 ticks) max for standard visual scaling
        let progress = 1.0 - (ticks_left as f32 / 40.0).clamp(0.0, 1.0);

        let pos = visual.position + Vec3::Y * 0.1;
        // Draw a ground circle that grows inward (warning ring)
        let ring_radius = visual.radius * (1.0 + (1.0 - progress) * 0.5);
        let alpha = 0.3 + progress * 0.7;

        gizmos.circle(
            Isometry3d::new(pos, Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)),
            visual.radius,
            Color::srgba(1.0, 0.2, 0.2, alpha * 0.2), // Faint fill area
        );

        gizmos.circle(
            Isometry3d::new(pos, Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)),
            ring_radius,
            Color::srgba(1.0, 0.1, 0.1, alpha), // Bright warning ring
        );
    }
}

// ── Encounter Cue Ring Visuals ─────────────────────────────────────────────────
//
// EncounterCue shapes are persistent spatial overlays anchored to a boss entity.
// They follow the anchor entity each frame and expire at a server tick boundary.
// Used for mechanics like alternating ring buffs where players must track zone
// boundaries over multiple ticks.

/// Emitted from the combat event poll when an EncounterCue arrives.
#[derive(Event)]
pub struct EncounterCueVfxEvent {
    pub cue_id: String,
    pub anchor_entity: Option<u64>,
    pub position: Vec3,
    pub inner_radius: f32,
    pub outer_radius: f32,
    pub expires_at_tick: u64,
}

/// Marks a live encounter cue ring visual in the world.
#[derive(Component)]
struct EncounterCueVisual {
    cue_id: String,
    anchor_entity: Option<u64>,
    position: Vec3,
    inner_radius: f32,
    outer_radius: f32,
    expires_at_tick: u64,
}

fn spawn_encounter_cue_visuals(
    mut commands: Commands,
    mut events: EventReader<EncounterCueVfxEvent>,
    // Despawn any old visual with the same cue_id before respawning.
    existing: Query<(bevy::ecs::entity::Entity, &EncounterCueVisual)>,
) {
    for ev in events.read() {
        // Replace any stale visual for the same cue_id.
        for (entity, vis) in existing.iter() {
            if vis.cue_id == ev.cue_id {
                commands.entity(entity).despawn();
            }
        }
        commands.spawn(EncounterCueVisual {
            cue_id: ev.cue_id.clone(),
            anchor_entity: ev.anchor_entity,
            position: ev.position,
            inner_radius: ev.inner_radius,
            outer_radius: ev.outer_radius,
            expires_at_tick: ev.expires_at_tick,
        });
    }
}

fn update_encounter_cue_visuals(
    mut commands: Commands,
    mut gizmos: Gizmos,
    tick_counter: Option<Res<crate::spacetime::TickCounter>>,
    mut query: Query<(bevy::ecs::entity::Entity, &mut EncounterCueVisual)>,
    server_entities: Query<(&ServerEntity, &Transform)>,
) {
    let current_tick = tick_counter.map(|tc| tc.last_tick).unwrap_or(0);

    for (entity, mut visual) in query.iter_mut() {
        // Expire when the server tick passes the cue's end.
        if current_tick > 0 && current_tick >= visual.expires_at_tick {
            commands.entity(entity).despawn();
            continue;
        }

        // Track anchor entity position if present.
        if let Some(anchor_id) = visual.anchor_entity {
            if let Some((_, tf)) = server_entities
                .iter()
                .find(|(se, _)| se.entity_id == anchor_id)
            {
                // Only update XZ; keep Y from the cue's spawn position.
                visual.position.x = tf.translation.x;
                visual.position.z = tf.translation.z;
            }
        }

        // Fade out during last 20 ticks of lifetime.
        let ticks_left = visual.expires_at_tick.saturating_sub(current_tick);
        let alpha = (ticks_left as f32 / 20.0).clamp(0.0, 1.0);
        // Gold color distinguishes these from red combat telegraphs.
        let color = Color::srgba(1.0, 0.85, 0.15, 0.7 * alpha.max(0.25));

        let base_pos = visual.position + Vec3::Y * 0.15;
        let iso = Isometry3d::new(base_pos, Quat::from_rotation_x(std::f32::consts::FRAC_PI_2));

        // Draw inner boundary (if non-zero) and outer boundary.
        if visual.inner_radius > 0.01 {
            gizmos.circle(iso, visual.inner_radius, color);
        }
        gizmos.circle(iso, visual.outer_radius, color);
    }
}
