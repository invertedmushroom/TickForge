use std::any::Any;

use game_protocol::entity_id::EntityId;
use game_protocol::types::{Quatf, Transform, Vec3f};
use game_schema::EntityKind;
use game_schema::LayerCollisionPolicy;

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
    /// Generic gameplay volume — trigger zone, puzzle pad, water, arena,
    /// cleanse pool. Carries the `VolumeId.0` so the worker can route
    /// occupant updates back to the [`crate::volume::VolumeStore`]. Volumes
    /// never deal damage; the volume system reads `sensor_intersections`
    /// directly each tick instead of relying on contact events.
    Volume(u64),
}

/// Abstract shape for a sensor collider (hitbox, trigger zone).
/// Maps to a physics engine shape without exposing engine-specific types.
#[derive(Clone, Copy, Debug)]
pub enum SensorShape {
    Sphere { radius: f32 },
    Capsule { half_height: f32, radius: f32 },
}

/// Catalog of authoritative body shapes used by spawned entities.
///
/// Each variant pins the exact half-extents / radius used by the physics
/// backend, the lag-comp hurtbox, and the client mesh so they cannot drift.
///
/// Capsule variants are oriented along the Y axis. `half_height` is the
/// half-distance between the two hemisphere centers; total capsule height
/// is `2 * (half_height + radius)`.
///
/// Cuboid half-extents are stored as `(half_x, half_y, half_z)`. The
/// `pushable` flag controls whether the prop body is dynamic+CCD or fixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BodyShape {
    /// Standard player capsule — half_height 0.5, radius 0.3.
    PlayerCapsule,
    /// Standard NPC capsule — same dimensions as `PlayerCapsule`.
    NpcCapsule,
    /// Standard boss capsule — half_height 0.9, radius 0.45.
    BossCapsule,
    /// Large boss capsule — half_height 1.5, radius 0.9.
    LargeBossCapsule,
    /// Tall blocking gate cuboid — half-extents (2.0, 2.5, 0.25). Fixed.
    GateCuboid,
    /// Small interactable switch cuboid — half-extents (0.25, 0.25, 0.25). Fixed.
    SwitchCuboid,
    /// Chest cuboid — half-extents (0.5, 0.35, 0.4). Fixed.
    ChestCuboid,
    /// Pushable crate cuboid — half-extents (0.5, 0.5, 0.5). Dynamic + CCD.
    CrateCuboid,
}

impl BodyShape {
    /// All `BodyShape` variants in declaration order. Useful for building
    /// per-shape lookup tables (e.g. client mesh handles).
    pub const ALL: [BodyShape; 8] = [
        BodyShape::PlayerCapsule,
        BodyShape::NpcCapsule,
        BodyShape::BossCapsule,
        BodyShape::LargeBossCapsule,
        BodyShape::GateCuboid,
        BodyShape::SwitchCuboid,
        BodyShape::ChestCuboid,
        BodyShape::CrateCuboid,
    ];

    /// Capsule dimensions `(half_height, radius)`. Returns `None` for cuboid variants.
    pub fn capsule_dims(self) -> Option<(f32, f32)> {
        match self {
            BodyShape::PlayerCapsule => Some((0.5, 0.3)),
            BodyShape::NpcCapsule => Some((0.5, 0.3)),
            BodyShape::BossCapsule => Some((0.9, 0.45)),
            BodyShape::LargeBossCapsule => Some((1.5, 0.9)),
            BodyShape::GateCuboid
            | BodyShape::SwitchCuboid
            | BodyShape::ChestCuboid
            | BodyShape::CrateCuboid => None,
        }
    }

    /// Cuboid half-extents. Returns `None` for capsule variants.
    pub fn cuboid_half_extents(self) -> Option<Vec3f> {
        match self {
            BodyShape::GateCuboid => Some(Vec3f::new(2.0, 2.5, 0.25)),
            BodyShape::SwitchCuboid => Some(Vec3f::new(0.25, 0.25, 0.25)),
            BodyShape::ChestCuboid => Some(Vec3f::new(0.5, 0.35, 0.4)),
            BodyShape::CrateCuboid => Some(Vec3f::new(0.5, 0.5, 0.5)),
            BodyShape::PlayerCapsule
            | BodyShape::NpcCapsule
            | BodyShape::BossCapsule
            | BodyShape::LargeBossCapsule => None,
        }
    }

    /// Whether this prop shape produces a dynamic (pushable) rigid body.
    /// `false` (fixed) for capsule variants and for non-`CrateCuboid` props.
    pub fn is_pushable_prop(self) -> bool {
        matches!(self, BodyShape::CrateCuboid)
    }

    /// True if this shape is a character capsule.
    pub fn is_capsule(self) -> bool {
        self.capsule_dims().is_some()
    }

    /// True if this shape is a cuboid prop.
    pub fn is_cuboid(self) -> bool {
        self.cuboid_half_extents().is_some()
    }

    /// Best-effort `EntityKind` for collision-group dispatch.
    ///
    /// Capsule variants map to `Player`/`Npc`/`Boss`. Cuboid variants
    /// always map to `Prop`. Used by the Rapier backend when only a shape
    /// is available but a kind is needed for `InteractionGroups`.
    pub fn entity_kind(self) -> EntityKind {
        match self {
            BodyShape::PlayerCapsule => EntityKind::Player,
            BodyShape::NpcCapsule => EntityKind::Npc,
            BodyShape::BossCapsule | BodyShape::LargeBossCapsule => EntityKind::Boss,
            BodyShape::GateCuboid
            | BodyShape::SwitchCuboid
            | BodyShape::ChestCuboid
            | BodyShape::CrateCuboid => EntityKind::Prop,
        }
    }

    /// Default capsule shape for an `EntityKind` when no per-entity
    /// override is configured. `Prop`/`Projectile`/`Hazard` have no
    /// natural capsule and fall back to `NpcCapsule`.
    pub fn default_capsule_for_kind(kind: EntityKind) -> BodyShape {
        match kind {
            EntityKind::Player => BodyShape::PlayerCapsule,
            EntityKind::Npc => BodyShape::NpcCapsule,
            EntityKind::Boss => BodyShape::BossCapsule,
            EntityKind::Prop | EntityKind::Projectile | EntityKind::Hazard => BodyShape::NpcCapsule,
        }
    }

    /// Encode as a `u8` discriminant for DB storage. Stable across versions:
    /// values must not be reordered.
    pub fn to_u8(self) -> u8 {
        match self {
            BodyShape::PlayerCapsule => 0,
            BodyShape::NpcCapsule => 1,
            BodyShape::BossCapsule => 2,
            BodyShape::LargeBossCapsule => 3,
            BodyShape::GateCuboid => 4,
            BodyShape::SwitchCuboid => 5,
            BodyShape::ChestCuboid => 6,
            BodyShape::CrateCuboid => 7,
        }
    }

    /// Decode from a `u8` discriminant. Returns `None` for unknown values.
    pub fn from_u8(v: u8) -> Option<BodyShape> {
        Some(match v {
            0 => BodyShape::PlayerCapsule,
            1 => BodyShape::NpcCapsule,
            2 => BodyShape::BossCapsule,
            3 => BodyShape::LargeBossCapsule,
            4 => BodyShape::GateCuboid,
            5 => BodyShape::SwitchCuboid,
            6 => BodyShape::ChestCuboid,
            7 => BodyShape::CrateCuboid,
            _ => return None,
        })
    }

    /// Hurtbox shape used for lag-compensation rewind intersection tests.
    ///
    /// Capsule body shapes return their exact capsule. Cuboid props return a
    /// bounding capsule (radius = max horizontal half-extent, half-height = vertical
    /// half-extent) — props don't move, so this is mostly used by AoE shape tests
    /// that include props in their candidate set.
    pub fn hurtbox_sensor_shape(self) -> SensorShape {
        if let Some((half_height, radius)) = self.capsule_dims() {
            SensorShape::Capsule {
                half_height,
                radius,
            }
        } else if let Some(half) = self.cuboid_half_extents() {
            SensorShape::Capsule {
                half_height: half.y,
                radius: half.x.max(half.z),
            }
        } else {
            SensorShape::Capsule {
                half_height: 0.5,
                radius: 0.3,
            }
        }
    }

    /// Maximum hurtbox radius across all body-shape variants. Used by lag-comp
    /// broadphase to compute a conservative candidate radius without knowing
    /// the specific target shape up-front.
    pub const fn max_hurtbox_radius() -> f32 {
        // Equals LargeBossCapsule radius.
        0.9
    }
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
    fn spawn_sensor(
        &mut self,
        entity_id: EntityId,
        shape: SensorShape,
        offset: Vec3f,
        kind: ColliderKind,
    ) -> Option<u64>;

    /// Spawn a sensor collider at a fixed world position, not attached to any entity body.
    ///
    /// Used for projectiles and ground-targeted abilities whose collision region moves
    /// independently of any entity. `owner` is the caster entity for damage attribution
    /// in collision events. Returns an opaque handle for positioning and removal.
    fn spawn_world_sensor(
        &mut self,
        position: Vec3f,
        shape: SensorShape,
        kind: ColliderKind,
        owner: EntityId,
    ) -> u64;

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
    fn spawn_character_body(
        &mut self,
        entity_id: EntityId,
        position: Vec3f,
        kind: EntityKind,
    ) -> bool;

    /// Spawn a character body using an explicit `BodyShape`.
    ///
    /// Single source of truth for character capsule dimensions used by
    /// physics, lag-comp hurtbox, and client meshes. The default
    /// implementation discards the shape and falls back to
    /// `spawn_character_body` (mock backends without geometry). Real
    /// backends must override to use `shape.capsule_dims()` and remember
    /// the per-entity shape for pooling.
    fn spawn_character_body_shaped(
        &mut self,
        entity_id: EntityId,
        position: Vec3f,
        shape: BodyShape,
    ) -> bool {
        self.spawn_character_body(entity_id, position, shape.entity_kind())
    }

    /// Spawn a dynamic box body for a prop entity.
    ///
    /// Creates a cuboid rigid body with the given half-extents that responds to
    /// physics forces (gravity, collisions). Players and NPCs push it around via
    /// kinematic contacts. Returns true if the body was created, false if the
    /// entity already has a body.
    fn spawn_prop_body(
        &mut self,
        entity_id: EntityId,
        position: Vec3f,
        half_extents: Vec3f,
        pushable: bool,
    ) -> bool;

    /// Spawn a prop body using an explicit `BodyShape`.
    ///
    /// Default implementation pulls `cuboid_half_extents()` and
    /// `is_pushable_prop()` off the shape and delegates to
    /// `spawn_prop_body`. Returns `false` if `shape` is not a cuboid.
    fn spawn_prop_body_shaped(
        &mut self,
        entity_id: EntityId,
        position: Vec3f,
        shape: BodyShape,
    ) -> bool {
        let Some(half_extents) = shape.cuboid_half_extents() else {
            return false;
        };
        self.spawn_prop_body(entity_id, position, half_extents, shape.is_pushable_prop())
    }

    /// Move a kinematic character body by `desired_translation`, sliding along obstacles.
    ///
    /// Uses a character controller to resolve collisions against static geometry
    /// (walls, floors, obstacles).  Returns the corrected world-space position
    /// and whether the character is touching the ground after the move.
    fn move_character(
        &mut self,
        entity_id: EntityId,
        desired_translation: Vec3f,
    ) -> Option<MoveResult>;

    /// Cast a targeting ray from `origin` along `direction` up to `max_distance`.
    ///
    /// Returns the first hit (closest by time-of-impact) among hurtboxes and
    /// blocking world geometry. Character body colliders and non-blocking
    /// gameplay sensors are ignored so target acquisition cannot whiff on the
    /// physical capsule in front of an entity's hurtbox. If `ignore_entity` is
    /// provided, that entity's own rigid body and attached colliders are also
    /// excluded from the query.
    fn raycast(
        &self,
        origin: Vec3f,
        direction: Vec3f,
        max_distance: f32,
        ignore_entity: Option<EntityId>,
    ) -> Option<RayHit>;

    /// Raycast against environment geometry only and return the first hit
    /// point (world-space). Character bodies, hurtboxes, and sensors are
    /// transparent — this query is for resolving ground-target Y, spawn
    /// heights, and similar "where is the floor here?" questions against
    /// terrain (cuboids, cylinders, heightfields).
    ///
    /// Layer-aware: only environment colliders on `layer` (or on the shared
    /// unlayered ground plane, `layer == 0`) are considered.
    ///
    /// Returns `None` if the ray hits nothing within `max_distance`.
    ///
    /// Default: returns `None` (test backends without terrain).
    fn raycast_surface(
        &self,
        origin: Vec3f,
        direction: Vec3f,
        max_distance: f32,
        layer: u32,
    ) -> Option<Vec3f> {
        let _ = (origin, direction, max_distance, layer);
        None
    }

    /// Check line-of-sight between two world positions against environment geometry only.
    ///
    /// Returns `true` if the straight-line path is unobstructed (ray hits nothing).
    /// Ignores character bodies, hurtboxes, and sensors — only static world
    /// geometry (ENVIRONMENT + PROP_BODY + FLIGHT_BLOCKER) can block LoS.
    /// Used for lock-on tagging validation and ground-target placement.
    fn line_of_sight(&self, from: Vec3f, to: Vec3f) -> bool;

    /// Layer-aware line-of-sight: only environment colliders stamped with
    /// `layer` can occlude the ray. Strict same-layer — there is no
    /// shared layer-0 fallback; each layer must author its own geometry.
    ///
    /// Default: delegates to `line_of_sight` (ignores layer).
    fn line_of_sight_on_layer(&self, from: Vec3f, to: Vec3f, _layer: u32) -> bool {
        self.line_of_sight(from, to)
    }

    /// Cast from `from` toward `to` through environment-only geometry and return
    /// the safe teleport destination. If no wall is hit, returns `to` unchanged.
    /// If a wall is hit, returns the contact point pulled back 0.3 units toward `from`
    /// so the traveller stops flush against the wall without overlapping it.
    /// Used by `TeleportForward` and `TeleportBehindTarget` to prevent going through walls.
    fn cast_to_wall(&self, from: Vec3f, to: Vec3f) -> Vec3f;

    /// Layer-aware cast-to-wall: only environment colliders stamped with
    /// `layer` can block the cast. Strict same-layer — there is no shared
    /// layer-0 fallback.
    ///
    /// Default: delegates to `cast_to_wall` (ignores layer).
    fn cast_to_wall_on_layer(&self, from: Vec3f, to: Vec3f, _layer: u32) -> Vec3f {
        self.cast_to_wall(from, to)
    }

    /// Teleport an entity's physics body to `position` immediately.
    ///
    /// Moves the kinematic body in world space (no contact resolution — teleports
    /// through walls). Used by `TeleportBehindTarget` and `TeleportForward` abilities.
    /// No-op and returns `false` if the entity has no physics body.
    fn teleport_entity(&mut self, entity_id: EntityId, position: Vec3f) -> bool;

    // ── Environment collider layer management ───────────────────

    /// Add static environment geometry at the given position, tagged with an instance layer.
    /// Returns an opaque handle for later removal.
    /// Used by dungeon instance creation to spawn walls, floors, pillars.
    fn add_environment_collider_on_layer(
        &mut self,
        shape: EnvironmentShape,
        position: Vec3f,
        layer: u32,
    ) -> u64 {
        let _ = (shape, position, layer);
        0
    }

    /// Remove all environment colliders tagged with the given layer.
    /// Used for bulk cleanup when a dungeon instance expires.
    fn remove_environment_colliders_by_layer(&mut self, layer: u32) {
        let _ = layer;
    }

    /// Remove a single environment collider previously returned from
    /// [`add_environment_collider_on_layer`]. Used by the live terrain
    /// edit pipeline to swap one chunk's TriMesh without rebuilding the
    /// whole layer. Returns `true` if the handle was known and removed.
    fn remove_environment_collider(&mut self, handle: u64) -> bool {
        let _ = handle;
        false
    }

    /// Toggle a prop entity's collider enabled/disabled (gate open/close).
    /// Uses Rapier's `Collider::set_enabled()` natively.
    fn set_collider_enabled(&mut self, entity_id: EntityId, enabled: bool) -> bool {
        let _ = (entity_id, enabled);
        false
    }

    /// Disable a character body and pool it for later reuse instead of destroying it.
    ///
    /// The body and its colliders stay allocated but are removed from broadphase
    /// and collision detection. `kind` is used as the pool key so bodies are
    /// reused within the same entity kind (matching collider geometry).
    ///
    /// Default: falls back to `remove_entity` (no pooling).
    fn disable_entity(&mut self, entity_id: EntityId, _kind: EntityKind) -> bool {
        self.remove_entity(entity_id)
    }

    /// Try to reuse a pooled character body for `entity_id`, or create a fresh one.
    ///
    /// Default: falls back to `spawn_character_body` (no pooling).
    fn reuse_or_spawn_character(
        &mut self,
        entity_id: EntityId,
        position: Vec3f,
        kind: EntityKind,
    ) -> bool {
        self.spawn_character_body(entity_id, position, kind)
    }

    /// Try to reuse a pooled character body for `entity_id`, or create a fresh
    /// one with the requested `BodyShape`.
    ///
    /// Default: delegates to `reuse_or_spawn_character` using
    /// `shape.entity_kind()` and ignores per-shape capsule geometry. Real
    /// backends must override to pool by shape so different capsule sizes
    /// (e.g. boss vs player) never share pooled bodies.
    fn reuse_or_spawn_character_shaped(
        &mut self,
        entity_id: EntityId,
        position: Vec3f,
        shape: BodyShape,
    ) -> bool {
        self.reuse_or_spawn_character(entity_id, position, shape.entity_kind())
    }

    /// Remove excess pooled bodies above `max_idle` to bound memory.
    ///
    /// Default: no-op (no pool to drain).
    fn drain_pool(&mut self, _max_idle: usize) {}

    // ── Layer metadata ──────────────────────────────────────────

    /// Set the visibility/isolation layer for an entity in the physics runtime.
    ///
    /// Used by scene-query predicates to filter colliders so entities on
    /// different layers never interact physically. Called on spawn and when
    /// an entity changes layer (instance join/leave).
    ///
    /// Default: no-op.
    fn set_entity_layer(&mut self, _entity_id: EntityId, _layer: u32) {}

    /// Set the team for an entity in the physics runtime.
    /// Get the visibility/isolation layer for an entity.
    ///
    /// Returns 0 (open world) if the entity is unknown or layer was never set.
    fn entity_layer(&self, _entity_id: EntityId) -> u32 {
        0
    }

    /// Register a collision policy for a layer.
    ///
    /// Called when a dungeon instance is created; the policy is looked up by
    /// scene-query predicates to decide whether specific entity kinds can
    /// interact physically (e.g. player-vs-player collision).
    ///
    /// Default: no-op.
    fn set_layer_policy(&mut self, _layer: u32, _policy: LayerCollisionPolicy) {}

    /// Get the collision policy for a layer.
    ///
    /// Returns `LayerCollisionPolicy::default()` for unknown layers.
    fn layer_policy(&self, _layer: u32) -> LayerCollisionPolicy {
        LayerCollisionPolicy::default()
    }

    /// Remove the collision policy for a layer (instance teardown).
    ///
    /// Default: no-op.
    fn remove_layer_policy(&mut self, _layer: u32) {}
}

/// Abstract shape for environment colliders (walls, floors, pillars).
/// Maps to a physics engine shape without exposing engine-specific types.
#[derive(Clone, Debug)]
pub enum EnvironmentShape {
    Cuboid {
        half_x: f32,
        half_y: f32,
        half_z: f32,
    },
    Cylinder {
        half_height: f32,
        radius: f32,
    },
    /// Heightfield terrain on the x-z plane. See
    /// `game_schema::dungeon::ShapeDef::Heightfield` for layout.
    Heightfield {
        nrows: usize,
        ncols: usize,
        scale_x: f32,
        scale_y: f32,
        scale_z: f32,
        heights: Vec<f32>,
    },
    /// Indexed triangle mesh. `vertices` is `[x, y, z, ...]` (flat),
    /// `indices` is a flat triangle list. See
    /// `game_schema::dungeon::ShapeDef::TriMesh` for the authoring layout.
    /// Backends that don't support triangle meshes may fall back to a
    /// degenerate filler collider.
    TriMesh {
        vertices: Vec<f32>,
        indices: Vec<u32>,
    },
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
