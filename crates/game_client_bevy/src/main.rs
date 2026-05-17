#[cfg(feature = "connected")]
mod spacetime;
#[cfg(feature = "connected")]
mod sync;
#[cfg(feature = "connected")]
mod input;
#[cfg(feature = "connected")]
mod combat_log;
#[cfg(feature = "connected")]
mod vfx;
#[cfg(feature = "connected")]
mod inspector;
#[cfg(feature = "connected")]
mod admin;
#[cfg(feature = "connected")]
mod inventory;
mod ability_bar;
mod ability_visuals;
mod camera;
mod diagnostics;
mod hud;

use bevy::prelude::*;

fn main() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Jump".into(),
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
        app.add_plugins(combat_log::CombatLogPlugin);
        app.add_plugins(vfx::VfxPlugin);
        app.add_plugins(inspector::InspectorPlugin);
        app.add_plugins(admin::AdminPlugin);
        app.add_plugins(inventory::InventoryPlugin);
    }

    app.run();
}

/// Set up the ground plane, lighting, and static scene elements.
fn setup_scene(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // Ground plane
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::new(Vec3::Y, Vec2::splat(100.0)))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.3, 0.5, 0.3),
            ..default()
        })),
        Transform::from_xyz(0.0, 0.0, 0.0),
    ));

    // Directional light (sun)
    commands.spawn((
        DirectionalLight {
            illuminance: 10_000.0,
            shadows_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.8, 0.4, 0.0)),
    ));

    // Ambient light
    commands.insert_resource(AmbientLight {
        color: Color::WHITE,
        brightness: 300.0,
    });
}
