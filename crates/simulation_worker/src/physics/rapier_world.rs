use rapier3d::prelude::*;
use rapier3d::math::{Pose, Vector};
use std::collections::HashMap;
use std::sync::mpsc;
use game_protocol::entity_id::EntityId;
use game_core::physics_backend::PhysicsBackend;
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
}

/// A collision event resolved to entity IDs.
#[derive(Clone, Debug)]
pub struct EntityCollisionEvent {
    pub entity1: EntityId,
    pub entity2: EntityId,
    pub started: bool,
    pub is_sensor: bool,
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
        let mut params = IntegrationParameters::default();
        params.dt = dt;

        let (collision_send, collision_recv) = mpsc::channel();
        let (contact_force_send, contact_force_recv) = mpsc::channel();

        Self {
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
        }
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
    pub fn drain_collision_events(&self) -> Vec<EntityCollisionEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.collision_recv.try_recv() {
            let entity1 = self.entity_for_collider(event.collider1());
            let entity2 = self.entity_for_collider(event.collider2());
            if let (Some(e1), Some(e2)) = (entity1, entity2) {
                events.push(EntityCollisionEvent {
                    entity1: e1,
                    entity2: e2,
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
        let body_handle = collider.parent()?;
        self.body_to_entity.get(&body_handle).copied()
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

    // ── Body creation ───────────────────────────────────────────────

    /// Add a static ground plane at y=0.
    pub fn add_static_ground(&mut self, entity_id: EntityId) -> EntityId {

        let body = RigidBodyBuilder::fixed()
            .translation(Vector::new(0.0, 0.0, 0.0))
            .build();
        let body_handle = self.bodies.insert(body);

        let collider = ColliderBuilder::cuboid(100.0, 0.1, 100.0)
            .collision_groups(collision_groups::environment_groups())
            .build();
        self.colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);

        self.entity_to_body.insert(entity_id, body_handle);
        self.body_to_entity.insert(body_handle, entity_id);

        entity_id
    }

    /// Add a dynamic sphere body at the given position.
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
        self.colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);

        self.entity_to_body.insert(entity_id, body_handle);
        self.body_to_entity.insert(body_handle, entity_id);

        entity_id
    }

    /// Add a dynamic capsule body (useful for player/NPC characters).
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
        self.colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);

        self.entity_to_body.insert(entity_id, body_handle);
        self.body_to_entity.insert(body_handle, entity_id);

        entity_id
    }

    /// Add a kinematic body (server-controlled movement, not physics-driven).
    /// Used for: flight, scripted movement, elevators.
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

        let collider = ColliderBuilder::capsule_y(half_height, radius)
            .collision_groups(groups)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        self.colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);

        self.entity_to_body.insert(entity_id, body_handle);
        self.body_to_entity.insert(body_handle, entity_id);

        entity_id
    }

    /// Add a sensor collider attached to an existing body.
    /// Used for: skill hitboxes, trigger zones, auras.
    /// Sensors detect overlap but produce no physical contact forces.
    pub fn add_sensor_to_entity(
        &mut self,
        entity_id: EntityId,
        shape: SharedShape,
        offset: Pose,
        groups: InteractionGroups,
    ) -> Option<ColliderHandle> {
        let body_handle = *self.entity_to_body.get(&entity_id)?;

        let collider = ColliderBuilder::new(shape)
            .position(offset)
            .sensor(true)
            .collision_groups(groups)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .build();
        let handle = self.colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);
        Some(handle)
    }

    /// Remove a specific collider (e.g. a skill hitbox sensor).
    pub fn remove_collider(&mut self, handle: ColliderHandle) {
        self.colliders.remove(handle, &mut self.islands, &mut self.bodies, true);
    }

    // ── Body removal ────────────────────────────────────────────

    /// Remove an entity and its physics body/colliders from the world.
    pub fn remove_entity(&mut self, entity_id: EntityId) -> bool {
        if let Some(body_handle) = self.entity_to_body.remove(&entity_id) {
            self.body_to_entity.remove(&body_handle);
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

    /// Set the position of a kinematic body.
    pub fn set_kinematic_position(
        &mut self,
        entity_id: EntityId,
        position: Vector,
    ) -> bool {
        if let Some(handle) = self.entity_to_body.get(&entity_id) {
            if let Some(body) = self.bodies.get_mut(*handle) {
                let rotation = *body.rotation();
                body.set_next_kinematic_position(Pose::from_parts(
                    position,
                    rotation,
                ));
                return true;
            }
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
    /// Useful for AoE abilities, aura pulses, proximity checks.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ball_falls_onto_ground() {
        let mut world = PhysicsWorld::new(1.0 / 60.0);
        world.add_static_ground(EntityId(1));
        let ball = world.add_dynamic_sphere(
            EntityId(2),
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
        world.add_static_ground(EntityId(1));
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
