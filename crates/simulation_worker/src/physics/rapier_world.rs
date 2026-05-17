use super::collision_groups;
use game_core::physics_backend::{
    ColliderKind, CollisionEvent as GameCollisionEvent, EnvironmentShape, MoveResult,
    PhysicsBackend, RayHit,
};
use game_protocol::entity_id::EntityId;
use game_schema::EntityKind;
use rapier3d::control::{CharacterAutostep, CharacterLength, KinematicCharacterController};
use rapier3d::math::{Pose, Vector};
use rapier3d::parry::shape::{TriMesh, TriMeshFlags};
use rapier3d::parry::utils::Array2;
use rapier3d::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc;

// ── Collider user_data encoding ─────────────────────────────────────────
//
// Rapier's `Collider::user_data: u128` is stamped on every collider so that
// scene-query predicates and contact resolution can read entity metadata
// without any HashMap lookups.
//
// Layout (little-endian order inside the u128):
//   bits  0..31  — layer_id        (u32)  dungeon/instance isolation
//   bits 32..47  — (unused)        (16 bits) available for future flags
//   bits 48..50  — collider_kind   (3 bits)  Body=0, Hurtbox=1, Hitbox=2, BlockCone=3
//   bits 51..63  — flags           (13 bits) future: phased, stealthed, etc.
//   bits 64..95  — entity_id.0     (u32)  owning entity (0 = environment)
//   bits 96..127 — reserved        (u32)  expansion
//
// Team membership is tracked in the dense `entity_team_cache` (full u32),
// not in user_data.  A 1-bit stealth flag can be added to the flags field
// when that feature is implemented.
//
// Layer semantics (strict — no shared layer):
//   Every collider belongs to exactly one layer. Layer 0 is the open-world
//   layer; layers 1..=N are dungeon instances (or future open-world maps).
//   There is NO cross-layer visibility — environment authored on layer A is
//   invisible to scene queries issued by an entity on layer B.
//
//   Each layer must author its own floor. The placeholder open-world floor
//   lives on layer 0 and is visible only to layer-0 entities; once the voxel
//   terrain pipeline lands it will be replaced by per-layer baked TriMesh
//   chunks loaded from `terrain_chunk` rows.
//
// Predicate: `col_layer == caller_layer`
//
// ColliderKind discriminant:
//   Body and Hurtbox (discriminant 0, 1) are fully decoded from user_data.
//   Hitbox and BlockCone (discriminant 2, 3) carry payload data that doesn't
//   fit in user_data — for those, `collider_kinds` HashMap is the fallback.

const UD_KIND_SHIFT: u32 = 48;
const UD_KIND_MASK: u128 = 0x7; // 3 bits
const UD_ENTITY_SHIFT: u32 = 64;
const UD_ENTITY_MASK: u128 = 0xFFFF_FFFF; // 32 bits
const UD_KIND_BODY: u128 = 0;
const UD_KIND_HURTBOX: u128 = 1;
const UD_KIND_HITBOX: u128 = 2;
const UD_KIND_BLOCKCONE: u128 = 3;
const UD_KIND_VOLUME: u128 = 4;

/// Extract the layer from a collider's user_data.
#[inline(always)]
fn ud_layer(user_data: u128) -> u32 {
    user_data as u32
}

/// Encode a layer into user_data (preserving upper bits).
#[inline(always)]
fn ud_set_layer(user_data: u128, layer: u32) -> u128 {
    (user_data & !0xFFFF_FFFFu128) | layer as u128
}

/// Extract the owning EntityId from user_data. Returns `None` for environment
/// colliders (entity_id.0 == 0).
#[inline(always)]
fn ud_entity_id(user_data: u128) -> Option<EntityId> {
    let raw = ((user_data >> UD_ENTITY_SHIFT) & UD_ENTITY_MASK) as u64;
    if raw == 0 { None } else { Some(EntityId(raw)) }
}

/// Encode an EntityId into user_data (preserving other fields).
#[inline(always)]
fn ud_set_entity_id(user_data: u128, entity_id: EntityId) -> u128 {
    let cleared = user_data & !(UD_ENTITY_MASK << UD_ENTITY_SHIFT);
    cleared | ((entity_id.0 as u128) << UD_ENTITY_SHIFT)
}

/// Extract the ColliderKind discriminant from user_data.
/// Returns the full `ColliderKind` for Body/Hurtbox. For Hitbox/BlockCone
/// returns `None` (caller must fall back to the `collider_kinds` HashMap).
#[inline(always)]
fn ud_collider_kind(user_data: u128) -> Option<ColliderKind> {
    match (user_data >> UD_KIND_SHIFT) & UD_KIND_MASK {
        UD_KIND_BODY => Some(ColliderKind::Body),
        UD_KIND_HURTBOX => Some(ColliderKind::Hurtbox),
        // Hitbox/BlockCone carry payload not in user_data — fallback required.
        _ => None,
    }
}

/// Encode a ColliderKind discriminant into user_data (preserving other fields).
#[inline(always)]
fn ud_set_kind(user_data: u128, kind: &ColliderKind) -> u128 {
    let disc = match kind {
        ColliderKind::Body => UD_KIND_BODY,
        ColliderKind::Hurtbox => UD_KIND_HURTBOX,
        ColliderKind::Hitbox(_) => UD_KIND_HITBOX,
        ColliderKind::BlockCone(_) => UD_KIND_BLOCKCONE,
        ColliderKind::Volume(_) => UD_KIND_VOLUME,
    };
    let cleared = user_data & !(UD_KIND_MASK << UD_KIND_SHIFT);
    cleared | (disc << UD_KIND_SHIFT)
}

/// Stamp entity_id + collider_kind into user_data in a single call.
#[inline(always)]
fn ud_stamp(user_data: u128, entity_id: EntityId, kind: &ColliderKind) -> u128 {
    ud_set_entity_id(ud_set_kind(user_data, kind), entity_id)
}

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

    // Disabled character body pool — bodies with colliders intact but disabled.
    // Keyed by EntityKind so NPC bodies are reused for NPCs, etc.
    // All character kinds currently share identical capsule geometry (0.5, 0.3).
    disabled_pool: Vec<(RigidBodyHandle, EntityKind)>,

    // Per-entity visibility layer — cached from authoritative entity_regions.
    // Used by scene-query predicates to filter cross-layer interactions.
    entity_layers: HashMap<EntityId, u32>,

    // Per-layer collision policies — controls entity-kind interaction rules.
    // Keyed by layer. Layer 0 (open world) uses default when absent.
    layer_policies: HashMap<u32, game_schema::LayerCollisionPolicy>,
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
        let params = IntegrationParameters {
            dt,
            ..Default::default()
        };

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
            collider_kinds: HashMap::new(),
            sensor_handle_counter: 0,
            sensor_handles: HashMap::new(),
            world_sensor_owners: HashMap::new(),
            env_collider_counter: 0,
            env_collider_handles: HashMap::new(),
            env_colliders_by_layer: HashMap::new(),
            disabled_pool: Vec::new(),
            entity_layers: HashMap::new(),
            layer_policies: HashMap::new(),
        };

        // Default placeholder floor on layer 0. This is a *convenience
        // default* so test fixtures (`PhysicsWorld::new()` followed by
        // ad-hoc collider inserts) and out-of-the-box worker startup
        // both have a sane ground plane to fall onto. Production worker
        // startup overrides this by calling
        // `replace_layer_geometry(0, …)` from `materialize_layer` whenever
        // `data/layers.ron` declares a `WorldLayerDef` for layer 0
        // (see `crates/simulation_worker/src/coordinator.rs`). Strict
        // same-layer scene queries still apply: this floor is invisible
        // to entities on any layer ≠ 0.
        world.add_environment_collider_on_layer(
            EnvironmentShape::Cuboid {
                half_x: 500.0,
                half_y: 0.1,
                half_z: 500.0,
            },
            game_protocol::types::Vec3f {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            0,
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
    /// Primary path: extract from user_data (O(1) bit shift, no HashMap).
    /// Returns `None` for environment colliders (entity_id == 0 in user_data).
    fn entity_for_collider(&self, collider_handle: ColliderHandle) -> Option<EntityId> {
        let collider = self.colliders.get(collider_handle)?;
        if let Some(eid) = ud_entity_id(collider.user_data) {
            return Some(eid);
        }
        // Parentless sensors without user_data stamp (legacy fallback).
        self.world_sensor_owners.get(&collider_handle).copied()
    }

    /// Resolve a collider handle to its `ColliderKind`.
    /// Primary path: extract discriminant from user_data (O(1) bit shift).
    /// For Hitbox/BlockCone (which carry payload), falls back to `collider_kinds` HashMap.
    /// Defaults to `Body` for environment colliders or unregistered colliders.
    fn kind_for_collider(&self, handle: ColliderHandle) -> ColliderKind {
        if let Some(collider) = self.colliders.get(handle) {
            if let Some(kind) = ud_collider_kind(collider.user_data) {
                return kind;
            }
        }
        // Hitbox/BlockCone or unstamped — full lookup.
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
            .user_data(ud_stamp(0, entity_id, &ColliderKind::Body))
            .build();
        let ch = self
            .colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);
        self.collider_kinds.insert(ch, ColliderKind::Body);

        // Hurtbox sensor — same shape, sensor-only, hurtbox collision group.
        let hurtbox = ColliderBuilder::ball(radius)
            .sensor(true)
            .collision_groups(collision_groups::skill_hurtbox_groups())
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .user_data(ud_stamp(0, entity_id, &ColliderKind::Hurtbox))
            .build();
        let hch = self
            .colliders
            .insert_with_parent(hurtbox, body_handle, &mut self.bodies);
        self.collider_kinds.insert(hch, ColliderKind::Hurtbox);

        self.entity_to_body.insert(entity_id, body_handle);

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
            .user_data(ud_stamp(0, entity_id, &ColliderKind::Body))
            .build();
        let ch = self
            .colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);
        self.collider_kinds.insert(ch, ColliderKind::Body);

        // Hurtbox sensor — same capsule shape, sensor-only.
        let hurtbox = ColliderBuilder::capsule_y(half_height, radius)
            .sensor(true)
            .collision_groups(collision_groups::skill_hurtbox_groups())
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .user_data(ud_stamp(0, entity_id, &ColliderKind::Hurtbox))
            .build();
        let hch = self
            .colliders
            .insert_with_parent(hurtbox, body_handle, &mut self.bodies);
        self.collider_kinds.insert(hch, ColliderKind::Hurtbox);

        self.entity_to_body.insert(entity_id, body_handle);

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
            .user_data(ud_stamp(0, entity_id, &ColliderKind::Body))
            .build();
        let ch = self
            .colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);
        self.collider_kinds.insert(ch, ColliderKind::Body);

        // Hurtbox sensor — same capsule shape, sensor-only.
        // Also needs KINEMATIC_KINEMATIC for the same reason as the body collider above.
        let hurtbox = ColliderBuilder::capsule_y(half_height, radius)
            .sensor(true)
            .collision_groups(collision_groups::skill_hurtbox_groups())
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .active_collision_types(active_types)
            .user_data(ud_stamp(0, entity_id, &ColliderKind::Hurtbox))
            .build();
        let hch = self
            .colliders
            .insert_with_parent(hurtbox, body_handle, &mut self.bodies);
        self.collider_kinds.insert(hch, ColliderKind::Hurtbox);

        self.entity_to_body.insert(entity_id, body_handle);

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
        let layer = self.entity_layer(entity_id);
        let base = ud_set_layer(0, layer);
        let collider = ColliderBuilder::new(shape)
            .position(offset)
            .sensor(true)
            .collision_groups(groups)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .active_collision_types(active_types)
            .user_data(ud_stamp(base, entity_id, &kind))
            .build();
        let handle = self
            .colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);
        self.collider_kinds.insert(handle, kind);
        Some(handle)
    }

    /// Remove a specific collider (e.g. a skill hitbox sensor).
    pub fn remove_collider(&mut self, handle: ColliderHandle) {
        self.collider_kinds.remove(&handle);
        self.world_sensor_owners.remove(&handle);
        self.colliders
            .remove(handle, &mut self.islands, &mut self.bodies, true);
    }

    // ── Body removal ────────────────────────────────────────────

    /// Remove an entity and its physics body/colliders from the world.
    pub fn remove_entity(&mut self, entity_id: EntityId) -> bool {
        if let Some(body_handle) = self.entity_to_body.remove(&entity_id) {
            self.entity_layers.remove(&entity_id);
            // Clean up collider metadata for all colliders attached to this body.
            // Use body.colliders() for O(attached) instead of scanning all colliders.
            let attached: Vec<ColliderHandle> = self
                .bodies
                .get(body_handle)
                .map(|b| b.colliders().to_vec())
                .unwrap_or_default();
            let attached_set: std::collections::HashSet<ColliderHandle> =
                attached.iter().copied().collect();

            // Remove any opaque sensor handles pointing at colliders attached to this body
            // so the internal `sensor_handles` map does not grow unbounded.
            self.sensor_handles
                .retain(|_, ch| !attached_set.contains(ch));

            for ch in &attached {
                self.collider_kinds.remove(ch);
            }

            // Clean up world-space sensor ownership entries for this entity so the
            // map does not leak references to removed entities over long sessions.
            self.world_sensor_owners
                .retain(|_, &mut owner| owner != entity_id);
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

    /// Disable a character body and pool it for reuse instead of destroying it.
    ///
    /// Removes the entity from lookup maps and disables the rigid body and all
    /// attached colliders so they no longer participate in broadphase queries or
    /// collision detection. The body+colliders remain allocated in Rapier's sets
    /// and can be re-enabled cheaply by `reuse_or_spawn_character`.
    ///
    /// Returns `true` if the entity was found and pooled.
    pub fn disable_entity(&mut self, entity_id: EntityId, kind: EntityKind) -> bool {
        if let Some(body_handle) = self.entity_to_body.remove(&entity_id) {
            self.entity_layers.remove(&entity_id);

            // Clean up sensor and collider-kind metadata exactly as remove_entity does,
            // but keep the body+colliders alive in Rapier.
            let attached: Vec<ColliderHandle> = self
                .bodies
                .get(body_handle)
                .map(|b| b.colliders().to_vec())
                .unwrap_or_default();
            let attached_set: HashSet<ColliderHandle> = attached.iter().copied().collect();
            self.sensor_handles
                .retain(|_, ch| !attached_set.contains(ch));
            for ch in &attached {
                self.collider_kinds.remove(ch);
            }
            self.world_sensor_owners
                .retain(|_, &mut owner| owner != entity_id);

            // Disable body — removes from island/broadphase, zero cost per step.
            if let Some(body) = self.bodies.get_mut(body_handle) {
                body.set_enabled(false);
            }
            // Disable all attached colliders so they don't appear in any query.
            for &ch in &attached {
                if let Some(collider) = self.colliders.get_mut(ch) {
                    collider.set_enabled(false);
                }
            }

            self.disabled_pool.push((body_handle, kind));
            true
        } else {
            false
        }
    }

    /// Try to reuse a pooled character body, or fall back to creating a fresh one.
    ///
    /// Looks for a disabled body matching `kind` in the pool. If found, re-enables
    /// it, resets position/velocity, updates collision groups for the target kind,
    /// and maps it to the new entity. If the pool is empty for that kind, delegates
    /// to `add_kinematic_capsule` for a fresh allocation.
    ///
    /// Returns `true` if the entity now has a body (always succeeds).
    pub fn reuse_or_spawn_character(
        &mut self,
        entity_id: EntityId,
        position: Vector,
        kind: EntityKind,
    ) -> bool {
        if self.entity_to_body.contains_key(&entity_id) {
            return false;
        }

        // Find a pooled body matching this kind (pop from back for O(1)).
        let pool_idx = self.disabled_pool.iter().rposition(|(_, k)| *k == kind);
        if let Some(idx) = pool_idx {
            let (body_handle, _) = self.disabled_pool.swap_remove(idx);

            // Re-enable body and reset its state.
            if let Some(body) = self.bodies.get_mut(body_handle) {
                body.set_enabled(true);
                body.set_next_kinematic_position(Pose::from_parts(position, Default::default()));
                body.set_linvel(Vector::new(0.0, 0.0, 0.0), false);
                body.set_angvel(Vector::new(0.0, 0.0, 0.0), false);
                body.reset_forces(false);
            }

            // Re-enable all attached colliders and restore collider_kinds metadata.
            let attached: Vec<ColliderHandle> = self
                .bodies
                .get(body_handle)
                .map(|b| b.colliders().to_vec())
                .unwrap_or_default();

            let groups = match kind {
                EntityKind::Player => collision_groups::player_body_groups(),
                EntityKind::Npc | EntityKind::Boss => collision_groups::npc_body_groups(),
                _ => collision_groups::player_body_groups(),
            };

            for (i, &ch) in attached.iter().enumerate() {
                if let Some(collider) = self.colliders.get_mut(ch) {
                    collider.set_enabled(true);
                    // First collider = Body, second = Hurtbox (per add_kinematic_capsule layout).
                    let ck = if i == 0 {
                        ColliderKind::Body
                    } else {
                        ColliderKind::Hurtbox
                    };
                    if i == 0 {
                        collider.set_collision_groups(groups);
                    } else {
                        collider.set_collision_groups(collision_groups::skill_hurtbox_groups());
                    }
                    // Re-stamp user_data with new entity_id + kind (clears stale bits).
                    collider.user_data = ud_stamp(collider.user_data, entity_id, &ck);
                    self.collider_kinds.insert(ch, ck);
                }
            }

            self.entity_to_body.insert(entity_id, body_handle);
            true
        } else {
            // Pool empty for this kind — create fresh.
            let groups = match kind {
                EntityKind::Player => collision_groups::player_body_groups(),
                EntityKind::Npc | EntityKind::Boss => collision_groups::npc_body_groups(),
                _ => collision_groups::player_body_groups(),
            };
            self.add_kinematic_capsule(entity_id, position, 0.5, 0.3, groups);
            true
        }
    }

    /// Remove excess pooled bodies to bound memory usage.
    ///
    /// Keeps at most `max_idle` bodies in the pool; fully removes the rest
    /// from Rapier's body/collider sets. Call after bulk despawn ticks.
    pub fn drain_pool(&mut self, max_idle: usize) {
        while self.disabled_pool.len() > max_idle {
            let (handle, _) = self.disabled_pool.pop().unwrap();
            // Fully destroy the body and its colliders.
            self.bodies.remove(
                handle,
                &mut self.islands,
                &mut self.colliders,
                &mut self.impulse_joints,
                &mut self.multibody_joints,
                true,
            );
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
    pub fn set_kinematic_position(&mut self, entity_id: EntityId, position: Vector) -> bool {
        if let Some(handle) = self.entity_to_body.get(&entity_id)
            && let Some(body) = self.bodies.get_mut(*handle)
        {
            // Preserve any pending rotation set earlier in this tick.
            let rotation = body.next_position().rotation;
            body.set_next_kinematic_position(Pose::from_parts(position, rotation));
            return true;
        }
        false
    }

    /// Set the rotation of a kinematic body, preserving the queued position.
    ///
    /// Uses `body.next_position()` so that a prior `set_next_kinematic_position`
    /// from `move_character` (e.g. arc movement) is not overwritten by resetting
    /// the position back to the last-committed `translation()`.
    pub fn set_kinematic_rotation(&mut self, entity_id: EntityId, rotation: Rotation) -> bool {
        if let Some(handle) = self.entity_to_body.get(&entity_id)
            && let Some(body) = self.bodies.get_mut(*handle)
        {
            let position = body.next_position().translation;
            body.set_next_kinematic_position(Pose::from_parts(position, rotation));
            return true;
        }
        false
    }

    // ── Scene queries ───────────────────────────────────────────

    /// Cast a ray and return the first hit.
    pub fn raycast(&self, origin: Vector, direction: Vector, max_toi: f32) -> Option<RaycastHit> {
        let query = self.query_pipeline();
        let ray = Ray::new(origin, direction);
        query
            .cast_ray_and_get_normal(&ray, max_toi, true)
            .map(|(collider, intersection)| RaycastHit {
                collider,
                toi: intersection.time_of_impact,
                normal: intersection.normal,
            })
    }

    /// Find all colliders intersecting a sphere at the given position.
    /// Useful for AoE abilities, proximity checks.
    pub fn intersections_with_sphere(&self, center: Vector, radius: f32) -> Vec<ColliderHandle> {
        let query = self.query_pipeline();
        let shape = Ball::new(radius);
        let shape_pos = Pose::translation(center.x, center.y, center.z);

        query
            .intersect_shape(shape_pos, &shape)
            .map(|(handle, _)| handle)
            .collect()
    }

    // ── Accessors for advanced usage ────────────────────────────

    pub fn body_count(&self) -> usize {
        self.bodies.len()
    }

    pub fn has_entity_body(&self, entity_id: EntityId) -> bool {
        self.entity_to_body.contains_key(&entity_id)
    }

    pub fn pool_size(&self) -> usize {
        self.disabled_pool.len()
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
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

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
            && let Some(body) = self.bodies.get_mut(*handle)
        {
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
            SensorShape::Capsule {
                half_height,
                radius,
            } => SharedShape::capsule_y(half_height, radius),
        };
        let groups = match kind {
            ColliderKind::Hitbox(_) => collision_groups::skill_hitbox_groups(),
            ColliderKind::Hurtbox => collision_groups::skill_hurtbox_groups(),
            _ => collision_groups::skill_hitbox_groups(),
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
            SensorShape::Capsule {
                half_height,
                radius,
            } => SharedShape::capsule_y(half_height, radius),
        };
        let groups = match kind {
            ColliderKind::Hitbox(_) => collision_groups::skill_hitbox_groups(),
            ColliderKind::Hurtbox => collision_groups::skill_hurtbox_groups(),
            _ => collision_groups::skill_hitbox_groups(),
        };
        let active_types = ActiveCollisionTypes::default()
            | ActiveCollisionTypes::KINEMATIC_KINEMATIC
            | ActiveCollisionTypes::KINEMATIC_FIXED;
        let layer = self.entity_layer(owner);
        let base = ud_set_layer(0, layer);
        let collider = ColliderBuilder::new(rapier_shape)
            .translation(Vector::new(position.x, position.y, position.z))
            .sensor(true)
            .collision_groups(groups)
            .active_events(ActiveEvents::COLLISION_EVENTS)
            .active_collision_types(active_types)
            .user_data(ud_stamp(base, owner, &kind))
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
        // Layer isolation: only include candidate bodies/hurtboxes whose
        // stamped layer exactly matches the sensor's owner layer. Strict
        // same-layer means a sensor on layer N never sees entities on any
        // other layer (open world or otherwise). Combined with the strict
        // environment-query predicates, this gives full per-layer isolation
        // without any layer-0 fallthrough.
        let sensor_layer = ud_layer(sensor.user_data);
        let query = self.query_pipeline();

        // Use a direct shape query against the current scene instead of
        // transition events so stationary occupants are still detected.
        let mut entities = HashSet::new();
        for (other, _) in query.intersect_shape(sensor_pose, sensor_shape) {
            if other == sensor_handle {
                continue;
            }
            // Strict same-layer match on candidate collider.
            let Some(other_col) = self.colliders.get(other) else {
                continue;
            };
            if ud_layer(other_col.user_data) != sensor_layer {
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
            EntityKind::Projectile | EntityKind::Hazard | EntityKind::Prop => {
                collision_groups::player_body_groups()
            }
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
            .user_data(ud_stamp(0, entity_id, &ColliderKind::Body))
            .build();
        let ch = self
            .colliders
            .insert_with_parent(collider, body_handle, &mut self.bodies);
        self.collider_kinds.insert(ch, ColliderKind::Body);

        self.entity_to_body.insert(entity_id, body_handle);
        true
    }

    fn move_character(
        &mut self,
        entity_id: EntityId,
        desired_translation: game_protocol::types::Vec3f,
    ) -> Option<MoveResult> {
        const KCC_OFFSET_REL: f32 = 0.02;

        let handle = *self.entity_to_body.get(&entity_id)?;
        let desired_vec = Vector::new(
            desired_translation.x,
            desired_translation.y,
            desired_translation.z,
        );
        let caller_layer = self.entity_layers.get(&entity_id).copied().unwrap_or(0);

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
            //
            // Layer predicate: only collide with same-layer colliders. Reads the
            // layer from collider.user_data — zero HashMap lookups. Strict
            // same-layer means each layer must author its own floor; there is
            // no layer-0 fallback.
            let layer_pred = move |_ch: ColliderHandle, collider: &Collider| -> bool {
                let col_layer = ud_layer(collider.user_data);
                col_layer == caller_layer
            };
            let filter = QueryFilter::default()
                .exclude_rigid_body(handle)
                .groups(collision_groups::kcc_movement_groups())
                .predicate(&layer_pred);
            let queries = self.broad_phase.as_query_pipeline(
                self.narrow_phase.query_dispatcher(),
                &self.bodies,
                &self.colliders,
                filter,
            );
            let controller = KinematicCharacterController {
                offset: CharacterLength::Relative(KCC_OFFSET_REL),
                autostep: Some(CharacterAutostep {
                    max_height: CharacterLength::Relative(0.15),
                    min_width: CharacterLength::Relative(0.2),
                    include_dynamic_bodies: false,
                }),
                snap_to_ground: Some(CharacterLength::Relative(0.2)),
                normal_nudge_factor: 1.0e-3,
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
        let mut grounded = movement.grounded;

        // For walk-style motion (ground-pull or horizontal shoves), repair small
        // false-negative grounded results by snapping to the same-layer surface
        // directly below when it is already within walk distance. This keeps
        // authored floors and flat TriMesh terrain stable without introducing
        // a global minimum-Y clamp or affecting true airborne movement.
        if (-0.25..=1.0e-5).contains(&desired_vec.y) {
            const SURFACE_PROBE_UP: f32 = 1.0;
            const SURFACE_PROBE_DISTANCE: f32 = 2.0;
            const MAX_GROUNDED_SNAP_DELTA: f32 = 0.35;

            if let Some(hit) = self.raycast_surface(
                game_protocol::types::Vec3f::new(
                    new_pos.x,
                    new_pos.y + SURFACE_PROBE_UP,
                    new_pos.z,
                ),
                game_protocol::types::Vec3f::new(0.0, -1.0, 0.0),
                SURFACE_PROBE_DISTANCE,
                caller_layer,
            ) {
                let rest_y = hit.y
                    + game_core::physics_constants::CAPSULE_HALF_HEIGHT
                    + game_core::physics_constants::CAPSULE_RADIUS
                    + (game_core::physics_constants::CAPSULE_HALF_HEIGHT
                        + game_core::physics_constants::CAPSULE_RADIUS)
                        * 2.0
                        * KCC_OFFSET_REL;
                let snap_delta = rest_y - new_pos.y;
                if snap_delta.abs() <= MAX_GROUNDED_SNAP_DELTA {
                    new_pos.y = rest_y;
                    grounded = true;
                }
            }
        }

        // No global Y clamp here. Each layer's authored geometry (or the
        // layer-0 placeholder cuboid) is the only thing that should stop
        // a falling character. Entities on layers without ground fall
        // indefinitely — game logic must add OOB / kill-volume handling
        // where appropriate (§4.8b).

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
            grounded,
        })
    }

    fn raycast(
        &self,
        origin: game_protocol::types::Vec3f,
        direction: game_protocol::types::Vec3f,
        max_distance: f32,
        ignore_entity: Option<EntityId>,
    ) -> Option<RayHit> {
        // Determine the caller's layer from ignore_entity (the caster).
        let caller_layer = ignore_entity
            .and_then(|eid| self.entity_layers.get(&eid).copied())
            .unwrap_or(0);
        let layer_pred = move |_ch: ColliderHandle, collider: &Collider| -> bool {
            let col_layer = ud_layer(collider.user_data);
            col_layer == caller_layer
        };
        let mut filter = QueryFilter::new()
            .groups(collision_groups::targeting_ray_groups())
            .predicate(&layer_pred);
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

    fn raycast_surface(
        &self,
        origin: game_protocol::types::Vec3f,
        direction: game_protocol::types::Vec3f,
        max_distance: f32,
        layer: u32,
    ) -> Option<game_protocol::types::Vec3f> {
        let len_sq =
            direction.x * direction.x + direction.y * direction.y + direction.z * direction.z;
        if !len_sq.is_finite() || len_sq < 1e-12 {
            return None;
        }
        let inv = 1.0 / len_sq.sqrt();
        let dir = Vector::new(direction.x * inv, direction.y * inv, direction.z * inv);
        let ray = Ray::new(Vector::new(origin.x, origin.y, origin.z), dir);
        // Strict same-layer environment query. Each layer (open world or
        // dungeon) must author its own floor; there is no shared layer-0
        // fallback. Templates that previously relied on the implicit
        // open-world floor must now declare their own surface (heightfield,
        // cuboid, or future TriMesh).
        let layer_pred = move |_ch: ColliderHandle, collider: &Collider| -> bool {
            let col_layer = ud_layer(collider.user_data);
            col_layer == layer
        };
        let env_filter = QueryFilter::new()
            .groups(collision_groups::kcc_movement_groups())
            .predicate(&layer_pred);
        let query = self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.bodies,
            &self.colliders,
            env_filter,
        );
        let (_, hit) = query.cast_ray_and_get_normal(&ray, max_distance, true)?;
        let toi = hit.time_of_impact;
        Some(game_protocol::types::Vec3f {
            x: origin.x + dir.x * toi,
            y: origin.y + dir.y * toi,
            z: origin.z + dir.z * toi,
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
            EnvironmentShape::Cuboid {
                half_x,
                half_y,
                half_z,
            } => SharedShape::cuboid(half_x, half_y, half_z),
            EnvironmentShape::Cylinder {
                half_height,
                radius,
            } => SharedShape::cylinder(half_height, radius),
            EnvironmentShape::Heightfield {
                nrows,
                ncols,
                scale_x,
                scale_y,
                scale_z,
                heights,
            } => {
                // Length mismatches would panic inside Array2::new — guard
                // with a clear error and fall back to a 1×1 flat cuboid so
                // instance spawn keeps going (ground plane stays intact).
                if heights.len() != nrows * ncols || nrows < 2 || ncols < 2 {
                    log::error!(
                        "heightfield shape has inconsistent dims (rows={nrows}, cols={ncols}, \
                         heights.len()={}) — substituting 1m flat filler",
                        heights.len()
                    );
                    SharedShape::cuboid(0.5, 0.05, 0.5)
                } else {
                    let heights_mat = Array2::new(nrows, ncols, heights);
                    SharedShape::heightfield(heights_mat, Vector::new(scale_x, scale_y, scale_z))
                }
            }
            EnvironmentShape::TriMesh { vertices, indices } => {
                // Validate flat layouts: vertices = [x,y,z, ...], indices =
                // [i0,i1,i2, ...]. Out-of-bounds indices would panic inside
                // parry's trimesh ctor; guard with a clear error and fall
                // back to a 1 m flat filler so layer materialisation keeps
                // going (other geometry on the layer is unaffected).
                let vert_count = vertices.len() / 3;
                let tri_count = indices.len() / 3;
                let max_index = indices.iter().copied().max().unwrap_or(0) as usize;
                if vertices.len() % 3 != 0
                    || indices.len() % 3 != 0
                    || vert_count < 3
                    || tri_count < 1
                    || max_index >= vert_count
                {
                    log::error!(
                        "trimesh shape has invalid layout (vertices.len()={}, indices.len()={}, \
                         max_index={max_index}) — substituting 1m flat filler",
                        vertices.len(),
                        indices.len(),
                    );
                    SharedShape::cuboid(0.5, 0.05, 0.5)
                } else {
                    let points: Vec<Vector> = vertices
                        .chunks_exact(3)
                        .map(|c| Vector::new(c[0], c[1], c[2]))
                        .collect();
                    let tris: Vec<[u32; 3]> = indices
                        .chunks_exact(3)
                        .map(|c| [c[0], c[1], c[2]])
                        .collect();
                    match TriMesh::with_flags(points, tris, TriMeshFlags::FIX_INTERNAL_EDGES) {
                        Ok(mesh) => SharedShape::new(mesh),
                        Err(e) => {
                            log::error!(
                                "trimesh build failed ({e:?}) — substituting 1m flat filler"
                            );
                            SharedShape::cuboid(0.5, 0.05, 0.5)
                        }
                    }
                }
            }
        };
        let col_handle = self.add_environment_collider(
            rapier_shape,
            Vector::new(position.x, position.y, position.z),
        );
        // Stamp layer into collider user_data for zero-cost predicate reads.
        if let Some(col) = self.colliders.get_mut(col_handle) {
            col.user_data = ud_set_layer(col.user_data, layer);
        }
        let opaque = self.env_collider_counter;
        self.env_collider_counter += 1;
        self.env_collider_handles.insert(opaque, col_handle);
        self.env_colliders_by_layer
            .entry(layer)
            .or_default()
            .push(opaque);
        opaque
    }

    fn remove_environment_colliders_by_layer(&mut self, layer: u32) {
        if let Some(handles) = self.env_colliders_by_layer.remove(&layer) {
            for opaque in handles {
                if let Some(col_handle) = self.env_collider_handles.remove(&opaque) {
                    self.collider_kinds.remove(&col_handle);
                    self.colliders
                        .remove(col_handle, &mut self.islands, &mut self.bodies, true);
                }
            }
        }
    }

    fn remove_environment_collider(&mut self, handle: u64) -> bool {
        let Some(col_handle) = self.env_collider_handles.remove(&handle) else {
            return false;
        };
        // Drop the layer-bucket entry too so per-layer bulk remove stays consistent.
        for bucket in self.env_colliders_by_layer.values_mut() {
            if let Some(pos) = bucket.iter().position(|h| *h == handle) {
                bucket.swap_remove(pos);
                break;
            }
        }
        self.collider_kinds.remove(&col_handle);
        self.colliders
            .remove(col_handle, &mut self.islands, &mut self.bodies, true);
        true
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

    fn disable_entity(&mut self, entity_id: EntityId, kind: EntityKind) -> bool {
        PhysicsWorld::disable_entity(self, entity_id, kind)
    }

    fn reuse_or_spawn_character(
        &mut self,
        entity_id: EntityId,
        position: game_protocol::types::Vec3f,
        kind: EntityKind,
    ) -> bool {
        let pos = Vector::new(position.x, position.y, position.z);
        PhysicsWorld::reuse_or_spawn_character(self, entity_id, pos, kind)
    }

    fn drain_pool(&mut self, max_idle: usize) {
        PhysicsWorld::drain_pool(self, max_idle);
    }

    fn line_of_sight_on_layer(
        &self,
        from: game_protocol::types::Vec3f,
        to: game_protocol::types::Vec3f,
        layer: u32,
    ) -> bool {
        let dx = to.x - from.x;
        let dy = to.y - from.y;
        let dz = to.z - from.z;
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        if dist < 1e-6 {
            return true;
        }
        let inv = 1.0 / dist;
        let dir = Vector::new(dx * inv, dy * inv, dz * inv);
        let origin = Vector::new(from.x, from.y, from.z);
        let ray = Ray::new(origin, dir);
        let layer_pred = move |_ch: ColliderHandle, collider: &Collider| -> bool {
            let col_layer = ud_layer(collider.user_data);
            col_layer == layer
        };
        let env_filter = QueryFilter::new()
            .groups(collision_groups::kcc_movement_groups())
            .predicate(&layer_pred);
        let query = self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.bodies,
            &self.colliders,
            env_filter,
        );
        match query.cast_ray_and_get_normal(&ray, dist, true) {
            None => true,
            Some((_, hit)) => hit.time_of_impact >= dist - 0.25,
        }
    }

    fn cast_to_wall_on_layer(
        &self,
        from: game_protocol::types::Vec3f,
        to: game_protocol::types::Vec3f,
        layer: u32,
    ) -> game_protocol::types::Vec3f {
        let dx = to.x - from.x;
        let dy = to.y - from.y;
        let dz = to.z - from.z;
        let dist = (dx * dx + dy * dy + dz * dz).sqrt();
        if dist < 1e-6 {
            return to;
        }
        let inv = 1.0 / dist;
        let dir = Vector::new(dx * inv, dy * inv, dz * inv);
        let origin = Vector::new(from.x, from.y, from.z);
        let ray = Ray::new(origin, dir);
        let layer_pred = move |_ch: ColliderHandle, collider: &Collider| -> bool {
            let col_layer = ud_layer(collider.user_data);
            col_layer == layer
        };
        let env_filter = QueryFilter::new()
            .groups(collision_groups::kcc_movement_groups())
            .predicate(&layer_pred);
        let query = self.broad_phase.as_query_pipeline(
            self.narrow_phase.query_dispatcher(),
            &self.bodies,
            &self.colliders,
            env_filter,
        );
        if let Some((_, hit)) = query.cast_ray_and_get_normal(&ray, dist, true) {
            let safe_dist = (hit.time_of_impact - 0.3_f32).max(0.0);
            game_protocol::types::Vec3f::new(
                from.x + dir.x * safe_dist,
                from.y + dir.y * safe_dist,
                from.z + dir.z * safe_dist,
            )
        } else {
            to
        }
    }

    fn set_entity_layer(&mut self, entity_id: EntityId, layer: u32) {
        self.entity_layers.insert(entity_id, layer);
        // Stamp layer into user_data of all colliders attached to this entity's body.
        if let Some(&body_handle) = self.entity_to_body.get(&entity_id) {
            if let Some(body) = self.bodies.get(body_handle) {
                let collider_handles: Vec<_> = body.colliders().to_vec();
                for ch in collider_handles {
                    if let Some(col) = self.colliders.get_mut(ch) {
                        col.user_data = ud_set_layer(col.user_data, layer);
                    }
                }
            }
        }
    }

    fn entity_layer(&self, entity_id: EntityId) -> u32 {
        self.entity_layers.get(&entity_id).copied().unwrap_or(0)
    }

    fn set_layer_policy(&mut self, layer: u32, policy: game_schema::LayerCollisionPolicy) {
        self.layer_policies.insert(layer, policy);
    }

    fn layer_policy(&self, layer: u32) -> game_schema::LayerCollisionPolicy {
        self.layer_policies.get(&layer).copied().unwrap_or_default()
    }

    fn remove_layer_policy(&mut self, layer: u32) {
        self.layer_policies.remove(&layer);
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
        let hits = world.intersections_with_sphere(Vector::new(5.0, 5.0, 0.0), 2.0);
        assert!(!hits.is_empty(), "Should find ball in sphere query");

        // Query far away — should find nothing
        let misses = world.intersections_with_sphere(Vector::new(50.0, 50.0, 0.0), 1.0);
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
        let e1 = world.add_dynamic_sphere(
            EntityId(1),
            Vector::new(0.0, 5.0, 0.0),
            0.5,
            1.0,
            collision_groups::player_body_groups(),
        );
        let e2 = world.add_dynamic_sphere(
            EntityId(2),
            Vector::new(3.0, 5.0, 0.0),
            0.5,
            1.0,
            collision_groups::npc_body_groups(),
        );
        assert_eq!(world.body_count(), 2);

        assert!(world.remove_entity(e1));
        assert_eq!(world.body_count(), 1);

        // Double removal returns false
        assert!(!world.remove_entity(e1));

        // e2 still works
        assert!(world.get_body_position(e2).is_some());
    }

    #[test]
    fn move_character_stays_on_flat_trimesh_surface() {
        use game_core::physics_backend::{EnvironmentShape, PhysicsBackend};
        use game_core::physics_constants::{CAPSULE_HALF_HEIGHT, CAPSULE_RADIUS, GROUND_PULL};
        const KCC_OFFSET_REL: f32 = 0.02;

        let dt = 0.05;
        let mut world = PhysicsWorld::new(dt);

        let vertices = vec![
            -20.0, 0.0, -20.0, // 0
            20.0, 0.0, -20.0, // 1
            20.0, 0.0, 20.0, // 2
            -20.0, 0.0, 20.0, // 3
        ];
        let indices = vec![0u32, 2, 1, 0, 3, 2];
        world.add_environment_collider_on_layer(
            EnvironmentShape::TriMesh { vertices, indices },
            game_protocol::types::Vec3f::new(0.0, 0.0, 0.0),
            4,
        );

        let entity = world.add_kinematic_capsule(
            EntityId(1),
            Vector::new(-10.0, CAPSULE_HALF_HEIGHT + CAPSULE_RADIUS, 0.0),
            0.5,
            0.3,
            collision_groups::player_body_groups(),
        );
        PhysicsBackend::set_entity_layer(&mut world, entity, 4);
        world.step();

        let expected_y = CAPSULE_HALF_HEIGHT
            + CAPSULE_RADIUS
            + (CAPSULE_HALF_HEIGHT + CAPSULE_RADIUS) * 2.0 * KCC_OFFSET_REL;
        let mut last_x = -10.0;

        for _ in 0..120 {
            let result = PhysicsBackend::move_character(
                &mut world,
                entity,
                game_protocol::types::Vec3f::new(0.1, -GROUND_PULL * dt, 0.0),
            )
            .expect("character move should succeed");
            world.step();

            let pos = world
                .get_body_position(entity)
                .expect("character body should still exist");
            assert!(
                result.grounded,
                "character should stay grounded on flat trimesh"
            );
            assert!(
                (pos.y - expected_y).abs() < 0.05,
                "character drifted off floor rest height: expected y≈{}, got {}",
                expected_y,
                pos.y
            );
            assert!(
                pos.x > last_x,
                "character should keep making forward progress on flat ground: prev x={}, new x={}",
                last_x,
                pos.x
            );
            last_x = pos.x;
        }
    }

    #[test]
    fn add_and_remove_environment_colliders_by_layer() {
        use game_core::physics_backend::EnvironmentShape;

        let mut world = PhysicsWorld::new(1.0 / 60.0);

        // Add 3 colliders on layer 100, 1 on layer 101.
        let h1 = world.add_environment_collider_on_layer(
            EnvironmentShape::Cuboid {
                half_x: 5.0,
                half_y: 1.0,
                half_z: 5.0,
            },
            game_protocol::types::Vec3f::new(0.0, 0.0, 0.0),
            100,
        );
        let h2 = world.add_environment_collider_on_layer(
            EnvironmentShape::Cuboid {
                half_x: 1.0,
                half_y: 3.0,
                half_z: 0.5,
            },
            game_protocol::types::Vec3f::new(10.0, 3.0, 0.0),
            100,
        );
        let h3 = world.add_environment_collider_on_layer(
            EnvironmentShape::Cylinder {
                half_height: 2.0,
                radius: 0.5,
            },
            game_protocol::types::Vec3f::new(-5.0, 2.0, -5.0),
            100,
        );
        let h4 = world.add_environment_collider_on_layer(
            EnvironmentShape::Cuboid {
                half_x: 2.0,
                half_y: 1.0,
                half_z: 2.0,
            },
            game_protocol::types::Vec3f::new(0.0, 0.0, 20.0),
            101,
        );

        // All handles should be distinct.
        let handles = [h1, h2, h3, h4];
        for i in 0..handles.len() {
            for j in (i + 1)..handles.len() {
                assert_ne!(handles[i], handles[j], "handles must be unique");
            }
        }

        // Verify tracking maps.
        assert_eq!(
            world.env_colliders_by_layer.get(&100).map(|v| v.len()),
            Some(3)
        );
        assert_eq!(
            world.env_colliders_by_layer.get(&101).map(|v| v.len()),
            Some(1)
        );
        // 4 from this test + 1 layer-0 placeholder floor from PhysicsWorld::new.
        assert_eq!(world.env_collider_handles.len(), 5);

        // Remove layer 100 — should leave layer 101 + layer-0 floor intact.
        world.remove_environment_colliders_by_layer(100);
        assert!(world.env_colliders_by_layer.get(&100).is_none());
        assert_eq!(world.env_collider_handles.len(), 2);
        assert_eq!(
            world.env_colliders_by_layer.get(&101).map(|v| v.len()),
            Some(1)
        );

        // Remove layer 101.
        world.remove_environment_colliders_by_layer(101);
        assert_eq!(world.env_collider_handles.len(), 1);
        assert_eq!(
            world.env_colliders_by_layer.get(&0).map(|v| v.len()),
            Some(1),
            "layer-0 placeholder floor must remain"
        );

        // Double-remove is a no-op.
        world.remove_environment_colliders_by_layer(100);
    }

    #[test]
    fn layer_colliders_block_raycast() {
        use game_core::physics_backend::EnvironmentShape;

        let mut world = PhysicsWorld::new(1.0 / 60.0);

        // Place a wall at x=5.
        world.add_environment_collider_on_layer(
            EnvironmentShape::Cuboid {
                half_x: 0.5,
                half_y: 5.0,
                half_z: 5.0,
            },
            game_protocol::types::Vec3f::new(5.0, 5.0, 0.0),
            100,
        );
        world.step();

        // Raycast from x=0 toward +x should hit the wall.
        let hit = world.raycast(Vector::new(0.0, 5.0, 0.0), Vector::new(1.0, 0.0, 0.0), 20.0);
        assert!(hit.is_some(), "ray should hit the layer collider wall");
        let toi = hit.unwrap().toi;
        assert!(
            toi > 3.0 && toi < 6.0,
            "hit should be near x=5, got toi={toi}"
        );

        // Remove layer 100 — ray should now pass through.
        world.remove_environment_colliders_by_layer(100);
        world.step();

        let hit_after = world.raycast(Vector::new(0.0, 5.0, 0.0), Vector::new(1.0, 0.0, 0.0), 20.0);
        // Should only hit the far ground or nothing (the ground is at y≈0).
        // At y=5 shooting horizontally, no ground hit expected within 20 units.
        assert!(
            hit_after.is_none(),
            "ray should pass through after layer removal"
        );
    }

    #[test]
    fn set_collider_enabled_toggles_prop() {
        let mut world = PhysicsWorld::new(1.0 / 60.0);
        let entity_id = EntityId(10);

        // Spawn a prop (gate). spawn_prop_body creates a body with a collider.
        let created = world.spawn_prop_body(
            entity_id,
            game_protocol::types::Vec3f::new(5.0, 1.0, 0.0),
            game_protocol::types::Vec3f::new(0.5, 2.0, 2.0),
            false,
        );
        assert!(created, "prop should be created");
        world.step();

        // Disable the collider (gate opens).
        assert!(world.set_collider_enabled(entity_id, false));

        // Re-enable (gate closes).
        assert!(world.set_collider_enabled(entity_id, true));

        // Non-existent entity returns false.
        assert!(!world.set_collider_enabled(EntityId(999), false));
    }

    // ── user_data bit-packing round-trip tests ──────────────────────

    #[test]
    fn ud_layer_round_trip() {
        let ud = ud_set_layer(0, 42);
        assert_eq!(ud_layer(ud), 42);

        let ud = ud_set_layer(0, u32::MAX);
        assert_eq!(ud_layer(ud), u32::MAX);

        let ud = ud_set_layer(0, 0);
        assert_eq!(ud_layer(ud), 0);
    }

    #[test]
    fn ud_entity_id_round_trip() {
        let ud = ud_set_entity_id(0, EntityId(123));
        assert_eq!(ud_entity_id(ud), Some(EntityId(123)));

        // Entity 0 → None (environment collider).
        assert_eq!(ud_entity_id(0), None);

        let ud = ud_set_entity_id(0, EntityId(0xFFFF_FFFF));
        assert_eq!(ud_entity_id(ud), Some(EntityId(0xFFFF_FFFF)));
    }

    #[test]
    fn ud_collider_kind_round_trip() {
        let ud = ud_set_kind(0, &ColliderKind::Body);
        assert_eq!(ud_collider_kind(ud), Some(ColliderKind::Body));

        let ud = ud_set_kind(0, &ColliderKind::Hurtbox);
        assert_eq!(ud_collider_kind(ud), Some(ColliderKind::Hurtbox));

        // Hitbox and BlockCone return None (HashMap fallback).
        let ud = ud_set_kind(0, &ColliderKind::Hitbox(99));
        assert_eq!(ud_collider_kind(ud), None);

        let ud = ud_set_kind(0, &ColliderKind::BlockCone(5));
        assert_eq!(ud_collider_kind(ud), None);
    }

    #[test]
    fn ud_stamp_encodes_entity_and_kind() {
        let ud = ud_stamp(0, EntityId(42), &ColliderKind::Hurtbox);
        assert_eq!(ud_entity_id(ud), Some(EntityId(42)));
        assert_eq!(ud_collider_kind(ud), Some(ColliderKind::Hurtbox));
    }

    #[test]
    fn ud_fields_do_not_overlap() {
        // Set all fields on the same u128 and verify each survives.
        let mut ud: u128 = 0;
        ud = ud_set_layer(ud, 100);
        ud = ud_set_kind(ud, &ColliderKind::Hurtbox);
        ud = ud_set_entity_id(ud, EntityId(9999));

        assert_eq!(ud_layer(ud), 100, "layer corrupted");
        assert_eq!(
            ud_collider_kind(ud),
            Some(ColliderKind::Hurtbox),
            "kind corrupted"
        );
        assert_eq!(
            ud_entity_id(ud),
            Some(EntityId(9999)),
            "entity_id corrupted"
        );
    }

    #[test]
    fn ud_set_layer_preserves_other_fields() {
        let mut ud: u128 = 0;
        ud = ud_set_entity_id(ud, EntityId(42));
        ud = ud_set_kind(ud, &ColliderKind::Body);

        // Now change only the layer.
        ud = ud_set_layer(ud, 777);
        assert_eq!(ud_layer(ud), 777);
        assert_eq!(
            ud_entity_id(ud),
            Some(EntityId(42)),
            "entity_id clobbered by set_layer"
        );
        assert_eq!(
            ud_collider_kind(ud),
            Some(ColliderKind::Body),
            "kind clobbered by set_layer"
        );
    }

    #[test]
    fn heightfield_collider_blocks_raycast_surface() {
        use game_core::physics_backend::EnvironmentShape;
        use game_core::physics_backend::PhysicsBackend;
        let mut world = PhysicsWorld::new(1.0 / 60.0);

        // 3×3 heightfield: flat at y=2.0 over a 10×10 footprint centred at origin.
        // Column-major: heights[i + j*nrows]. Here all samples == 2.0.
        let heights = vec![2.0_f32; 9];
        let shape = EnvironmentShape::Heightfield {
            nrows: 3,
            ncols: 3,
            scale_x: 10.0,
            scale_y: 1.0,
            scale_z: 10.0,
            heights,
        };
        world.add_environment_collider_on_layer(
            shape,
            game_protocol::types::Vec3f::new(0.0, 0.0, 0.0),
            0, // open-world layer
        );
        world.step();

        // Cast straight down from high above the centre; should hit the
        // heightfield plane at y=2.0 (scale_y * height sample == 1.0 * 2.0).
        let hit = world.raycast_surface(
            game_protocol::types::Vec3f::new(0.0, 50.0, 0.0),
            game_protocol::types::Vec3f::new(0.0, -1.0, 0.0),
            100.0,
            0,
        );
        let hit = hit.expect("raycast_surface should hit heightfield");
        assert!(
            (hit.y - 2.0).abs() < 0.1,
            "expected y≈2.0 on heightfield, got y={}",
            hit.y
        );
    }

    #[test]
    fn trimesh_collider_blocks_raycast_surface() {
        use game_core::physics_backend::EnvironmentShape;
        use game_core::physics_backend::PhysicsBackend;
        let mut world = PhysicsWorld::new(1.0 / 60.0);

        // Two-triangle quad at y=3.0, spanning [-5..5] in x and z. Vertices
        // are flat XYZ; indices form a triangle list. Wound CCW so the
        // upward normal faces +Y (right-hand rule).
        let vertices = vec![
            -5.0, 3.0, -5.0, // 0
            5.0, 3.0, -5.0, // 1
            5.0, 3.0, 5.0, // 2
            -5.0, 3.0, 5.0, // 3
        ];
        let indices = vec![0u32, 2, 1, 0, 3, 2];
        world.add_environment_collider_on_layer(
            EnvironmentShape::TriMesh { vertices, indices },
            game_protocol::types::Vec3f::new(0.0, 0.0, 0.0),
            4,
        );
        world.step();

        // Cast straight down through the centre on layer 4: should land
        // at y=3.0 on the trimesh.
        let hit = world
            .raycast_surface(
                game_protocol::types::Vec3f::new(0.0, 50.0, 0.0),
                game_protocol::types::Vec3f::new(0.0, -1.0, 0.0),
                100.0,
                4,
            )
            .expect("raycast_surface should hit trimesh");
        assert!(
            (hit.y - 3.0).abs() < 0.1,
            "expected y≈3.0 on trimesh, got y={}",
            hit.y
        );

        // Strict same-layer: a layer-5 caster must miss (the trimesh
        // is on layer 4).
        let miss = world.raycast_surface(
            game_protocol::types::Vec3f::new(0.0, 50.0, 0.0),
            game_protocol::types::Vec3f::new(0.0, -1.0, 0.0),
            100.0,
            5,
        );
        assert!(
            miss.is_none(),
            "layer-5 caster must not see layer-4 trimesh, got {miss:?}"
        );
    }

    #[test]
    fn trimesh_collider_falls_back_on_invalid_layout() {
        use game_core::physics_backend::EnvironmentShape;
        use game_core::physics_backend::PhysicsBackend;
        let mut world = PhysicsWorld::new(1.0 / 60.0);

        // Index out of range — should fall back to a 1m flat filler
        // rather than panic.
        let bad = EnvironmentShape::TriMesh {
            vertices: vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0],
            indices: vec![0, 1, 99],
        };
        let handle = world.add_environment_collider_on_layer(
            bad,
            game_protocol::types::Vec3f::new(0.0, 0.0, 0.0),
            42,
        );
        // Handle was still issued — the layer is materialised with a filler.
        assert!(world.env_collider_handles.contains_key(&handle));
        assert_eq!(
            world.env_colliders_by_layer.get(&42).map(|v| v.len()),
            Some(1)
        );
    }

    #[test]
    fn raycast_surface_returns_none_when_miss() {
        let mut world = PhysicsWorld::new(1.0 / 60.0);
        world.step();
        // Shoot upward — there's nothing above the layer-0 placeholder floor.
        let hit = world.raycast_surface(
            game_protocol::types::Vec3f::new(0.0, 1.0, 0.0),
            game_protocol::types::Vec3f::new(0.0, 1.0, 0.0),
            100.0,
            0,
        );
        assert!(hit.is_none(), "upward ray should miss, got {hit:?}");
    }

    #[test]
    fn raycast_surface_ignores_other_layer() {
        use game_core::physics_backend::EnvironmentShape;
        use game_core::physics_backend::PhysicsBackend;
        let mut world = PhysicsWorld::new(1.0 / 60.0);
        // Add a raised floor on layer 7.
        world.add_environment_collider_on_layer(
            EnvironmentShape::Cuboid {
                half_x: 5.0,
                half_y: 0.5,
                half_z: 5.0,
            },
            game_protocol::types::Vec3f::new(0.0, 5.0, 0.0),
            7,
        );
        world.step();

        // Strict same-layer: a layer-3 caster sees neither the layer-7
        // raised floor nor the layer-0 placeholder floor. With no terrain
        // authored on layer 3, the downward ray misses entirely.
        let hit3 = world.raycast_surface(
            game_protocol::types::Vec3f::new(0.0, 20.0, 0.0),
            game_protocol::types::Vec3f::new(0.0, -1.0, 0.0),
            100.0,
            3,
        );
        assert!(
            hit3.is_none(),
            "layer-3 caster must miss (no terrain on layer 3); strict layer rules \
             reject the layer-0 floor and the layer-7 raised floor. got {hit3:?}"
        );

        // Caster on layer 7 SHOULD see the raised floor (top at y≈5.5).
        let hit7 = world
            .raycast_surface(
                game_protocol::types::Vec3f::new(0.0, 20.0, 0.0),
                game_protocol::types::Vec3f::new(0.0, -1.0, 0.0),
                100.0,
                7,
            )
            .expect("layer-7 caster should hit raised floor");
        assert!(
            hit7.y > 5.0 && hit7.y < 6.0,
            "layer-7 caster should land on raised floor, got y={}",
            hit7.y
        );

        // Open-world caster (layer 0) sees the layer-0 placeholder floor.
        let hit0 = world
            .raycast_surface(
                game_protocol::types::Vec3f::new(0.0, 20.0, 0.0),
                game_protocol::types::Vec3f::new(0.0, -1.0, 0.0),
                100.0,
                0,
            )
            .expect("layer-0 caster should hit the open-world placeholder floor");
        assert!(
            hit0.y < 1.0,
            "layer-0 caster should land on the open-world placeholder floor, got y={}",
            hit0.y
        );
    }

    #[test]
    fn remove_environment_collider_by_handle_swaps_one_shape() {
        // Per-handle removal underpins the live terrain edit pipeline:
        // an `on_update` of a `terrain_chunk` row removes the old collider
        // and inserts the new one, without touching unrelated chunks on the
        // same layer. Verify the bucket bookkeeping stays consistent.
        use game_core::physics_backend::EnvironmentShape;

        let mut world = PhysicsWorld::new(1.0 / 60.0);
        let h_a = world.add_environment_collider_on_layer(
            EnvironmentShape::Cuboid {
                half_x: 1.0,
                half_y: 1.0,
                half_z: 1.0,
            },
            game_protocol::types::Vec3f::new(0.0, 0.0, 0.0),
            7,
        );
        let h_b = world.add_environment_collider_on_layer(
            EnvironmentShape::Cuboid {
                half_x: 1.0,
                half_y: 1.0,
                half_z: 1.0,
            },
            game_protocol::types::Vec3f::new(10.0, 0.0, 0.0),
            7,
        );
        assert_ne!(h_a, h_b);
        assert_eq!(
            world.env_colliders_by_layer.get(&7).map(|v| v.len()),
            Some(2)
        );

        // Remove A only.
        assert!(world.remove_environment_collider(h_a));
        assert_eq!(
            world.env_colliders_by_layer.get(&7).map(|v| v.len()),
            Some(1)
        );
        assert!(!world.env_collider_handles.contains_key(&h_a));
        assert!(world.env_collider_handles.contains_key(&h_b));

        // Idempotent on unknown handle.
        assert!(!world.remove_environment_collider(h_a));

        // Bulk remove still cleans the rest of the layer.
        world.remove_environment_colliders_by_layer(7);
        assert!(world.env_colliders_by_layer.get(&7).is_none());
        assert!(!world.env_collider_handles.contains_key(&h_b));
    }
}
