use std::any::Any;

use game_protocol::entity_id::EntityId;
use game_protocol::types::{Quatf, Transform, Vec3f};
use game_schema::EntityKind;

/// What role a collider plays on an entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColliderKind {
    /// Primary physics body collider (movement, blocking).
    Body,
    /// Vulnerable region — receives damage.
    Hurtbox,
    /// Active attack region — deals damage. Carries an `AbilityExecutionId.0` (opaque u64).
    Hitbox(u64),
    /// Directional block region. Carries a block id.
    BlockCone(u32),
}

/// Abstract shape for a sensor collider (hitbox, trigger zone).
/// Maps to a physics engine shape without exposing engine-specific types.
#[derive(Clone, Copy, Debug)]
pub enum SensorShape {
    Sphere { radius: f32 },
    Capsule { half_height: f32, radius: f32 },
}

/// Engine-agnostic collision event — produced by `drain_collision_events`.
///
/// The physics backend maps its internal collision representation
/// (e.g. Rapier `CollisionEvent`) into this type.
#[derive(Clone, Debug)]
pub struct CollisionEvent {
    pub entity1: EntityId,
    pub entity2: EntityId,
    /// What kind of collider entity1 had in this contact.
    pub kind1: ColliderKind,
    /// What kind of collider entity2 had in this contact.
    pub kind2: ColliderKind,
    /// True if this is a new contact, false if contact ended.
    pub started: bool,
    /// True if one of the colliders is a sensor (hitbox, trigger zone).
    pub is_sensor: bool,
}

/// Abstraction over Rapier configuration so production and debug builds
/// differ only in the physics backend.
///
/// Per spec (rapier_feature_flags.md):
/// - Production: RapierSimdBackend (SIMD + parallel)
/// - Debug: RapierDeterministicBackend (enhanced-determinism, no SIMD/parallel)
pub trait PhysicsBackend: Send {
    /// Upcast to `Any` for safe downcasting in tests and tooling.
    fn as_any(&self) -> &dyn Any;
    /// Upcast to `Any` (mutable) for safe downcasting.
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Advance the simulation by one fixed timestep.
    fn step(&mut self, dt: f32);

    /// Get the authoritative transform for an entity.
    fn get_transform(&self, entity_id: EntityId) -> Option<Transform>;

    /// Get all active entity transforms (for snapshot generation).
    fn get_all_transforms(&self) -> Vec<(EntityId, Transform)>;

    /// Drain collision events that occurred during the last step.
    ///
    /// Each call returns all pending events and clears the internal queue.
    /// Events include both contact-started and contact-ended notifications,
    /// and flag whether the collision involved a sensor collider.
    fn drain_collision_events(&mut self) -> Vec<CollisionEvent>;

    /// Remove an entity's physics body and all its colliders.
    /// Returns true if the entity existed.
    fn remove_entity(&mut self, entity_id: EntityId) -> bool;

    /// Set the next kinematic position for a position-based kinematic body.
    fn set_kinematic_position(&mut self, entity_id: EntityId, position: Vec3f) -> bool;

    /// Set the next kinematic rotation for a position-based kinematic body.
    ///
    /// `rotation` is a unit quaternion representing the desired orientation.
    /// The body's current translation is preserved; only rotation changes.
    /// Returns true if the entity exists and the body was updated, false otherwise.
    fn set_kinematic_rotation(&mut self, entity_id: EntityId, rotation: Quatf) -> bool;

    /// Set the linear velocity of a dynamic body.
    fn set_linear_velocity(&mut self, entity_id: EntityId, velocity: Vec3f) -> bool;

    /// Attach a sensor collider to an existing entity body.
    ///
    /// `offset` is in entity-local space — Vec3f::ZERO centres the sensor on the body origin.
    /// Returns an opaque handle for later removal. Returns None if the entity has no body.
    fn spawn_sensor(&mut self, entity_id: EntityId, shape: SensorShape, offset: Vec3f, kind: ColliderKind) -> Option<u64>;

    /// Spawn a sensor collider at a fixed world position, not attached to any entity body.
    ///
    /// Used for projectiles and ground-targeted abilities whose collision region moves
    /// independently of any entity. `owner` is the caster entity for damage attribution
    /// in collision events. Returns an opaque handle for positioning and removal.
    fn spawn_world_sensor(&mut self, position: Vec3f, shape: SensorShape, kind: ColliderKind, owner: EntityId) -> u64;

    /// Update the world-space position of a sensor created by `spawn_world_sensor`.
    /// No-op and returns false if the handle is unknown.
    fn set_sensor_position(&mut self, handle: u64, position: Vec3f) -> bool;

    /// Return the entities currently intersecting the given sensor collider.
    ///
    /// Used by continuous hitboxes (auras, hazard zones) so damage can be based
    /// on the authoritative overlap state instead of only Started/Stopped events.
    fn sensor_intersections(&self, _handle: u64) -> Vec<EntityId> {
        Vec::new()
    }

    /// Remove a sensor collider previously created by spawn_sensor. No-op if handle unknown.
    fn remove_sensor(&mut self, handle: u64);

    /// Spawn a standard kinematic character body (capsule + hurtbox sensor) at the given position.
    ///
    /// The `kind` selects the correct physics collision layer:
    /// - `Player` → PLAYER_BODY layer
    /// - `Npc` | `Boss` → NPC_BODY layer (bug #14: was previously always PLAYER_BODY)
    ///
    /// Used by the coordinator to create physics bodies for entities arriving via DB subscription.
    /// Returns true if the body was created; false if the entity already has a body.
    fn spawn_character_body(&mut self, entity_id: EntityId, position: Vec3f, kind: EntityKind) -> bool;

    /// Spawn a dynamic box body for a prop entity.
    ///
    /// Creates a cuboid rigid body with the given half-extents that responds to
    /// physics forces (gravity, collisions). Players and NPCs push it around via
    /// kinematic contacts. Returns true if the body was created, false if the
    /// entity already has a body.
    fn spawn_prop_body(&mut self, entity_id: EntityId, position: Vec3f, half_extents: Vec3f, pushable: bool) -> bool;

    /// Move a kinematic character body by `desired_translation`, sliding along obstacles.
    ///
    /// Uses a character controller to resolve collisions against static geometry
    /// (walls, floors, obstacles).  Returns the corrected world-space position
    /// and whether the character is touching the ground after the move.
    fn move_character(&mut self, entity_id: EntityId, desired_translation: Vec3f) -> Option<MoveResult>;

    /// Cast a targeting ray from `origin` along `direction` up to `max_distance`.
    ///
    /// Returns the first hit (closest by time-of-impact) among hurtboxes and
    /// blocking world geometry. Character body colliders and non-blocking
    /// gameplay sensors are ignored so target acquisition cannot whiff on the
    /// physical capsule in front of an entity's hurtbox. If `ignore_entity` is
    /// provided, that entity's own rigid body and attached colliders are also
    /// excluded from the query.
    fn raycast(&self, origin: Vec3f, direction: Vec3f, max_distance: f32, ignore_entity: Option<EntityId>) -> Option<RayHit>;

    /// Check line-of-sight between two world positions against environment geometry only.
    ///
    /// Returns `true` if the straight-line path is unobstructed (ray hits nothing).
    /// Ignores character bodies, hurtboxes, and sensors — only static world
    /// geometry (ENVIRONMENT + PROP_BODY + FLIGHT_BLOCKER) can block LoS.
    /// Used for lock-on tagging validation and ground-target placement.
    fn line_of_sight(&self, from: Vec3f, to: Vec3f) -> bool;

    /// Cast from `from` toward `to` through environment-only geometry and return
    /// the safe teleport destination. If no wall is hit, returns `to` unchanged.
    /// If a wall is hit, returns the contact point pulled back 0.3 units toward `from`
    /// so the traveller stops flush against the wall without overlapping it.
    /// Used by `TeleportForward` and `TeleportBehindTarget` to prevent going through walls.
    fn cast_to_wall(&self, from: Vec3f, to: Vec3f) -> Vec3f;

    /// Teleport an entity's physics body to `position` immediately.
    ///
    /// Moves the kinematic body in world space (no contact resolution — teleports
    /// through walls). Used by `TeleportBehindTarget` and `TeleportForward` abilities.
    /// No-op and returns `false` if the entity has no physics body.
    fn teleport_entity(&mut self, entity_id: EntityId, position: Vec3f) -> bool;
}

/// Result of a `move_character` call.
#[derive(Clone, Copy, Debug)]
pub struct MoveResult {
    /// Corrected world-space position after contact resolution.
    pub position: Vec3f,
    /// True if the character is touching the ground after the move.
    pub grounded: bool,
}

/// Result of a `raycast` call.
#[derive(Clone, Copy, Debug)]
pub struct RayHit {
    /// World-space position of the hit point: `origin + direction * toi`.
    pub point: Vec3f,
    /// Surface normal at the hit point.
    pub normal: Vec3f,
    /// Time-of-impact (distance along the ray direction).
    pub toi: f32,
    /// Entity that owns the hit collider.
    pub entity: EntityId,
    /// What kind of collider was hit.
    pub kind: ColliderKind,
}
