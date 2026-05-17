use rapier3d::prelude::*;
use rapier3d::control::{CharacterAutostep, CharacterLength, KinematicCharacterController};
use rapier3d::math::{Pose, Vector};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use game_protocol::entity_id::EntityId;
use game_schema::EntityKind;
use game_core::physics_backend::{
    ColliderKind,
    CollisionEvent as GameCollisionEvent,
    EnvironmentShape,
    MoveResult,
    PhysicsBackend,
    RayHit,
};
use super::collision_groups;

/// Wraps the full Rapier physics simulation state.
///
/// This is the long-lived physics world owned by the simulation worker.
/// Per architecture docs:
/// - PhysicsPipeline is reused across frames (scratch buffer reuse)
/// - QueryPipeline is obtained from BroadPhaseBvh on demand for scene queries
/// - Entity ↔ handle mappings are maintained bidirectionally
///
/// Rapier 0.32 specifics:
/// - BroadPhase is now BVH-based (BroadPhaseBvh) for better performance
/// - Math types are glam-based (Vec3, Quat) via parry3d, not nalgebra
/// - QueryPipeline is a borrowed struct obtained from BroadPhaseBvh
/// - Step takes gravity as Vector (copy), not reference
pub struct PhysicsWorld {
    // Core Rapier simulation state
    pipeline: PhysicsPipeline,
    params: IntegrationParameters,
    gravity: Vector,
    islands: IslandManager,
    broad_phase: BroadPhaseBvh,
    narrow_phase: NarrowPhase,
    bodies: RigidBodySet,
    colliders: ColliderSet,
    impulse_joints: ImpulseJointSet,
    multibody_joints: MultibodyJointSet,
    ccd_solver: CCDSolver,

    // Collision event collection
    collision_send: mpsc::Sender<CollisionEvent>,
    collision_recv: mpsc::Receiver<CollisionEvent>,
    contact_force_send: mpsc::Sender<ContactForceEvent>,
    contact_force_recv: mpsc::Receiver<ContactForceEvent>,

    // Entity ↔ Rapier handle mapping
    entity_to_body: HashMap<EntityId, RigidBodyHandle>,
    body_to_entity: HashMap<RigidBodyHandle, EntityId>,

    // Collider metadata — tracks the role of every collider
    collider_kinds: HashMap<ColliderHandle, ColliderKind>,
    // Game-level opaque sensor handles: u64 → ColliderHandle.
    sensor_handle_counter: u64,
    sensor_handles: HashMap<u64, ColliderHandle>,
    // World-space (parentless) sensors → owning entity for collision resolution.
    world_sensor_owners: HashMap<ColliderHandle, EntityId>,
    // Layer-tagged environment colliders for dungeon instance bulk removal.
    env_collider_counter: u64,
    env_collider_handles: HashMap<u64, ColliderHandle>,
    env_colliders_by_layer: HashMap<u32, Vec<u64>>,
}

/// Result of a raycast query.
pub struct RaycastHit {
    pub collider: ColliderHandle,
    pub toi: Real,
    pub normal: Vector,
}

impl PhysicsWorld {
    /// Create a new physics world with the given fixed timestep.
    ///
    /// The timestep should match the simulation tick rate.
    /// For 20Hz: dt = 0.05
    /// For 60Hz: dt = 1/60 ≈ 0.01667
    pub fn new(dt: f32) -> Self {
        let params = IntegrationParameters { dt, ..Default::default() };

        let (collision_send, collision_recv) = mpsc::channel();
        let (contact_force_send, contact_force_recv) = mpsc::channel();

        let mut world = Self {
            pipeline: PhysicsPipeline::new(),
            params,
            gravity: Vector::new(0.0, -9.81, 0.0),
            islands: IslandManager::new(),
            broad_phase: BroadPhaseBvh::new(),
            narrow_phase: NarrowPhase::new(),
            bodies: RigidBodySet::new(),
            colliders: ColliderSet::new(),
            impulse_joints: ImpulseJointSet::new(),
            multibody_joints: MultibodyJointSet::new(),
            ccd_solver: CCDSolver::new(),
            collision_send,
            collision_recv,
            contact_force_send,
            contact_force_recv,
            entity_to_body: HashMap::new(),
            body_to_entity: HashMap::new(),
            collider_kinds: HashMap::new(),
            sensor_handle_counter: 0,
            sensor_handles: HashMap::new(),
            world_sensor_owners: HashMap::new(),
            env_collider_counter: 0,
            env_collider_handles: HashMap::new(),
            env_colliders_by_layer: HashMap::new(),
        };

        // Default ground plane — every world has a floor.
        world.add_environment_collider(
            SharedShape::cuboid(500.0, 0.1, 500.0),
            Vector::new(0.0, 0.0, 0.0),
        );
        world
    }

    /// Advance the physics simulation by one fixed timestep.
    pub fn step(&mut self) {
        let event_handler = ChannelEventCollector::new(
            self.collision_send.clone(),
            self.contact_force_send.clone(),
        );

        self.pipeline.step(
            self.gravity,
            &self.params,
            &mut self.islands,
            &mut self.broad_phase,
            &mut self.narrow_phase,
            &mut self.bodies,
            &mut self.colliders,
            &mut self.impulse_joints,
            &mut self.multibody_joints,
            &mut self.ccd_solver,
            &(),
            &event_handler,
        );
    }

    /// Drain all collision events from the last step, resolved to entity IDs.
    pub fn drain_collision_events(&self) -> Vec<GameCollisionEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.collision_recv.try_recv() {
            let c1 = event.collider1();
            let c2 = event.collider2();
            let entity1 = self.entity_for_collider(c1);
            let entity2 = self.entity_for_collider(c2);
            if let (Some(e1), Some(e2)) = (entity1, entity2) {
                events.push(GameCollisionEvent {
                    entity1: e1,
                    entity2: e2,
                    kind1: self.kind_for_collider(c1),
                    kind2: self.kind_for_collider(c2),
                    started: event.started(),
                    is_sensor: event.sensor(),
                });
            }
        }
        events
    }

    /// Drain contact force events from the last step.
    pub fn drain_contact_force_events(&self) -> Vec<ContactForceEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.contact_force_recv.try_recv() {
            events.push(event);
        }
        events
    }

    /// Resolve a collider handle to its parent entity ID.
    fn entity_for_collider(&self, collider_handle: ColliderHandle) -> Option<EntityId> {
        let collider = self.colliders.get(collider_handle)?;
        if let Some(body_handle) = collider.parent() {
            return self.body_to_entity.get(&body_handle).copied();
        }
        // Parentless (world-space) sensor — look up via world_sensor_owners.
        self.world_sensor_owners.get(&collider_handle).copied()
    }

    /// Resolve a collider handle to its `ColliderKind`.
    /// Defaults to `Body` for colliders that were never registered
    /// (e.g. environment / legacy bodies).
    fn kind_for_collider(&self, handle: ColliderHandle) -> ColliderKind {
        self.collider_kinds
            .get(&handle)
            .copied()
            .unwrap_or(ColliderKind::Body)
    }

    /// Get a QueryPipeline for scene queries, borrowing from the broad phase.
    fn query_pipeline(&self) -> QueryPipeline<'_> {
        self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.bodies,
            &self.colliders,
            QueryFilter::default(),
        )
    }

    // ── Environment geometry ────────────────────────────────────────
    //
    // Static world geometry (ground, walls, dungeon structures, obstacles).
    // These are NOT game entities — they have no EntityId, no body-to-entity
    // mapping, and are invisible to `get_all_transforms` / region updates.
    // In Rapier, parentless colliders act as fixed obstacles.

    /// Add static environment geometry at the given position.
    /// Returns the collider handle for later removal if needed.
    pub fn add_environment_collider(
        &mut self,
        shape: SharedShape,
        position: Vector,
    ) -> ColliderHandle {
        let collider = ColliderBuilder::new(shape)
            .translation(position)
            .collision_groups(collision_groups::environment_groups())
            .active_collision_types(
                ActiveCollisionTypes::default()
                    | ActiveCollisionTypes::KINEMATIC_FIXED
                    | ActiveCollisionTypes::DYNAMIC_FIXED,
            )
            .build();
        // Parentless collider — fixed in world space, no rigid body needed.
        self.colliders.insert(collider)
    }

    // ── Body creation ───────────────────────────────────────────────

    /// Add a dynamic sphere body at the given position.
    /// Also attaches a hurtbox sensor of the same radius.
    pub fn add_dynamic_sphere(
        &mut self,
        entity_id: EntityId,
        position: Vector,
        radius: f32,
        density: f32,
        groups: InteractionGroups,
    ) -> EntityId {

        let body = RigidBodyBuilder::dynamic()
            .translation(position)
            .ccd_enabled(true)
            .build();
        let body_handle = self.bodies.insert(body);

        let collider = ColliderBuilder::ball(radius)
            .density(density)
            .restitution(0.3)
            .collision_groups(groups)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        let ch = self.colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);
        self.collider_kinds.insert(ch, ColliderKind::Body);

        // Hurtbox sensor — same shape, sensor-only, hurtbox collision group.
        let hurtbox = ColliderBuilder::ball(radius)
            .sensor(true)
            .collision_groups(collision_groups::skill_hurtbox_groups())
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        let hch = self.colliders
            .insert_with_parent(hurtbox, body_handle, &mut self.bodies);
        self.collider_kinds.insert(hch, ColliderKind::Hurtbox);

        self.entity_to_body.insert(entity_id, body_handle);
        self.body_to_entity.insert(body_handle, entity_id);

        entity_id
    }

    /// Add a dynamic capsule body (useful for player/NPC characters).
    /// Also attaches a hurtbox sensor of the same capsule shape.
    pub fn add_dynamic_capsule(
        &mut self,
        entity_id: EntityId,
        position: Vector,
        half_height: f32,
        radius: f32,
        density: f32,
        groups: InteractionGroups,
    ) -> EntityId {

        let body = RigidBodyBuilder::dynamic()
            .translation(position)
            .ccd_enabled(true)
            .build();
        let body_handle = self.bodies.insert(body);

        let collider = ColliderBuilder::capsule_y(half_height, radius)
            .density(density)
            .collision_groups(groups)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        let ch = self.colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);
        self.collider_kinds.insert(ch, ColliderKind::Body);

        // Hurtbox sensor — same capsule shape, sensor-only.
        let hurtbox = ColliderBuilder::capsule_y(half_height, radius)
            .sensor(true)
            .collision_groups(collision_groups::skill_hurtbox_groups())
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        let hch = self.colliders
            .insert_with_parent(hurtbox, body_handle, &mut self.bodies);
        self.collider_kinds.insert(hch, ColliderKind::Hurtbox);

        self.entity_to_body.insert(entity_id, body_handle);
        self.body_to_entity.insert(body_handle, entity_id);

        entity_id
    }

    /// Add a kinematic body (server-controlled movement, not physics-driven).
    /// Used for: flight, scripted movement, elevators.
    /// Also attaches a hurtbox sensor of the same capsule shape.
    pub fn add_kinematic_capsule(
        &mut self,
        entity_id: EntityId,
        position: Vector,
        half_height: f32,
        radius: f32,
        groups: InteractionGroups,
    ) -> EntityId {

        let body = RigidBodyBuilder::kinematic_position_based()
            .translation(position)
            .build();
        let body_handle = self.bodies.insert(body);

        // Include KINEMATIC_KINEMATIC so sensor hitboxes (attached to a kinematic player body)
        // can register contacts against this kinematic body collider.
        // Rapier defaults exclude kinematic-kinematic pairs — without this, skill hitboxes
        // fired by kinematic player bodies never detect kinematic NPC bodies.
        // KINEMATIC_FIXED enables world-space sensors (HazardZone etc.) to detect these bodies.
        let active_types = ActiveCollisionTypes::default()
            | ActiveCollisionTypes::KINEMATIC_KINEMATIC
            | ActiveCollisionTypes::KINEMATIC_FIXED;
        let collider = ColliderBuilder::capsule_y(half_height, radius)
            .collision_groups(groups)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .active_collision_types(active_types)
            .build();
        let ch = self.colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);
        self.collider_kinds.insert(ch, ColliderKind::Body);

        // Hurtbox sensor — same capsule shape, sensor-only.
        // Also needs KINEMATIC_KINEMATIC for the same reason as the body collider above.
        let hurtbox = ColliderBuilder::capsule_y(half_height, radius)
            .sensor(true)
            .collision_groups(collision_groups::skill_hurtbox_groups())
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .active_collision_types(active_types)
            .build();
        let hch = self.colliders
            .insert_with_parent(hurtbox, body_handle, &mut self.bodies);
        self.collider_kinds.insert(hch, ColliderKind::Hurtbox);

        self.entity_to_body.insert(entity_id, body_handle);
        self.body_to_entity.insert(body_handle, entity_id);

        entity_id
    }

    /// Add a sensor collider attached to an existing body.
    /// Used for: skill hitboxes, trigger zones.
    /// Sensors detect overlap but produce no physical contact forces.
    pub fn add_sensor_to_entity(
        &mut self,
        entity_id: EntityId,
        shape: SharedShape,
        offset: Pose,
        groups: InteractionGroups,
        kind: ColliderKind,
    ) -> Option<ColliderHandle> {
        let body_handle = *self.entity_to_body.get(&entity_id)?;

        // Skill hitboxes must detect kinematic NPC bodies — include KINEMATIC_KINEMATIC
        // so contacts fire even when both the caster and target are kinematic bodies.
        // KINEMATIC_FIXED enables world-space sensors (HazardZone etc.) to fire contacts.
        let active_types = ActiveCollisionTypes::default()
            | ActiveCollisionTypes::KINEMATIC_KINEMATIC
            | ActiveCollisionTypes::KINEMATIC_FIXED;
        let collider = ColliderBuilder::new(shape)
            .position(offset)
            .sensor(true)
            .collision_groups(groups)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .active_collision_types(active_types)
            .build();
        let handle = self.colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);
        self.collider_kinds.insert(handle, kind);
        Some(handle)
    }

    /// Remove a specific collider (e.g. a skill hitbox sensor).
    pub fn remove_collider(&mut self, handle: ColliderHandle) {
        self.collider_kinds.remove(&handle);
        self.world_sensor_owners.remove(&handle);
        self.colliders.remove(handle, &mut self.islands, &mut self.bodies, true);
    }

    // ── Body removal ────────────────────────────────────────────

    /// Remove an entity and its physics body/colliders from the world.
    pub fn remove_entity(&mut self, entity_id: EntityId) -> bool {
        if let Some(body_handle) = self.entity_to_body.remove(&entity_id) {
            self.body_to_entity.remove(&body_handle);
            // Clean up collider metadata for all colliders attached to this body.
            // Rapier's body removal cascades to colliders, so we mirror that.
            let attached: std::collections::HashSet<ColliderHandle> = self.collider_kinds
                .keys()
                .filter(|ch| {
                    self.colliders.get(**ch)
                        .and_then(|c| c.parent()) == Some(body_handle)
                })
                .copied()
                .collect();

            // Remove any opaque sensor handles pointing at colliders attached to this body
            // so the internal `sensor_handles` map does not grow unbounded.
            self.sensor_handles.retain(|_, ch| !attached.contains(ch));

            for ch in attached.iter() {
                self.collider_kinds.remove(ch);
            }

            // Clean up world-space sensor ownership entries for this entity so the
            // map does not leak references to removed entities over long sessions.
            self.world_sensor_owners.retain(|_, &mut owner| owner != entity_id);
            self.bodies.remove(
                body_handle,
                &mut self.islands,
                &mut self.colliders,
                &mut self.impulse_joints,
                &mut self.multibody_joints,
                true,
            );
            true
        } else {
            false
        }
    }

    // ── State queries ───────────────────────────────────────────

    /// Get the position of a body by entity ID.
    pub fn get_body_position(&self, entity_id: EntityId) -> Option<Vector> {
        let handle = self.entity_to_body.get(&entity_id)?;
        let body = self.bodies.get(*handle)?;
        Some(body.translation())
    }

    /// Get the full transform (position + rotation + velocity) of a body.
    pub fn get_body_transform(
        &self,
        entity_id: EntityId,
    ) -> Option<game_protocol::types::Transform> {
        let handle = self.entity_to_body.get(&entity_id)?;
        let body = self.bodies.get(*handle)?;
        Some(super::conversions::body_to_transform(body))
    }

    /// Set the position of a kinematic body, preserving current rotation.
    /// Clamps Y to prevent embedding characters below the ground surface.
    pub fn set_kinematic_position(
        &mut self,
        entity_id: EntityId,
        position: Vector,
    ) -> bool {
        if let Some(handle) = self.entity_to_body.get(&entity_id)
            && let Some(body) = self.bodies.get_mut(*handle) {
                let mut pos = position;
                let min_y = game_core::physics_constants::MIN_CHARACTER_Y;
                if pos.y < min_y {
                    pos.y = min_y;
                }
                // Preserve any pending rotation set earlier in this tick.
                let rotation = body.next_position().rotation;
                body.set_next_kinematic_position(Pose::from_parts(
                    pos,
                    rotation,
                ));
                return true;
            }
        false
    }

    /// Set the rotation of a kinematic body, preserving the queued position.
    ///
    /// Uses `body.next_position()` so that a prior `set_next_kinematic_position`
    /// from `move_character` (e.g. arc movement) is not overwritten by resetting
    /// the position back to the last-committed `translation()`.
    pub fn set_kinematic_rotation(
        &mut self,
        entity_id: EntityId,
        rotation: Rotation,
    ) -> bool {
        if let Some(handle) = self.entity_to_body.get(&entity_id)
            && let Some(body) = self.bodies.get_mut(*handle) {
                let position = body.next_position().translation;
                body.set_next_kinematic_position(Pose::from_parts(
                    position,
                    rotation,
                ));
                return true;
            }
        false
    }

    // ── Scene queries ───────────────────────────────────────────

    /// Cast a ray and return the first hit.
    pub fn raycast(
        &self,
        origin: Vector,
        direction: Vector,
        max_toi: f32,
    ) -> Option<RaycastHit> {
        let query = self.query_pipeline();
        let ray = Ray::new(origin, direction);
        query
            .cast_ray_and_get_normal(
                &ray,
                max_toi,
                true,
            )
            .map(|(collider, intersection)| RaycastHit {
                collider,
                toi: intersection.time_of_impact,
                normal: intersection.normal,
            })
    }

    /// Find all colliders intersecting a sphere at the given position.
    /// Useful for AoE abilities, proximity checks.
    pub fn intersections_with_sphere(
        &self,
        center: Vector,
        radius: f32,
    ) -> Vec<ColliderHandle> {
        let query = self.query_pipeline();
        let shape = Ball::new(radius);
        let shape_pos = Pose::translation(center.x, center.y, center.z);

        query.intersect_shape(shape_pos, &shape)
            .map(|(handle, _)| handle)
            .collect()
    }

    // ── Accessors for advanced usage ────────────────────────────

    pub fn body_count(&self) -> usize {
        self.bodies.len()
    }

    pub fn entity_for_body(&self, handle: RigidBodyHandle) -> Option<EntityId> {
        self.body_to_entity.get(&handle).copied()
    }

    pub fn body_handle_for_entity(&self, entity_id: EntityId) -> Option<RigidBodyHandle> {
        self.entity_to_body.get(&entity_id).copied()
    }

    /// Get immutable access to the island manager.
    /// Useful for iterating only active (non-sleeping) bodies.
    pub fn islands(&self) -> &IslandManager {
        &self.islands
    }

    /// Get immutable access to the body set.
    pub fn bodies(&self) -> &RigidBodySet {
        &self.bodies
    }

    /// Get immutable access to the collider set.
    pub fn colliders(&self) -> &ColliderSet {
        &self.colliders
    }
}

impl PhysicsBackend for PhysicsWorld {
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any { self }

    fn step(&mut self, _dt: f32) {
        // dt is already baked into IntegrationParameters; call Rapier step.
        self.step();
    }

    fn get_transform(&self, entity_id: EntityId) -> Option<game_protocol::types::Transform> {
        self.get_body_transform(entity_id)
    }

    fn get_all_transforms(&self) -> Vec<(EntityId, game_protocol::types::Transform)> {
        self.entity_to_body
            .iter()
            .filter_map(|(eid, handle)| {
                let body = self.bodies.get(*handle)?;
                Some((*eid, super::conversions::body_to_transform(body)))
            })
            .collect()
    }

    fn drain_collision_events(&mut self) -> Vec<GameCollisionEvent> {
        // Delegate to the inherent method (same return type now).
        PhysicsWorld::drain_collision_events(self)
    }

    fn remove_entity(&mut self, entity_id: EntityId) -> bool {
        // Delegate to the inherent method via UFCS.
        PhysicsWorld::remove_entity(self, entity_id)
    }

    fn set_kinematic_position(
        &mut self,
        entity_id: EntityId,
        position: game_protocol::types::Vec3f,
    ) -> bool {
        let v = Vector::new(position.x, position.y, position.z);
        PhysicsWorld::set_kinematic_position(self, entity_id, v)
    }

    fn set_kinematic_rotation(
        &mut self,
        entity_id: EntityId,
        rotation: game_protocol::types::Quatf,
    ) -> bool {
        let r = super::conversions::quatf_to_rotation(rotation);
        PhysicsWorld::set_kinematic_rotation(self, entity_id, r)
    }

    fn set_linear_velocity(
        &mut self,
        entity_id: EntityId,
        velocity: game_protocol::types::Vec3f,
    ) -> bool {
        if let Some(handle) = self.entity_to_body.get(&entity_id)
            && let Some(body) = self.bodies.get_mut(*handle) {
                body.set_linvel(Vector::new(velocity.x, velocity.y, velocity.z), true);
                return true;
            }
        false
    }

    fn spawn_sensor(
        &mut self,
        entity_id: EntityId,
        shape: game_core::physics_backend::SensorShape,
        offset: game_protocol::types::Vec3f,
        kind: ColliderKind,
    ) -> Option<u64> {
        use game_core::physics_backend::SensorShape;
        let rapier_shape: SharedShape = match shape {
            SensorShape::Sphere { radius } => SharedShape::ball(radius),
            SensorShape::Capsule { half_height, radius } => SharedShape::capsule_y(half_height, radius),
        };
        let groups = match kind {
            ColliderKind::Hitbox(_) => collision_groups::skill_hitbox_groups(),
            ColliderKind::Hurtbox  => collision_groups::skill_hurtbox_groups(),
            _                      => collision_groups::skill_hitbox_groups(),
        };
        let pose = Pose::translation(offset.x, offset.y, offset.z);
        let handle = self.add_sensor_to_entity(entity_id, rapier_shape, pose, groups, kind)?;
        let opaque = self.sensor_handle_counter;
        self.sensor_handle_counter += 1;
        self.sensor_handles.insert(opaque, handle);
        Some(opaque)
    }

    fn remove_sensor(&mut self, handle: u64) {
        if let Some(col_handle) = self.sensor_handles.remove(&handle) {
            self.remove_collider(col_handle);
        }
    }

    fn spawn_world_sensor(
        &mut self,
        position: game_protocol::types::Vec3f,
        shape: game_core::physics_backend::SensorShape,
        kind: ColliderKind,
        owner: EntityId,
    ) -> u64 {
        use game_core::physics_backend::SensorShape;
        let rapier_shape: SharedShape = match shape {
            SensorShape::Sphere { radius } => SharedShape::ball(radius),
            SensorShape::Capsule { half_height, radius } => SharedShape::capsule_y(half_height, radius),
        };
        let groups = match kind {
            ColliderKind::Hitbox(_) => collision_groups::skill_hitbox_groups(),
            ColliderKind::Hurtbox  => collision_groups::skill_hurtbox_groups(),
            _                      => collision_groups::skill_hitbox_groups(),
        };
        let active_types = ActiveCollisionTypes::default()
            | ActiveCollisionTypes::KINEMATIC_KINEMATIC
            | ActiveCollisionTypes::KINEMATIC_FIXED;
        let collider = ColliderBuilder::new(rapier_shape)
            .translation(Vector::new(position.x, position.y, position.z))
            .sensor(true)
            .collision_groups(groups)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .active_collision_types(active_types)
            .build();
        // Insert without a parent body — collider is free-standing in world space.
        let col_handle = self.colliders.insert(collider);
        self.collider_kinds.insert(col_handle, kind);
        self.world_sensor_owners.insert(col_handle, owner);
        let opaque = self.sensor_handle_counter;
        self.sensor_handle_counter += 1;
        self.sensor_handles.insert(opaque, col_handle);
        opaque
    }

    fn set_sensor_position(&mut self, handle: u64, position: game_protocol::types::Vec3f) -> bool {
        if let Some(&col_handle) = self.sensor_handles.get(&handle) {
            if let Some(collider) = self.colliders.get_mut(col_handle) {
                collider.set_translation(Vector::new(position.x, position.y, position.z));
                return true;
            }
        }
        false
    }

    fn sensor_intersections(&self, handle: u64) -> Vec<EntityId> {
        let Some(&sensor_handle) = self.sensor_handles.get(&handle) else {
            return Vec::new();
        };

        let Some(sensor) = self.colliders.get(sensor_handle) else {
            return Vec::new();
        };

        let sensor_pose = *sensor.position();
        let sensor_shape = sensor.shape();
        let query = self.query_pipeline();

        // Use a direct shape query against the current scene instead of
        // transition events so stationary occupants are still detected.
        let mut entities = HashSet::new();
        for (other, _) in query.intersect_shape(sensor_pose, sensor_shape) {
            if other == sensor_handle {
                continue;
            }
            match self.kind_for_collider(other) {
                ColliderKind::Body | ColliderKind::Hurtbox => {
                    if let Some(entity_id) = self.entity_for_collider(other) {
                        entities.insert(entity_id);
                    }
                }
                _ => {}
            }
        }

        let mut entities: Vec<_> = entities.into_iter().collect();
        entities.sort_by_key(|entity_id| entity_id.0);
        entities
    }

    fn spawn_character_body(
        &mut self,
        entity_id: EntityId,
        position: game_protocol::types::Vec3f,
        kind: EntityKind,
    ) -> bool {
        if self.entity_to_body.contains_key(&entity_id) {
            return false;
        }
        let pos = Vector::new(position.x, position.y, position.z);
        // Standard character capsule: half_height=0.5, radius=0.3.
        // Select the correct collision layer based on entity kind (bug #14 fix).
        let groups = match kind {
            EntityKind::Player => collision_groups::player_body_groups(),
            EntityKind::Npc | EntityKind::Boss => collision_groups::npc_body_groups(),
            // Projectile/Hazard/Prop bodies are not created via this path.
            EntityKind::Projectile | EntityKind::Hazard | EntityKind::Prop => collision_groups::player_body_groups(),
        };
        self.add_kinematic_capsule(entity_id, pos, 0.5, 0.3, groups);
        true
    }

    fn spawn_prop_body(
        &mut self,
        entity_id: EntityId,
        position: game_protocol::types::Vec3f,
        half_extents: game_protocol::types::Vec3f,
        pushable: bool,
    ) -> bool {
        if self.entity_to_body.contains_key(&entity_id) {
            return false;
        }
        let pos = Vector::new(position.x, position.y, position.z);

        let body = if pushable {
            RigidBodyBuilder::dynamic()
                .translation(pos)
                .ccd_enabled(true)
                .linear_damping(0.5)
                .angular_damping(0.8)
                .build()
        } else {
            RigidBodyBuilder::fixed()
                .translation(pos)
                .ccd_enabled(true)
                .build()
        };
        let body_handle = self.bodies.insert(body);

        let collider = ColliderBuilder::cuboid(half_extents.x, half_extents.y, half_extents.z)
            .density(2.0)
            .restitution(0.1)
            .friction(0.7)
            .collision_groups(collision_groups::prop_body_groups())
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .active_collision_types(
                ActiveCollisionTypes::default()
                    | ActiveCollisionTypes::DYNAMIC_KINEMATIC
                    | ActiveCollisionTypes::DYNAMIC_FIXED,
            )
            .build();
        let ch = self.colliders.insert_with_parent(collider, body_handle, &mut self.bodies);
        self.collider_kinds.insert(ch, ColliderKind::Body);

        self.entity_to_body.insert(entity_id, body_handle);
        self.body_to_entity.insert(body_handle, entity_id);
        true
    }

    fn move_character(
        &mut self,
        entity_id: EntityId,
        desired_translation: game_protocol::types::Vec3f,
    ) -> Option<MoveResult> {
        let handle = *self.entity_to_body.get(&entity_id)?;
        let desired_vec = Vector::new(desired_translation.x, desired_translation.y, desired_translation.z);

        // Extract position and collider handle up front so the subsequent block
        // can borrow self freely without conflicting with these short-lived borrows.
        // Uses `next_position()` so that multiple move_character calls within the
        // same tick chain correctly (e.g. movement + repulsion). `position()` only
        // reflects the last physics step, so a prior `set_next_kinematic_position`
        // would be invisible and overwritten.
        let (current_pos, body_collider_handle) = {
            let body = self.bodies.get(handle)?;
            let ch = body.colliders().first().copied()?;
            (*body.next_position(), ch)
        };

        // Run the character controller in its own block so the borrows on
        // self.bodies / self.colliders (via `shape` and `queries`) are fully
        // released before we mutate self.bodies to apply push impulses below.
        let (movement, push_targets) = {
            let collider = self.colliders.get(body_collider_handle)?;
            let shape = collider.shape();
            // Exclude the entity's own body AND restrict to environment/prop/flight-blocker
            // geometry so characters never collide with other character capsules during
            // movement.  This prevents capsule stacking, landing-on-heads after launch CC,
            // and getting wedged between overlapping capsules.
            let filter = QueryFilter::default()
                .exclude_rigid_body(handle)
                .groups(collision_groups::kcc_movement_groups());
            let queries = self.broad_phase.as_query_pipeline(
                self.narrow_phase.query_dispatcher(),
                &self.bodies,
                &self.colliders,
                filter,
            );
            let controller = KinematicCharacterController {
                offset: CharacterLength::Relative(0.02),
                autostep: Some(CharacterAutostep {
                    max_height: CharacterLength::Relative(0.15),
                    min_width: CharacterLength::Relative(0.2),
                    include_dynamic_bodies: false,
                }),
                snap_to_ground: Some(CharacterLength::Relative(0.2)),
                ..Default::default()
            };
            let mut hits: Vec<ColliderHandle> = Vec::new();
            let mv = controller.move_shape(
                0.0, // dt=0: server controls Y; no gravity/slope friction needed
                &queries,
                shape,
                &current_pos,
                desired_vec,
                |collision| hits.push(collision.handle),
            );
            (mv, hits)
        }; // shape, queries, collider, controller all dropped here

        // Apply push impulses to any dynamic body the character touched.
        // Strength is tuned so a 2 kg prop reaches roughly player walk speed
        // in a few ticks of contact, then coasts away.
        const PUSH_STRENGTH: f32 = 2.0; // N·s per contact tick
        if desired_vec.length_squared() > 1e-6 {
            let push_dir = desired_vec.normalize();
            for &ch in &push_targets {
                let parent = match self.colliders.get(ch).and_then(|c| c.parent()) {
                    Some(p) if p != handle => p,
                    _ => continue,
                };
                if let Some(body) = self.bodies.get_mut(parent) {
                    if body.is_dynamic() {
                        body.apply_impulse(push_dir * PUSH_STRENGTH, true);

                        // Clamp horizontal speed so stacked players can't send
                        // the box flying. Applied immediately after each impulse
                        // so it self-limits regardless of how many players push
                        // in a single tick. Y is left untouched (gravity/bounce).
                        const MAX_PUSH_SPEED: f32 = 6.0; // m/s
                        let v = body.linvel();
                        let h = Vector::new(v.x, 0.0, v.z);
                        if h.length() > MAX_PUSH_SPEED {
                            let clamped = h.normalize() * MAX_PUSH_SPEED;
                            body.set_linvel(Vector::new(clamped.x, v.y, clamped.z), false);
                        }
                    }
                }
            }
        }

        let mut new_pos = current_pos.translation + movement.translation;

        // Ground clamp: prevent the character center from sinking below the
        // floor surface.  The KCC offset + snap_to_ground handle normal cases,
        // but a fast downward arc or edge-case penetration can still push the
        // capsule below the surface. Clamp to the minimum valid Y.
        let min_y = game_core::physics_constants::MIN_CHARACTER_Y;
        if new_pos.y < min_y {
            new_pos.y = min_y;
        }

        // Apply the corrected position to the kinematic body.
        // Use `next_position().rotation` so that a preceding
        // `set_kinematic_rotation` (e.g. WASD-driven facing) is preserved
        // rather than being overwritten by the stale committed rotation.
        let body = self.bodies.get_mut(handle)?;
        let rotation = body.next_position().rotation;
        body.set_next_kinematic_position(Pose::from_parts(new_pos, rotation));

        Some(MoveResult {
            position: game_protocol::types::Vec3f {
                x: new_pos.x,
                y: new_pos.y,
                z: new_pos.z,
            },
            grounded: movement.grounded,
        })
    }

    fn raycast(
        &self,
        origin: game_protocol::types::Vec3f,
        direction: game_protocol::types::Vec3f,
        max_distance: f32,
        ignore_entity: Option<EntityId>,
    ) -> Option<RayHit> {
        let mut filter = QueryFilter::new().groups(collision_groups::targeting_ray_groups());
        if let Some(entity_id) = ignore_entity
            && let Some(&body_handle) = self.entity_to_body.get(&entity_id)
        {
            filter = filter.exclude_rigid_body(body_handle);
        }
        let query = self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.bodies,
            &self.colliders,
            filter,
        );
        let ray = Ray::new(
            Vector::new(origin.x, origin.y, origin.z),
            Vector::new(direction.x, direction.y, direction.z),
        );
        query
            .cast_ray_and_get_normal(&ray, max_distance, true)
            .and_then(|(collider_handle, intersection)| {
                let kind = self.collider_kinds.get(&collider_handle).copied()?;
                let entity = self.entity_for_collider(collider_handle)?;
                let toi = intersection.time_of_impact;
                Some(RayHit {
                    point: game_protocol::types::Vec3f {
                        x: origin.x + direction.x * toi,
                        y: origin.y + direction.y * toi,
                        z: origin.z + direction.z * toi,
                    },
                    normal: game_protocol::types::Vec3f {
                        x: intersection.normal.x,
                        y: intersection.normal.y,
                        z: intersection.normal.z,
                    },
                    toi: intersection.time_of_impact,
                    entity,
                    kind,
                })
            })
    }

    fn line_of_sight(
        &self,
        from: game_protocol::types::Vec3f,
        to: game_protocol::types::Vec3f,
    ) -> bool {
        let dx = to.x - from.x;
        let dy = to.y - from.y;
        let dz = to.z - from.z;
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        if dist < 1e-6 {
            return true; // same point — trivially clear
        }
        let inv = 1.0 / dist;
        let dir = Vector::new(dx * inv, dy * inv, dz * inv);
        let origin = Vector::new(from.x, from.y, from.z);
        let ray = Ray::new(origin, dir);
        // Build a query pipeline filtered to environment-only geometry so entity
        // bodies, hurtboxes, and sensors are transparent to LoS checks.
        let env_filter = QueryFilter::new().groups(collision_groups::kcc_movement_groups());
        let query = self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.bodies,
            &self.colliders,
            env_filter,
        );
        match query.cast_ray_and_get_normal(&ray, dist, true) {
            None => true, // no hit = clear LoS
            Some((_, hit)) => {
                // A contact effectively at the destination is not an occluder.
                // This keeps ground-target casts valid when the ray terminates on
                // or just above the floor plane at long range.
                hit.time_of_impact >= dist - 0.25
            }
        }
    }

    fn cast_to_wall(
        &self,
        from: game_protocol::types::Vec3f,
        to: game_protocol::types::Vec3f,
    ) -> game_protocol::types::Vec3f {
        let dx = to.x - from.x;
        let dy = to.y - from.y;
        let dz = to.z - from.z;
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        if dist < 1e-6 {
            return to; // same point — nothing to cast
        }
        let inv = 1.0 / dist;
        let dir = Vector::new(dx * inv, dy * inv, dz * inv);
        let origin = Vector::new(from.x, from.y, from.z);
        let ray = Ray::new(origin, dir);
        let env_filter = QueryFilter::new().groups(collision_groups::kcc_movement_groups());
        let query = self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.bodies,
            &self.colliders,
            env_filter,
        );
        if let Some((_, hit)) = query.cast_ray_and_get_normal(&ray, dist, true) {
            // Pull back 0.3 units from the contact so the entity doesn't clip geometry.
            let safe_dist = (hit.time_of_impact - 0.3_f32).max(0.0);
            game_protocol::types::Vec3f::new(
                from.x + dir.x * safe_dist,
                from.y + dir.y * safe_dist,
                from.z + dir.z * safe_dist,
            )
        } else {
            to // clear path — use full destination
        }
    }

    fn teleport_entity(
        &mut self,
        entity_id: EntityId,
        position: game_protocol::types::Vec3f,
    ) -> bool {
        let Some(&handle) = self.entity_to_body.get(&entity_id) else {
            return false;
        };
        let Some(body) = self.bodies.get_mut(handle) else {
            return false;
        };
        let rotation = *body.rotation();
        body.set_next_kinematic_position(Pose::from_parts(
            Vector::new(position.x, position.y, position.z),
            rotation,
        ));
        true
    }

    fn add_environment_collider_on_layer(
        &mut self,
        shape: EnvironmentShape,
        position: game_protocol::types::Vec3f,
        layer: u32,
    ) -> u64 {
        let rapier_shape: SharedShape = match shape {
            EnvironmentShape::Cuboid { half_x, half_y, half_z } => {
                SharedShape::cuboid(half_x, half_y, half_z)
            }
            EnvironmentShape::Cylinder { half_height, radius } => {
                SharedShape::cylinder(half_height, radius)
            }
        };
        let col_handle = self.add_environment_collider(
            rapier_shape,
            Vector::new(position.x, position.y, position.z),
        );
        let opaque = self.env_collider_counter;
        self.env_collider_counter += 1;
        self.env_collider_handles.insert(opaque, col_handle);
        self.env_colliders_by_layer.entry(layer).or_default().push(opaque);
        opaque
    }

    fn remove_environment_colliders_by_layer(&mut self, layer: u32) {
        if let Some(handles) = self.env_colliders_by_layer.remove(&layer) {
            for opaque in handles {
                if let Some(col_handle) = self.env_collider_handles.remove(&opaque) {
                    self.collider_kinds.remove(&col_handle);
                    self.colliders.remove(
                        col_handle,
                        &mut self.islands,
                        &mut self.bodies,
                        true,
                    );
                }
            }
        }
    }

    fn set_collider_enabled(&mut self, entity_id: EntityId, enabled: bool) -> bool {
        if let Some(&body_handle) = self.entity_to_body.get(&entity_id) {
            if let Some(body) = self.bodies.get(body_handle) {
                if let Some(&col_handle) = body.colliders().first() {
                    if let Some(collider) = self.colliders.get_mut(col_handle) {
                        collider.set_enabled(enabled);
                        return true;
                    }
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ball_falls_onto_ground() {
        let mut world = PhysicsWorld::new(1.0 / 60.0);
        // Ground plane is created automatically by new().
        let ball = world.add_dynamic_sphere(
            EntityId(1),
            Vector::new(0.0, 5.0, 0.0),
            0.5,
            1.0,
            collision_groups::player_body_groups(),
        );

        // Step enough for ball to fall
        for _ in 0..300 {
            world.step();
        }

        let pos = world.get_body_position(ball).unwrap();
        // Ball (radius 0.5) resting on ground (at y=0, half-extent 0.1)
        // Should be approximately at y = 0.1 + 0.5 = 0.6
        assert!(pos.y > 0.0, "Ball fell through ground: y={}", pos.y);
        assert!(pos.y < 2.0, "Ball didn't fall enough: y={}", pos.y);
    }

    #[test]
    fn raycast_hits_ground() {
        let mut world = PhysicsWorld::new(1.0 / 60.0);
        // Ground plane is created automatically by new().
        world.step(); // need at least one step for broadphase

        let hit = world.raycast(
            Vector::new(0.0, 10.0, 0.0),
            Vector::new(0.0, -1.0, 0.0),
            100.0,
        );

        assert!(hit.is_some(), "Raycast should hit ground");
        let hit = hit.unwrap();
        // Ground is at y=0 with half-extent 0.1, so top surface is at y=0.1
        // Ray starts at y=10, so toi should be about 9.9
        assert!(hit.toi > 9.0, "Hit toi too small: {}", hit.toi);
        assert!(hit.toi < 10.5, "Hit toi too large: {}", hit.toi);
    }

    #[test]
    fn line_of_sight_allows_ground_endpoint() {
        let mut world = PhysicsWorld::new(1.0 / 60.0);
        world.step();

        assert!(world.line_of_sight(
            game_protocol::types::Vec3f::new(0.0, 5.0, 0.0),
            game_protocol::types::Vec3f::new(15.0, 0.1, 0.0),
        ));
    }

    #[test]
    fn sphere_intersection_query() {
        let mut world = PhysicsWorld::new(1.0 / 60.0);
        let entity = world.add_dynamic_sphere(
            EntityId(1),
            Vector::new(5.0, 5.0, 0.0),
            1.0,
            1.0,
            collision_groups::player_body_groups(),
        );
        world.step();

        // Query near the ball — should find it
        let hits = world.intersections_with_sphere(
            Vector::new(5.0, 5.0, 0.0),
            2.0,
        );
        assert!(!hits.is_empty(), "Should find ball in sphere query");

        // Query far away — should find nothing
        let misses = world.intersections_with_sphere(
            Vector::new(50.0, 50.0, 0.0),
            1.0,
        );
        assert!(misses.is_empty(), "Should not find anything far away");

        // Cleanup
        assert!(world.remove_entity(entity));
        assert_eq!(world.body_count(), 0);
    }

    #[test]
    fn kinematic_body_movement() {
        let mut world = PhysicsWorld::new(1.0 / 60.0);
        let entity = world.add_kinematic_capsule(
            EntityId(1),
            Vector::new(0.0, 5.0, 0.0),
            0.5,
            0.3,
            collision_groups::player_body_groups(),
        );

        // Move kinematic body
        let target = Vector::new(10.0, 5.0, 0.0);
        world.set_kinematic_position(entity, target);
        world.step();

        let pos = world.get_body_position(entity).unwrap();
        assert!(
            (pos - target).length() < 0.1,
            "Kinematic body should be near target: {:?}",
            pos
        );
    }

    #[test]
    fn entity_removal() {
        let mut world = PhysicsWorld::new(1.0 / 60.0);
        let e1 = world.add_dynamic_sphere(EntityId(1), Vector::new(0.0, 5.0, 0.0), 0.5, 1.0, collision_groups::player_body_groups());
        let e2 = world.add_dynamic_sphere(EntityId(2), Vector::new(3.0, 5.0, 0.0), 0.5, 1.0, collision_groups::npc_body_groups());
        assert_eq!(world.body_count(), 2);

        assert!(world.remove_entity(e1));
        assert_eq!(world.body_count(), 1);

        // Double removal returns false
        assert!(!world.remove_entity(e1));

        // e2 still works
        assert!(world.get_body_position(e2).is_some());
    }
}
