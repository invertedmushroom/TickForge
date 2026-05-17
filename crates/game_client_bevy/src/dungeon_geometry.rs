use bevy::prelude::*;
use bevy::render::mesh::{Indices, PrimitiveTopology};
use bevy::render::render_asset::RenderAssetUsages;
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

/// Tracks the currently-active instance so we know when to spawn/despawn
/// geometry. Also exposes the active template so other systems (e.g. the
/// crosshair) can bilinearly sample heightfield terrain for a correct Y.
#[derive(Resource)]
pub(crate) struct ActiveDungeon {
    instance_id: u64,
    pub(crate) template: &'static DungeonTemplate,
}

/// Tag on spawned dungeon geometry meshes for bulk despawn.
#[derive(Component)]
struct DungeonGeometry;

/// Tag on the default ground plane spawned at startup so it can be
/// removed when an instance's dungeon geometry is shown.
#[derive(Component)]
pub struct DefaultGround;

// ── Terrain sampling (shared with crosshair / UI) ───────────────────

/// Bilinearly sample the heightfield layer(s) of a template at world (x, z).
/// Returns `Some(y)` when the point falls inside at least one heightfield
/// footprint; picks the highest Y when multiple overlap (player stands on
/// the topmost layer). Returns `None` when no heightfield covers the point.
pub(crate) fn sample_terrain_y(template: &DungeonTemplate, x: f32, z: f32) -> Option<f32> {
    let mut best: Option<f32> = None;
    for geo in &template.geometry {
        let ShapeDef::Heightfield {
            nrows,
            ncols,
            scale_x,
            scale_y,
            scale_z,
            heights,
        } = &geo.shape
        else {
            continue;
        };
        if *nrows < 2 || *ncols < 2 || heights.len() != nrows * ncols {
            continue;
        }
        let local_x = x - geo.position[0];
        let local_z = z - geo.position[2];
        let u = (local_x / scale_x) + 0.5;
        let v = (local_z / scale_z) + 0.5;
        if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
            continue;
        }
        let fi = u * (*nrows as f32 - 1.0);
        let fj = v * (*ncols as f32 - 1.0);
        let i0 = (fi.floor() as usize).min(*nrows - 2);
        let j0 = (fj.floor() as usize).min(*ncols - 2);
        let tx = fi - i0 as f32;
        let tz = fj - j0 as f32;
        // parry column-major: heights[i + j*nrows]
        let h00 = heights[i0 + j0 * nrows];
        let h10 = heights[(i0 + 1) + j0 * nrows];
        let h01 = heights[i0 + (j0 + 1) * nrows];
        let h11 = heights[(i0 + 1) + (j0 + 1) * nrows];
        let h0 = h00 * (1.0 - tx) + h10 * tx;
        let h1 = h01 * (1.0 - tx) + h11 * tx;
        let y = geo.position[1] + scale_y * (h0 * (1.0 - tz) + h1 * tz);
        best = Some(best.map_or(y, |prev| prev.max(y)));
    }
    best
}

// ── System ──────────────────────────────────────────────────────────

fn track_instance_geometry(
    stdb: Option<Res<StdbConnection>>,
    local_player: Res<LocalPlayerEntity>,
    active: Option<Res<ActiveDungeon>>,
    query: Query<Entity, With<DungeonGeometry>>,
    ground_query: Query<Entity, With<DefaultGround>>,
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

            // Despawn default ground if present so dungeon geometry doesn't z-fight.
            for ent in ground_query.iter() {
                commands.entity(ent).despawn();
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
                template,
            });
        }
        None => {
            // Player is not in an instance — despawn any geometry and restore
            // the default ground plane if it is missing.
            if active.is_some() {
                despawn_geometry(&query, &mut commands);
                commands.remove_resource::<ActiveDungeon>();

                if ground_query.iter().next().is_none() {
                    let ground_mesh = meshes.add(Cuboid::new(100.0, 0.2, 100.0));
                    let ground_mat = materials.add(StandardMaterial {
                        base_color: Color::srgb(0.24, 0.36, 0.25),
                        perceptual_roughness: 1.0,
                        reflectance: 0.0,
                        metallic: 0.0,
                        ..default()
                    });
                    commands.spawn((
                        Mesh3d(ground_mesh),
                        MeshMaterial3d(ground_mat),
                        Transform::from_xyz(0.0, -0.1, 0.0),
                        DefaultGround,
                    ));
                }
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
            ShapeDef::Heightfield {
                nrows,
                ncols,
                scale_x,
                scale_y,
                scale_z,
                heights,
            } => {
                // Build a grid mesh from the parry column-major height
                // array. Server remains authoritative for collision; this
                // mesh is purely visual.
                let mesh = meshes.add(build_heightfield_mesh(
                    *nrows, *ncols, *scale_x, *scale_y, *scale_z, heights,
                ));
                (mesh, floor_mat.clone())
            }
            ShapeDef::TriMesh { vertices, indices } => {
                // Indexed triangle mesh — used for editor-baked terrain.
                // Hand-authored RON dungeons today don't ship trimeshes;
                // this branch exists so the match stays exhaustive when the
                // voxel pipeline lands. Falls back to a small placeholder
                // cube if the layout is malformed.
                let mesh = meshes.add(build_trimesh_mesh(vertices, indices));
                (mesh, floor_mat.clone())
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

/// Build a triangulated grid mesh from a parry-format heightfield.
///
/// Vertex layout: row-major by `(i, j)` → `i * ncols + j`. Each cell emits
/// two triangles wound so the surface normal is +Y (face up). Smooth
/// normals are computed after index insertion.
///
/// Heights input is column-major (`heights[i + j*nrows]`) to match the
/// parry layout used by both `game_schema::ShapeDef::Heightfield` and the
/// server-side Rapier collider.
fn build_heightfield_mesh(
    nrows: usize,
    ncols: usize,
    scale_x: f32,
    scale_y: f32,
    scale_z: f32,
    heights: &[f32],
) -> Mesh {
    // Fallback for malformed input — keep the previous "flat placeholder"
    // behaviour rather than panicking.
    if nrows < 2 || ncols < 2 || heights.len() != nrows * ncols {
        log::error!(
            "heightfield mesh: bad dims (rows={nrows}, cols={ncols}, heights.len={}) — emitting flat fallback",
            heights.len()
        );
        return Cuboid::new(scale_x, scale_y.abs().max(0.1), scale_z).into();
    }

    let step_x = scale_x / (nrows as f32 - 1.0);
    let step_z = scale_z / (ncols as f32 - 1.0);

    let mut positions: Vec<[f32; 3]> = Vec::with_capacity(nrows * ncols);
    let mut uvs: Vec<[f32; 2]> = Vec::with_capacity(nrows * ncols);
    for i in 0..nrows {
        for j in 0..ncols {
            let x = -scale_x * 0.5 + i as f32 * step_x;
            let z = -scale_z * 0.5 + j as f32 * step_z;
            let y = heights[i + j * nrows] * scale_y;
            positions.push([x, y, z]);
            uvs.push([
                i as f32 / (nrows as f32 - 1.0),
                j as f32 / (ncols as f32 - 1.0),
            ]);
        }
    }

    let idx = |i: usize, j: usize| (i * ncols + j) as u32;
    let mut indices: Vec<u32> = Vec::with_capacity((nrows - 1) * (ncols - 1) * 6);
    for i in 0..nrows - 1 {
        for j in 0..ncols - 1 {
            // Winding chosen so the cross product points +Y.
            indices.extend_from_slice(&[idx(i, j), idx(i, j + 1), idx(i + 1, j)]);
            indices.extend_from_slice(&[idx(i + 1, j), idx(i, j + 1), idx(i + 1, j + 1)]);
        }
    }

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_indices(Indices::U32(indices));
    mesh.compute_smooth_normals();
    mesh
}

/// Build a Bevy mesh from a flat indexed triangle list. Vertices are
/// `[x, y, z, ...]`, indices are a flat `[i0, i1, i2, ...]` triangle list.
/// Falls back to a 1 m placeholder cube on malformed input.
fn build_trimesh_mesh(vertices: &[f32], indices: &[u32]) -> Mesh {
    let vert_count = vertices.len() / 3;
    if vertices.len() % 3 != 0
        || indices.len() % 3 != 0
        || vert_count < 3
        || indices.is_empty()
        || indices.iter().copied().max().unwrap_or(0) as usize >= vert_count
    {
        log::error!(
            "trimesh mesh: bad layout (vertices.len()={}, indices.len()={}) — emitting 1m fallback",
            vertices.len(),
            indices.len(),
        );
        return Cuboid::new(1.0, 1.0, 1.0).into();
    }
    let positions: Vec<[f32; 3]> = vertices.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_indices(Indices::U32(indices.to_vec()));
    mesh.compute_smooth_normals();
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_schema::dungeon::GeometryDef;

    fn template_with_heightfield(
        nrows: usize,
        ncols: usize,
        scale_x: f32,
        scale_y: f32,
        scale_z: f32,
        heights: Vec<f32>,
        position: [f32; 3],
    ) -> DungeonTemplate {
        DungeonTemplate {
            template_id: "test".into(),
            name: "test".into(),
            max_players: 1,
            spawn_points: vec![[0.0, 0.0, 0.0]],
            exit_points: vec![],
            geometry: vec![GeometryDef {
                shape: ShapeDef::Heightfield {
                    nrows,
                    ncols,
                    scale_x,
                    scale_y,
                    scale_z,
                    heights,
                },
                position,
            }],
            interactables: vec![],
            collision_policy: Default::default(),
            terrain_set: None,
        }
    }

    #[test]
    fn sample_flat_heightfield() {
        // 3×3 flat field at raw height 2.0, scale_y 1.0 → surface Y=2.0.
        // 10×10 footprint centred at world (0, 0, 0).
        let t = template_with_heightfield(3, 3, 10.0, 1.0, 10.0, vec![2.0; 9], [0.0, 0.0, 0.0]);
        let y = sample_terrain_y(&t, 0.0, 0.0).expect("centre should hit");
        assert!((y - 2.0).abs() < 1e-4, "expected 2.0, got {y}");
    }

    #[test]
    fn sample_out_of_bounds_returns_none() {
        let t = template_with_heightfield(3, 3, 10.0, 1.0, 10.0, vec![2.0; 9], [0.0, 0.0, 0.0]);
        assert!(sample_terrain_y(&t, 50.0, 0.0).is_none());
        assert!(sample_terrain_y(&t, 0.0, -50.0).is_none());
    }

    #[test]
    fn sample_bilinear_interpolates() {
        // 2×2: heights[0]=0, heights[1]=4 (row 1, col 0), heights[2]=0 (row 0, col 1), heights[3]=0
        // Layout (parry column-major: heights[i + j*nrows]):
        //   (i=0,j=0)=0   (i=0,j=1)=0
        //   (i=1,j=0)=4   (i=1,j=1)=0
        // Sampling at the midpoint between (0,0) and (1,0): expect 2.0.
        let t = template_with_heightfield(
            2,
            2,
            4.0,
            1.0,
            4.0,
            vec![0.0, 4.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
        );
        // Midpoint on the i axis, at j=0 side: local_x = 0 (between -2..+2),
        // local_z = -2 (col 0 edge) → but u=0.5, v=0. Height at i=0.5, j=0 is
        // lerp(heights[0], heights[1], 0.5) = 2.0.
        let y = sample_terrain_y(&t, 0.0, -2.0).expect("on-grid midpoint");
        assert!((y - 2.0).abs() < 1e-3, "expected ~2.0, got {y}");
    }

    #[test]
    fn sample_applies_geo_position_offset() {
        let t = template_with_heightfield(3, 3, 10.0, 1.0, 10.0, vec![2.0; 9], [100.0, 5.0, -50.0]);
        let y = sample_terrain_y(&t, 100.0, -50.0).expect("centred on placed geo");
        assert!(
            (y - 7.0).abs() < 1e-4,
            "5.0 (geo y) + 2.0 (surface) = 7.0, got {y}"
        );
    }
}
