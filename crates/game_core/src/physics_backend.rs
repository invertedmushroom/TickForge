use game_protocol::entity_id::EntityId;
use game_protocol::types::Transform;

/// Abstraction over Rapier configuration so production and debug builds
/// differ only in the physics backend.
///
/// Per spec (rapier_feature_flags.md):
/// - Production: RapierSimdBackend (SIMD + parallel)
/// - Debug: RapierDeterministicBackend (enhanced-determinism, no SIMD/parallel)
pub trait PhysicsBackend: Send {
    /// Advance the simulation by one fixed timestep.
    fn step(&mut self, dt: f32);

    /// Get the authoritative transform for an entity.
    fn get_transform(&self, entity_id: EntityId) -> Option<Transform>;

    /// Get all active entity transforms (for snapshot generation).
    fn get_all_transforms(&self) -> Vec<(EntityId, Transform)>;
}
