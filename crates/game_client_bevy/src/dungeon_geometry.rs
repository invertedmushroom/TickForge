use bevy::prelude::*;
use game_client::module_bindings::{InstanceMembershipTableAccess, InstanceTableAccess};
use game_schema::dungeon::{DungeonFile, DungeonTemplate, ShapeDef};
use spacetimedb_sdk::Table;

use crate::spacetime::{LocalPlayerEntity, StdbConnection};

pub struct DungeonGeometryPlugin;

impl Plugin for DungeonGeometryPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, track_instance_geometry);
    }
}

// ── Embedded dungeon data ───────────────────────────────────────────

static DUNGEON_TEMPLATES: std::sync::OnceLock<Vec<DungeonTemplate>> = std::sync::OnceLock::new();

fn all_templates() -> &'static [DungeonTemplate] {
    DUNGEON_TEMPLATES.get_or_init(|| {
        let src = include_str!("../../../data/dungeons.ron");
        let file = ron::from_str::<DungeonFile>(src)
            .expect("data/dungeons.ron embedded at compile time must be valid RON");
        file.templates
    })
}

fn find_template(template_id: &str) -> Option<&'static DungeonTemplate> {
    all_templates()
        .iter()
        .find(|t| t.template_id == template_id)
}

// ── Bevy resources and components ───────────────────────────────────

/// Tracks the currently-active instance so we know when to spawn/despawn geometry.
#[derive(Resource)]
struct ActiveDungeon {
    instance_id: u64,
}

/// Tag on spawned dungeon geometry meshes for bulk despawn.
#[derive(Component)]
struct DungeonGeometry;

// ── System ──────────────────────────────────────────────────────────

fn track_instance_geometry(
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    active: Option<Res<ActiveDungeon>>,
    query: Query<Entity, With<DungeonGeometry>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let Some(stdb) = stdb else { return };
    let entity_id = match local_player.entity_id {
        Some(eid) => eid,
        None => {
            // Player not yet spawned — if we had geometry, despawn it.
            if active.is_some() {
                despawn_geometry(&query, &mut commands);
                commands.remove_resource::<ActiveDungeon>();
            }
            return;
        }
    };

    // Check current instance membership.
    let membership = stdb
        .conn
        .db
        .instance_membership()
        .iter()
        .find(|m| m.entity_id == entity_id);

    match membership {
        Some(m) => {
            // Player is in an instance — check if it's the same one we already rendered.
            if let Some(ad) = &active {
                if ad.instance_id == m.instance_id {
                    return; // Already showing the correct geometry.
                }
                // Different instance — despawn old geometry first.
                despawn_geometry(&query, &mut commands);
            }

            // Look up the instance row for the template_id.
            let instance = stdb.conn.db.instance().instance_id().find(&m.instance_id);
            let Some(inst) = instance else { return };

            // Find the dungeon template.
            let Some(template) = find_template(&inst.template_id) else {
                log::warn!(
                    "Dungeon template '{}' not found in embedded data",
                    inst.template_id
                );
                return;
            };

            spawn_geometry(template, &mut commands, &mut meshes, &mut materials);

            commands.insert_resource(ActiveDungeon {
                instance_id: m.instance_id,
            });
        }
        None => {
            // Player is not in an instance — despawn any geometry.
            if active.is_some() {
                despawn_geometry(&query, &mut commands);
                commands.remove_resource::<ActiveDungeon>();
            }
        }
    }
}

fn spawn_geometry(
    template: &DungeonTemplate,
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) {
    let wall_mat = materials.add(StandardMaterial {
        base_color: Color::srgba(0.5, 0.5, 0.55, 0.7),
        alpha_mode: AlphaMode::Blend,
        ..default()
    });
    let floor_mat = materials.add(StandardMaterial {
        base_color: Color::srgba(0.35, 0.35, 0.4, 0.8),
        alpha_mode: AlphaMode::Blend,
        ..default()
    });
    let pillar_mat = materials.add(StandardMaterial {
        base_color: Color::srgba(0.6, 0.55, 0.5, 0.75),
        alpha_mode: AlphaMode::Blend,
        ..default()
    });

    for geo in &template.geometry {
        let (mesh, mat) = match &geo.shape {
            ShapeDef::Cuboid {
                half_x,
                half_y,
                half_z,
            } => {
                let mesh = meshes.add(Cuboid::new(half_x * 2.0, half_y * 2.0, half_z * 2.0));
                // Use floor material for flat shapes, wall material otherwise.
                let mat = if *half_y < 1.0 {
                    floor_mat.clone()
                } else {
                    wall_mat.clone()
                };
                (mesh, mat)
            }
            ShapeDef::Cylinder {
                half_height,
                radius,
            } => {
                let mesh = meshes.add(Cylinder::new(*radius, half_height * 2.0));
                (mesh, pillar_mat.clone())
            }
        };

        commands.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::from_xyz(geo.position[0], geo.position[1], geo.position[2]),
            DungeonGeometry,
        ));
    }

    log::info!(
        "Spawned {} dungeon geometry meshes for '{}'",
        template.geometry.len(),
        template.template_id,
    );
}

fn despawn_geometry(query: &Query<Entity, With<DungeonGeometry>>, commands: &mut Commands) {
    let count = query.iter().count();
    for entity in query.iter() {
        commands.entity(entity).despawn();
    }
    if count > 0 {
        log::info!("Despawned {count} dungeon geometry meshes");
    }
}
