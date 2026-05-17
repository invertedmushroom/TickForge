use bevy::input::mouse::{MouseMotion, MouseWheel};
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, PrimaryWindow};

pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<OrbitState>();
        app.init_resource::<CursorCaptured>();
        app.add_systems(Startup, (spawn_camera, capture_cursor));
        #[cfg(feature = "connected")]
        app.add_systems(
            Update,
            (
                toggle_cursor_capture,
                orbit_input,
                follow_player.after(crate::sync::SyncSet::ApplyPresentation),
            )
                .chain(),
        );
        #[cfg(not(feature = "connected"))]
        app.add_systems(
            Update,
            (toggle_cursor_capture, orbit_input, follow_player).chain(),
        );
    }
}

/// Tag component for the main game camera.
#[derive(Component)]
pub struct GameCamera;

/// Tag for the local player entity in the Bevy world.
#[derive(Component)]
pub struct LocalPlayer;

/// Whether the mouse cursor is captured for camera look (TERA-style).
/// When captured, mouse motion drives camera yaw/pitch and the cursor is hidden.
/// Left Alt toggles this off to free the cursor for UI interaction.
#[derive(Resource)]
pub struct CursorCaptured(pub bool);

impl Default for CursorCaptured {
    fn default() -> Self {
        Self(true)
    }
}

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
            pitch: -0.5, // ~30° down
            distance: 25.0,
        }
    }
}

const CAMERA_ORBIT_HEIGHT: f32 = 1.25;
const CAMERA_LOOK_AT_HEIGHT: f32 = 3.5;
const CAMERA_FOLLOW_LERP_SPEED: f32 = 5.0;

fn spawn_camera(mut commands: Commands) {
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 15.0, 20.0).looking_at(Vec3::ZERO, Vec3::Y),
        GameCamera,
    ));
}

/// Capture cursor on startup so TERA-style mouse-look is active immediately.
fn capture_cursor(mut windows: Query<&mut Window, With<PrimaryWindow>>) {
    if let Ok(mut window) = windows.get_single_mut() {
        window.cursor_options.grab_mode = CursorGrabMode::Locked;
        window.cursor_options.visible = false;
    }
}

/// Left Alt toggles cursor capture on/off.
fn toggle_cursor_capture(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut captured: ResMut<CursorCaptured>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
) {
    if keyboard.just_pressed(KeyCode::AltLeft) {
        captured.0 = !captured.0;
        if let Ok(mut window) = windows.get_single_mut() {
            if captured.0 {
                window.cursor_options.grab_mode = CursorGrabMode::Locked;
                window.cursor_options.visible = false;
            } else {
                window.cursor_options.grab_mode = CursorGrabMode::None;
                window.cursor_options.visible = true;
            }
        }
    }
}

/// TERA-style mouse-look: mouse always drives orbit yaw/pitch when cursor
/// is captured. Scroll wheel for zoom. Right-mouse-drag still works when
/// cursor is free (for UI-mode camera orbit).
fn orbit_input(
    mouse_button: Res<ButtonInput<MouseButton>>,
    mut motion_events: EventReader<MouseMotion>,
    mut scroll_events: EventReader<MouseWheel>,
    mut orbit: ResMut<OrbitState>,
    captured: Res<CursorCaptured>,
    menu_state: Option<Res<crate::ability_bar::SkillMenuState>>,
) {
    // Mouse-look when cursor is captured, OR right-mouse-drag when free.
    if captured.0 || mouse_button.pressed(MouseButton::Right) {
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
    let menu_open = menu_state
        .as_ref()
        .is_some_and(|state| state.active_slot.is_some());
    if !menu_open {
        for ev in scroll_events.read() {
            orbit.distance -= ev.y * 2.0;
        }
    } else {
        scroll_events.clear();
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
    let Ok(player_tf) = player_q.get_single() else {
        return;
    };
    let Ok(mut cam_tf) = cam_q.get_single_mut() else {
        return;
    };

    let orbit_origin = player_tf.translation + Vec3::Y * CAMERA_ORBIT_HEIGHT;
    let focus_point = player_tf.translation + Vec3::Y * CAMERA_LOOK_AT_HEIGHT;

    // Compute spherical offset from orbit state.
    let offset = Vec3::new(
        orbit.distance * orbit.pitch.cos() * orbit.yaw.sin(),
        orbit.distance * -orbit.pitch.sin(),
        orbit.distance * orbit.pitch.cos() * orbit.yaw.cos(),
    );

    let target = orbit_origin + offset;
    cam_tf.translation = cam_tf
        .translation
        .lerp(target, CAMERA_FOLLOW_LERP_SPEED * time.delta_secs());
    cam_tf.look_at(focus_point, Vec3::Y);
}
