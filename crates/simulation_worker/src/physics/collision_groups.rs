use game_core::collision_layers::{CollisionLayer, CollisionMasks};
use rapier3d::prelude::*;

/// Convert our game collision layer + mask into Rapier's InteractionGroups.
///
/// Rapier uses InteractionGroups(membership, filter) where:
/// - membership: which groups this collider belongs to
/// - filter: which groups this collider interacts with
///
/// Two colliders A and B interact if and only if:
///   (A.membership & B.filter) != 0 && (B.membership & A.filter) != 0
pub fn interaction_groups(membership: CollisionLayer, filter: u32) -> InteractionGroups {
    InteractionGroups::new(
        Group::from_bits_retain(membership.0),
        Group::from_bits_retain(filter),
        InteractionTestMode::And,
    )
}

/// Pre-built interaction groups for common entity types.
pub fn player_body_groups() -> InteractionGroups {
    interaction_groups(CollisionLayer::PLAYER_BODY, CollisionMasks::PLAYER_BODY_FILTER)
}

pub fn npc_body_groups() -> InteractionGroups {
    interaction_groups(CollisionLayer::NPC_BODY, CollisionMasks::NPC_BODY_FILTER)
}

pub fn projectile_groups() -> InteractionGroups {
    interaction_groups(CollisionLayer::PROJECTILE, CollisionMasks::PROJECTILE_FILTER)
}

pub fn skill_hitbox_groups() -> InteractionGroups {
    interaction_groups(CollisionLayer::SKILL_HITBOX, CollisionMasks::SKILL_HITBOX_FILTER)
}

pub fn skill_hurtbox_groups() -> InteractionGroups {
    interaction_groups(CollisionLayer::SKILL_HURTBOX, CollisionMasks::SKILL_HURTBOX_FILTER)
}

pub fn environment_groups() -> InteractionGroups {
    interaction_groups(CollisionLayer::ENVIRONMENT, CollisionMasks::ENVIRONMENT_FILTER)
}

pub fn trigger_groups() -> InteractionGroups {
    interaction_groups(CollisionLayer::TRIGGER, CollisionMasks::TRIGGER_FILTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_collides_with_environment() {
        let player = player_body_groups();
        let env = environment_groups();

        // Both directions must pass
        let a_hits_b = (player.memberships & env.filter).bits() != 0;
        let b_hits_a = (env.memberships & player.filter).bits() != 0;
        assert!(a_hits_b && b_hits_a, "Player should collide with environment");
    }

    #[test]
    fn hitbox_does_not_collide_with_environment() {
        let hitbox = skill_hitbox_groups();
        let env = environment_groups();

        let a_hits_b = (hitbox.memberships & env.filter).bits() != 0;
        let b_hits_a = (env.memberships & hitbox.filter).bits() != 0;
        assert!(!(a_hits_b && b_hits_a), "Hitbox should not collide with environment");
    }

    #[test]
    fn hitbox_collides_with_hurtbox() {
        let hitbox = skill_hitbox_groups();
        let hurtbox = skill_hurtbox_groups();

        let a_hits_b = (hitbox.memberships & hurtbox.filter).bits() != 0;
        let b_hits_a = (hurtbox.memberships & hitbox.filter).bits() != 0;
        assert!(a_hits_b && b_hits_a, "Hitbox should interact with hurtbox");
    }

    #[test]
    fn projectile_collides_with_players_and_npcs() {
        let proj = projectile_groups();
        let player = player_body_groups();
        let npc = npc_body_groups();

        let proj_player = (proj.memberships & player.filter).bits() != 0
            && (player.memberships & proj.filter).bits() != 0;
        let proj_npc = (proj.memberships & npc.filter).bits() != 0
            && (npc.memberships & proj.filter).bits() != 0;

        assert!(proj_player, "Projectile should hit players");
        assert!(proj_npc, "Projectile should hit NPCs");
    }
}
