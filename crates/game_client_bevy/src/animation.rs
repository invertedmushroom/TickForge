use bevy::prelude::*;

use crate::sync::{AnimatedVisual, PresentationMotion, VisualOwner};

pub struct AnimationPlugin;

#[derive(SystemSet, Debug, Hash, PartialEq, Eq, Clone)]
pub enum AnimationSet {
    AnimatePresentation,
}

impl Plugin for AnimationPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            animate_character_visuals.in_set(AnimationSet::AnimatePresentation),
        );
    }
}

const MAX_FORWARD_TILT: f32 = 0.08;
const MAX_BANK: f32 = 0.10;
const MAX_TURN_RATE: f32 = 7.5;
const BOB_HEIGHT: f32 = 0.018;
const BOB_FREQ: f32 = 5.5;
const MOTION_EPSILON: f32 = 0.05;

fn animate_character_visuals(
    time: Res<Time>,
    root_q: Query<(&PresentationMotion, &Transform), Without<AnimatedVisual>>,
    mut visual_q: Query<(&VisualOwner, &mut Transform), With<AnimatedVisual>>,
) {
    let elapsed = time.elapsed_secs();

    for (owner, mut visual_tf) in visual_q.iter_mut() {
        let Ok((motion, root_tf)) = root_q.get(owner.0) else {
            continue;
        };

        let local_velocity = root_tf.rotation.inverse() * motion.velocity;
        let speed_ratio = (motion.planar_speed / 5.0).clamp(0.0, 1.0);
        let forward_tilt = (-local_velocity.z / 5.0).clamp(-1.0, 1.0) * MAX_FORWARD_TILT;
        let turn_bank = (motion.turn_rate / MAX_TURN_RATE).clamp(-1.0, 1.0) * 0.04;
        let strafe_bank = (-local_velocity.x / 5.0).clamp(-1.0, 1.0) * MAX_BANK;
        let bob = if motion.planar_speed > MOTION_EPSILON {
            (elapsed * (BOB_FREQ + speed_ratio * 2.0)).sin() * BOB_HEIGHT * speed_ratio
        } else {
            0.0
        };

        visual_tf.translation = Vec3::new(0.0, bob, 0.0);
        visual_tf.rotation =
            Quat::from_euler(EulerRot::XYZ, forward_tilt, 0.0, strafe_bank + turn_bank);
    }
}
