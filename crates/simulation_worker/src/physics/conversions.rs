use game_protocol::types::{Quatf, Transform, Vec3f};
use rapier3d::prelude::*;
use rapier3d::math::{Vector, Rotation};

/// Convert a Rapier RigidBody's state into our protocol Transform.
///
/// rapier3d 0.32 uses glam-based math types:
/// - translation()/linvel()/angvel() return Vector (= glam::Vec3)
/// - rotation() returns &Rotation (= &glam::Quat)
pub fn body_to_transform(body: &RigidBody) -> Transform {
    let pos = body.translation();
    let rot = body.rotation();
    let linvel = body.linvel();
    let angvel = body.angvel();

    Transform {
        position: Vec3f::new(pos.x, pos.y, pos.z),
        rotation: Quatf {
            x: rot.x,
            y: rot.y,
            z: rot.z,
            w: rot.w,
        },
        linear_velocity: Vec3f::new(linvel.x, linvel.y, linvel.z),
        angular_velocity: Vec3f::new(angvel.x, angvel.y, angvel.z),
    }
}

/// Convert our protocol Vec3f to rapier Vector (glam Vec3).
pub fn vec3f_to_vector(v: Vec3f) -> Vector {
    Vector::new(v.x, v.y, v.z)
}

/// Convert rapier Vector (glam Vec3) to our protocol Vec3f.
pub fn vector_to_vec3f(v: Vector) -> Vec3f {
    Vec3f::new(v.x, v.y, v.z)
}

/// Convert our protocol Quatf to rapier Rotation (glam Quat).
pub fn quatf_to_rotation(q: Quatf) -> Rotation {
    Rotation::from_xyzw(q.x, q.y, q.z, q.w)
}

/// Convert rapier Rotation (glam Quat) to our protocol Quatf.
pub fn rotation_to_quatf(q: &Rotation) -> Quatf {
    Quatf {
        x: q.x,
        y: q.y,
        z: q.z,
        w: q.w,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec3f_roundtrip() {
        let original = Vec3f::new(1.0, 2.0, 3.0);
        let v = vec3f_to_vector(original);
        let back = vector_to_vec3f(v);
        assert_eq!(original, back);
    }

    #[test]
    fn quatf_identity_roundtrip() {
        let original = Quatf::IDENTITY;
        let rot = quatf_to_rotation(original);
        let back = rotation_to_quatf(&rot);
        assert!((back.x - original.x).abs() < 1e-6);
        assert!((back.y - original.y).abs() < 1e-6);
        assert!((back.z - original.z).abs() < 1e-6);
        assert!((back.w - original.w).abs() < 1e-6);
    }
}
