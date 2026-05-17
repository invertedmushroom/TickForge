mod ability_bar;
mod ability_visuals;
#[cfg(feature = "connected")]
mod admin;
#[cfg(feature = "connected")]
mod animation;
mod camera;
#[cfg(feature = "connected")]
mod combat_log;
mod diagnostics;
#[cfg(feature = "connected")]
mod dungeon_geometry;
mod hud;
#[cfg(feature = "connected")]
mod input;
#[cfg(feature = "connected")]
mod inspector;
#[cfg(feature = "connected")]
mod instance_panel;
#[cfg(feature = "connected")]
mod inventory;
#[cfg(feature = "connected")]
mod spacetime;
#[cfg(feature = "connected")]
mod sync;
#[cfg(feature = "connected")]
mod vfx;

use bevy::pbr::{NotShadowCaster, NotShadowReceiver};
use bevy::prelude::*;
use bevy::render::mesh::VertexAttributeValues;

fn main() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "TickForge".into(),
            resolution: (1280.0, 720.0).into(),
            ..default()
        }),
        ..default()
    }));

    app.add_systems(Startup, setup_scene);
    app.add_plugins(camera::CameraPlugin);
    app.add_plugins(hud::HudPlugin);
    app.add_plugins(diagnostics::DiagnosticsPlugin);
    app.add_plugins(ability_bar::AbilityBarPlugin);

    #[cfg(feature = "connected")]
    {
        app.add_plugins(spacetime::SpacetimePlugin);
        app.add_plugins(sync::SyncPlugin);
        app.add_plugins(input::InputPlugin);
        app.add_plugins(animation::AnimationPlugin);
        app.add_plugins(combat_log::CombatLogPlugin);
        app.add_plugins(vfx::VfxPlugin);
        app.add_plugins(inspector::InspectorPlugin);
        app.add_plugins(admin::AdminPlugin);
        app.add_plugins(inventory::InventoryPlugin);
        app.add_plugins(instance_panel::InstancePanelPlugin);
        app.add_plugins(dungeon_geometry::DungeonGeometryPlugin);
        app.configure_sets(
            Update,
            (
                input::InputSet::DriveInput,
                sync::SyncSet::ApplyPresentation,
                animation::AnimationSet::AnimatePresentation,
            )
                .chain(),
        );
    }

    app.run();
}

/// Set up the ground plane, lighting, and static scene elements.
fn setup_scene(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut clear_color: ResMut<ClearColor>,
) {
    clear_color.0 = Color::srgb(0.80, 0.87, 0.97);

    // Vertex-colored skybox gives us a visible top-to-horizon gradient from every angle.
    commands.spawn((
        Mesh3d(meshes.add(make_skybox_mesh())),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::WHITE,
            emissive: LinearRgba::new(0.02, 0.03, 0.05, 1.0),
            unlit: true,
            cull_mode: None,
            alpha_mode: AlphaMode::Opaque,
            ..default()
        })),
        Transform::from_scale(Vec3::new(-1.0, 1.0, 1.0)),
        NotShadowCaster,
        NotShadowReceiver,
    ));

    // Ground plane
    let ground_mesh = meshes.add(Cuboid::new(100.0, 0.2, 100.0));
    let ground_mat = materials.add(StandardMaterial {
        base_color: Color::srgb(0.24, 0.36, 0.25),
        perceptual_roughness: 1.0,
        reflectance: 0.0,
        metallic: 0.0,
        ..default()
    });
    let mut ground_ent = commands.spawn((
        Mesh3d(ground_mesh),
        MeshMaterial3d(ground_mat),
        Transform::from_xyz(0.0, -0.1, 0.0),
    ));
    #[cfg(feature = "connected")]
    {
        ground_ent.insert(crate::dungeon_geometry::DefaultGround);
    }

    // Directional light (sun)
    commands.spawn((
        DirectionalLight {
            color: Color::srgb(1.0, 0.97, 0.9),
            illuminance: 32_000.0,
            shadows_enabled: true,
            shadow_depth_bias: 0.08,
            shadow_normal_bias: 0.6,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -1.05, 0.78, 0.0)),
    ));

    // Ambient light
    commands.insert_resource(AmbientLight {
        color: Color::srgb(0.77, 0.82, 0.90),
        brightness: 48.0,
    });
}

fn make_skybox_mesh() -> Mesh {
    let mut mesh = Mesh::from(Cuboid::new(600.0, 320.0, 600.0));
    let Some(VertexAttributeValues::Float32x3(positions)) =
        mesh.attribute(Mesh::ATTRIBUTE_POSITION)
    else {
        return mesh;
    };

    let colors: Vec<[f32; 4]> = positions
        .iter()
        .map(|position| {
            let t = ((position[1] / 320.0) + 0.5).clamp(0.0, 1.0);
            let top = Vec3::new(0.22, 0.39, 0.72);
            let horizon = Vec3::new(0.86, 0.90, 0.98);
            let mixed = horizon.lerp(top, t * t);
            [mixed.x, mixed.y, mixed.z, 1.0]
        })
        .collect();
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    mesh
}
