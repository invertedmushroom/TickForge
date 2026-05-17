use serde::{Deserialize, Serialize};

// Vec3f is the canonical shared type from game_schema.
pub use game_schema::Vec3f;

/// Quaternion rotation — engine-agnostic representation.
///
/// Stored as (x, y, z, w) to match nalgebra's internal layout.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Quatf {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Quatf {
    pub const IDENTITY: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 0.0,
        w: 1.0,
    };
}

/// Authoritative transform — position + rotation + velocity.
///
/// This matches the "hot state" that gets replicated to clients.
/// Angular velocity included for physics interpolation on clients.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub position: Vec3f,
    pub rotation: Quatf,
    pub linear_velocity: Vec3f,
    pub angular_velocity: Vec3f,
}

impl Transform {
    pub fn at_position(x: f32, y: f32, z: f32) -> Self {
        Self {
            position: Vec3f::new(x, y, z),
            rotation: Quatf::IDENTITY,
            linear_velocity: Vec3f::ZERO,
            angular_velocity: Vec3f::ZERO,
        }
    }
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            position: Vec3f::ZERO,
            rotation: Quatf::IDENTITY,
            linear_velocity: Vec3f::ZERO,
            angular_velocity: Vec3f::ZERO,
        }
    }
}
