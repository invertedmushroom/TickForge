use bevy::prelude::*;

pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_camera);
        app.add_systems(Update, follow_player);
    }
}

/// Tag component for the main game camera.
#[derive(Component)]
pub struct GameCamera;

/// Tag for the local player entity in the Bevy world.
#[derive(Component)]
pub struct LocalPlayer;

fn spawn_camera(mut commands: Commands) {
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 15.0, 20.0).looking_at(Vec3::ZERO, Vec3::Y),
        GameCamera,
    ));
}

/// Smoothly follow the local player entity.
fn follow_player(
    player_q: Query<&Transform, (With<LocalPlayer>, Without<GameCamera>)>,
    mut cam_q: Query<&mut Transform, With<GameCamera>>,
    time: Res<Time>,
) {
    let Ok(player_tf) = player_q.get_single() else { return };
    let Ok(mut cam_tf) = cam_q.get_single_mut() else { return };

    let offset = Vec3::new(0.0, 15.0, 20.0);
    let target = player_tf.translation + offset;
    let speed = 5.0;
    cam_tf.translation = cam_tf.translation.lerp(target, speed * time.delta_secs());
    cam_tf.look_at(player_tf.translation, Vec3::Y);
}
