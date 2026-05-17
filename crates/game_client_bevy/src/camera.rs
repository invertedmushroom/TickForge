use bevy::prelude::*;
use bevy::input::mouse::{MouseMotion, MouseWheel};

pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<OrbitState>();
        app.add_systems(Startup, spawn_camera);
        app.add_systems(Update, (orbit_input, follow_player).chain());
    }
}

/// Tag component for the main game camera.
#[derive(Component)]
pub struct GameCamera;

/// Tag for the local player entity in the Bevy world.
#[derive(Component)]
pub struct LocalPlayer;

/// Orbit camera state: yaw, pitch, distance.
#[derive(Resource)]
pub struct OrbitState {
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
}

impl Default for OrbitState {
    fn default() -> Self {
        Self {
            yaw: 0.0,
            pitch: -0.5,      // ~30° down
            distance: 25.0,
        }
    }
}

fn spawn_camera(mut commands: Commands) {
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 15.0, 20.0).looking_at(Vec3::ZERO, Vec3::Y),
        GameCamera,
    ));
}

/// Handle right-mouse-drag orbit + scroll zoom.
fn orbit_input(
    mouse_button: Res<ButtonInput<MouseButton>>,
    mut motion_events: EventReader<MouseMotion>,
    mut scroll_events: EventReader<MouseWheel>,
    mut orbit: ResMut<OrbitState>,
) {
    // Orbit on right-mouse drag.
    if mouse_button.pressed(MouseButton::Right) {
        for ev in motion_events.read() {
            orbit.yaw -= ev.delta.x * 0.005;
            orbit.pitch -= ev.delta.y * 0.005;
        }
    } else {
        motion_events.clear();
    }

    // Clamp pitch to avoid flipping.
    orbit.pitch = orbit.pitch.clamp(-1.4, -0.1);

    // Scroll zoom.
    for ev in scroll_events.read() {
        orbit.distance -= ev.y * 2.0;
    }
    orbit.distance = orbit.distance.clamp(5.0, 60.0);
}

/// Smoothly follow the local player entity with orbit offset.
fn follow_player(
    player_q: Query<&Transform, (With<LocalPlayer>, Without<GameCamera>)>,
    mut cam_q: Query<&mut Transform, With<GameCamera>>,
    orbit: Res<OrbitState>,
    time: Res<Time>,
) {
    let Ok(player_tf) = player_q.get_single() else { return };
    let Ok(mut cam_tf) = cam_q.get_single_mut() else { return };

    // Compute spherical offset from orbit state.
    let offset = Vec3::new(
        orbit.distance * orbit.pitch.cos() * orbit.yaw.sin(),
        orbit.distance * -orbit.pitch.sin(),
        orbit.distance * orbit.pitch.cos() * orbit.yaw.cos(),
    );

    let target = player_tf.translation + offset;
    let speed = 5.0;
    cam_tf.translation = cam_tf.translation.lerp(target, speed * time.delta_secs());
    cam_tf.look_at(player_tf.translation, Vec3::Y);
}
