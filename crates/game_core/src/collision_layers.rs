/// Collision layer bit flags for Rapier collision groups.
///
/// Defined early per architecture docs recommendation:
/// "Define layers explicitly early. Document the collision matrix."
///
/// These map directly to Rapier's `Group` bits (u32).
/// The simulation_worker converts these to Rapier's `InteractionGroups`.
///
/// # Collision matrix
///
/// ```text
///                 Player  NPC  Projectile  Hitbox  Hurtbox  Env  Trigger  Flight  Prop
/// Player body       -      X      X          X       -      X      X       X       X
/// NPC body          X      -      X          X       -      X      -       X       X
/// Projectile        X      X      -          -       -      X      -       -       X
/// Skill hitbox      X      X      -          -       X      -      -       -       -
/// Skill hurtbox     -      -      -          X       -      -      -       -       -
/// Environment       X      X      X          -       -      -      -       -       X
/// Trigger           X      -      -          -       -      -      -       -       -
/// Flight blocker    X      X      -          -       -      -      -       -       -
/// Prop body         X      X      X          -       -      X      -       -       X
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollisionLayer(pub u32);

impl CollisionLayer {
    pub const NONE: Self = Self(0);
    pub const PLAYER_BODY: Self = Self(1 << 0);
    pub const NPC_BODY: Self = Self(1 << 1);
    pub const PROJECTILE: Self = Self(1 << 2);
    pub const SKILL_HITBOX: Self = Self(1 << 3);
    pub const SKILL_HURTBOX: Self = Self(1 << 4);
    pub const ENVIRONMENT: Self = Self(1 << 5);
    pub const TRIGGER: Self = Self(1 << 6);
    pub const FLIGHT_BLOCKER: Self = Self(1 << 7);
    pub const PROP_BODY: Self = Self(1 << 8);

    /// Combine multiple layers into a single mask.
    pub const fn combine(layers: &[CollisionLayer]) -> u32 {
        let mut result = 0u32;
        let mut i = 0;
        while i < layers.len() {
            result |= layers[i].0;
            i += 1;
        }
        result
    }
}

/// Pre-computed collision masks — what each layer collides WITH.
///
/// Usage: `InteractionGroups::new(membership, filter)`
/// where membership = which layer this body IS,
/// and filter = which layers this body collides WITH.
pub struct CollisionMasks;

impl CollisionMasks {
    pub const PLAYER_BODY_FILTER: u32 = CollisionLayer::combine(&[
        CollisionLayer::NPC_BODY,
        CollisionLayer::PROJECTILE,
        CollisionLayer::SKILL_HITBOX,
        CollisionLayer::ENVIRONMENT,
        CollisionLayer::TRIGGER,
        CollisionLayer::FLIGHT_BLOCKER,
        CollisionLayer::PROP_BODY,
    ]);

    pub const NPC_BODY_FILTER: u32 = CollisionLayer::combine(&[
        CollisionLayer::PLAYER_BODY,
        CollisionLayer::PROJECTILE,
        CollisionLayer::SKILL_HITBOX,
        CollisionLayer::ENVIRONMENT,
        CollisionLayer::FLIGHT_BLOCKER,
        CollisionLayer::PROP_BODY,
    ]);

    pub const PROJECTILE_FILTER: u32 = CollisionLayer::combine(&[
        CollisionLayer::PLAYER_BODY,
        CollisionLayer::NPC_BODY,
        CollisionLayer::ENVIRONMENT,
        CollisionLayer::PROP_BODY,
    ]);

    pub const SKILL_HITBOX_FILTER: u32 = CollisionLayer::combine(&[
        CollisionLayer::PLAYER_BODY,
        CollisionLayer::NPC_BODY,
        CollisionLayer::SKILL_HURTBOX,
    ]);

    pub const SKILL_HURTBOX_FILTER: u32 = CollisionLayer::combine(&[
        CollisionLayer::SKILL_HITBOX,
    ]);

    pub const ENVIRONMENT_FILTER: u32 = CollisionLayer::combine(&[
        CollisionLayer::PLAYER_BODY,
        CollisionLayer::NPC_BODY,
        CollisionLayer::PROJECTILE,
        CollisionLayer::PROP_BODY,
    ]);

    pub const TRIGGER_FILTER: u32 = CollisionLayer::combine(&[
        CollisionLayer::PLAYER_BODY,
    ]);

    pub const FLIGHT_BLOCKER_FILTER: u32 = CollisionLayer::combine(&[
        CollisionLayer::PLAYER_BODY,
        CollisionLayer::NPC_BODY,
    ]);

    pub const PROP_BODY_FILTER: u32 = CollisionLayer::combine(&[
        CollisionLayer::PLAYER_BODY,
        CollisionLayer::NPC_BODY,
        CollisionLayer::PROJECTILE,
        CollisionLayer::ENVIRONMENT,
        CollisionLayer::PROP_BODY,
    ]);

    /// KCC movement filter: character controllers only collide with static/prop
    /// geometry during movement, preventing capsule stacking and landing-on-heads.
    /// Membership is PLAYER_BODY | NPC_BODY so environment/prop filters still match.
    pub const KCC_MOVEMENT_MEMBERSHIP: u32 = CollisionLayer::combine(&[
        CollisionLayer::PLAYER_BODY,
        CollisionLayer::NPC_BODY,
    ]);

    pub const KCC_MOVEMENT_FILTER: u32 = CollisionLayer::combine(&[
        CollisionLayer::ENVIRONMENT,
        CollisionLayer::PROP_BODY,
        CollisionLayer::FLIGHT_BLOCKER,
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collision_layers_are_unique_bits() {
        let layers = [
            CollisionLayer::PLAYER_BODY,
            CollisionLayer::NPC_BODY,
            CollisionLayer::PROJECTILE,
            CollisionLayer::SKILL_HITBOX,
            CollisionLayer::SKILL_HURTBOX,
            CollisionLayer::ENVIRONMENT,
            CollisionLayer::TRIGGER,
            CollisionLayer::FLIGHT_BLOCKER,
            CollisionLayer::PROP_BODY,
        ];

        // Each layer should be a single bit
        for layer in &layers {
            assert!(layer.0.is_power_of_two(), "Layer {:#010b} is not a single bit", layer.0);
        }

        // No two layers share a bit
        let mut combined = 0u32;
        for layer in &layers {
            assert_eq!(combined & layer.0, 0, "Layer {:#010b} overlaps", layer.0);
            combined |= layer.0;
        }
    }

    #[test]
    fn collision_matrix_is_symmetric() {
        // If A collides with B, B must collide with A.
        // Player vs NPC
        assert_ne!(CollisionMasks::PLAYER_BODY_FILTER & CollisionLayer::NPC_BODY.0, 0);
        assert_ne!(CollisionMasks::NPC_BODY_FILTER & CollisionLayer::PLAYER_BODY.0, 0);

        // Player vs Projectile
        assert_ne!(CollisionMasks::PLAYER_BODY_FILTER & CollisionLayer::PROJECTILE.0, 0);
        assert_ne!(CollisionMasks::PROJECTILE_FILTER & CollisionLayer::PLAYER_BODY.0, 0);

        // Player vs Environment
        assert_ne!(CollisionMasks::PLAYER_BODY_FILTER & CollisionLayer::ENVIRONMENT.0, 0);
        assert_ne!(CollisionMasks::ENVIRONMENT_FILTER & CollisionLayer::PLAYER_BODY.0, 0);
    }

    #[test]
    fn hitbox_does_not_collide_with_environment() {
        assert_eq!(CollisionMasks::SKILL_HITBOX_FILTER & CollisionLayer::ENVIRONMENT.0, 0);
    }
}
