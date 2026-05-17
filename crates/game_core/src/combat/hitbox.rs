use std::collections::{HashMap, HashSet};

use crate::combat::skill::{AbilityExecutionId, HitEffectSpec, HitboxRules, SkillShape};
use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_schema::Vec3f;

/// Key for an active hitbox — the unique execution ID for one concrete cast.
/// Replaces the former `(EntityId, ability_id)` tuple: execution IDs are
/// globally monotonic so two casts of the same ability by the same entity
/// have distinct keys and can coexist in the store simultaneously.
pub type HitboxKey = AbilityExecutionId;

/// Runtime record for a currently active hitbox collider.
///
/// Created when an ability timeline fires `SpawnHitbox` and removed
/// on `RemoveHitbox` (or when the owning entity despawns).
/// The `already_hit` set prevents the same hitbox from damaging a
/// target more than once per activation window.
#[derive(Clone, Debug)]
pub struct ActiveHitbox {
    /// Unique cast identity — mirrors the map key.
    pub execution_id: AbilityExecutionId,
    /// Entity that owns this hitbox.
    pub owner: EntityId,
    /// Ability that spawned it (used for damage lookup and event emission).
    pub ability_id: u32,
    /// Tick when the hitbox was created.
    pub spawned_at: TickId,
    /// Collision shape — stored so `ApplyDamageFrame` can materialise the Rapier
    /// sensor without re-reading the original `AbilityAction`.
    pub shape: SkillShape,
    /// Entity-local offset for the sensor origin — see `SpawnHitbox` action.
    pub offset: Vec3f,
    /// Flattened hit payload resolved at hitbox spawn time.
    pub effect: Option<HitEffectSpec>,
    /// Flattened hitbox behavior resolved at hitbox spawn time.
    pub rules: HitboxRules,
    /// True once `ApplyDamageFrame` has spawned the live Rapier sensor.
    /// Until armed, the hitbox is a logical declaration only — no physics collision.
    pub armed: bool,
    /// Opaque physics backend sensor handle for the live collider, if armed.
    ///
    /// Ownership is local to the hitbox record so lifecycle transitions are symmetric:
    /// - arm: set `armed=true` and `sensor_handle=Some(handle)`
    /// - remove/despawn: take handle from this record and call `physics.remove_sensor`
    pub sensor_handle: Option<u64>,
    /// Entities already hit by this hitbox instance (single-hit dedup).
    pub already_hit: HashSet<EntityId>,
    /// Lag compensation: number of ticks to rewind target positions when testing
    /// for overlap. Copied from the `AbilityExecutionContext` in Phase 3.
    /// 0 means no compensation.
    pub rewind_ticks: u32,
    /// Entities currently overlapping this hitbox sensor.
    ///
    /// Kept in sync from Started/Stopped events and from live sensor-overlap
    /// queries for continuous hitboxes so periodic damage can see stationary
    /// occupants that never generated a transition event.
    pub overlapping: HashSet<EntityId>,
    /// Tick when periodic damage was last applied. Initialised to the arm tick
    /// so the first periodic pulse fires after `damage_interval_ticks` elapse.
    pub last_damage_tick: TickId,
    /// True if this hitbox was materialised as a detached world-space sensor
    /// (ground-target, caster-offset, or projectile). Used by removal logic to emit
    /// `SkillObjectRemoved` so clients can despawn the visual.
    pub world_sensor: bool,
    /// If set, this hitbox is a world-space projectile travelling at `speed`
    /// units per tick along `direction`. Updated each tick in the projectile
    /// movement sub-phase. `None` for entity-parented hitboxes.
    pub projectile: Option<ProjectileState>,
}

/// Runtime state for a travelling world-space projectile hitbox.
#[derive(Clone, Debug)]
pub struct ProjectileState {
    /// Current world-space position of the projectile.
    pub position: Vec3f,
    /// Position at the start of this tick (before movement). Used for swept
    /// collision detection so fast projectiles cannot tunnel through targets.
    pub prev_position: Vec3f,
    /// Normalised travel direction (XZ plane, unit length).
    pub direction: Vec3f,
    /// Units per tick travel speed.
    pub speed: f32,
    /// Maximum squared distance from origin before auto-removal.
    pub max_range_sq: f32,
    /// Spawn origin — used for range check.
    pub origin: Vec3f,
}

/// Tracks all active hitbox colliders in the simulation.
///
/// Keyed by `AbilityExecutionId` — each accepted `UseAbility` cast gets a
/// unique monotonic ID, so two casts of the same ability by the same entity
/// produce distinct entries and can both be live simultaneously.
#[derive(Clone, Debug, Default)]
pub struct HitboxStore {
    active: HashMap<AbilityExecutionId, ActiveHitbox>,
}

impl HitboxStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new unarmed hitbox (logical declaration only — no Rapier sensor yet).
    /// Call `arm()` when `ApplyDamageFrame` fires to materialise the physics sensor.
    pub fn spawn(
        &mut self,
        execution_id: AbilityExecutionId,
        owner: EntityId,
        ability_id: u32,
        tick: TickId,
        shape: SkillShape,
        offset: Vec3f,
        rewind_ticks: u32,
        allow_reentry: bool,
        damage_interval_ticks: u32,
        pierce: bool,
        max_rewind_ticks: Option<u32>,
    ) {
        self.spawn_with_optional_payload(
            execution_id,
            owner,
            ability_id,
            tick,
            shape,
            offset,
            rewind_ticks,
            None,
            HitboxRules {
                allow_reentry,
                damage_interval_ticks,
                pierce,
                max_rewind_ticks,
                target_filter: crate::combat::skill::TargetFilter::Hostile,
            },
        );
    }

    pub fn spawn_with_payload(
        &mut self,
        execution_id: AbilityExecutionId,
        owner: EntityId,
        ability_id: u32,
        tick: TickId,
        shape: SkillShape,
        offset: Vec3f,
        rewind_ticks: u32,
        effect: Option<HitEffectSpec>,
        rules: HitboxRules,
    ) {
        self.spawn_with_optional_payload(
            execution_id,
            owner,
            ability_id,
            tick,
            shape,
            offset,
            rewind_ticks,
            effect,
            rules,
        );
    }

    fn spawn_with_optional_payload(
        &mut self,
        execution_id: AbilityExecutionId,
        owner: EntityId,
        ability_id: u32,
        tick: TickId,
        shape: SkillShape,
        offset: Vec3f,
        rewind_ticks: u32,
        effect: Option<HitEffectSpec>,
        rules: HitboxRules,
    ) {
        self.active.insert(
            execution_id,
            ActiveHitbox {
                execution_id,
                owner,
                ability_id,
                spawned_at: tick,
                shape,
                offset,
                effect,
                rules,
                armed: false,
                sensor_handle: None,
                already_hit: HashSet::new(),
                rewind_ticks,
                overlapping: HashSet::new(),
                last_damage_tick: tick,
                world_sensor: false,
                projectile: None,
            },
        );
    }

    /// Register a hitbox that is already armed (Rapier sensor already live).
    /// Use this in tests that bypass the timeline and inject sensors directly.
    pub fn spawn_armed(
        &mut self,
        execution_id: AbilityExecutionId,
        owner: EntityId,
        ability_id: u32,
        tick: TickId,
        shape: SkillShape,
        offset: Vec3f,
        sensor_handle: u64,
    ) {
        self.spawn(
            execution_id,
            owner,
            ability_id,
            tick,
            shape,
            offset,
            0,
            false,
            0,
            false,
            None,
        );
        self.arm_at_tick(execution_id, sensor_handle, tick);
    }

    /// Mark a hitbox as armed using its spawn tick as the periodic-damage anchor.
    ///
    /// Delayed materialization paths should prefer `arm_at_tick()` so the first
    /// periodic pulse is measured from the arm frame rather than the declaration frame.
    pub fn arm(&mut self, execution_id: AbilityExecutionId, sensor_handle: u64) -> bool {
        let arm_tick = match self.active.get(&execution_id) {
            Some(hb) => hb.spawned_at,
            None => return false,
        };
        self.arm_at_tick(execution_id, sensor_handle, arm_tick)
    }

    /// Mark a hitbox as armed (Rapier sensor now live) and anchor periodic timing to `tick`.
    /// Returns `true` if the hitbox existed and was not already armed, `false` otherwise.
    pub fn arm_at_tick(
        &mut self,
        execution_id: AbilityExecutionId,
        sensor_handle: u64,
        tick: TickId,
    ) -> bool {
        match self.active.get_mut(&execution_id) {
            Some(hb) if !hb.armed => {
                hb.armed = true;
                hb.sensor_handle = Some(sensor_handle);
                hb.last_damage_tick = tick;
                true
            }
            _ => false,
        }
    }

    /// Count of hitboxes that have a live Rapier sensor (armed == true).
    pub fn armed_count(&self) -> usize {
        self.active.values().filter(|hb| hb.armed).count()
    }

    /// Remove a hitbox and return its record if it existed.
    pub fn remove(&mut self, execution_id: AbilityExecutionId) -> Option<ActiveHitbox> {
        self.active.remove(&execution_id)
    }

    /// Look up an active hitbox by execution ID.
    pub fn get(&self, execution_id: AbilityExecutionId) -> Option<&ActiveHitbox> {
        self.active.get(&execution_id)
    }

    /// Look up an active hitbox mutably by execution ID.
    pub fn get_mut(&mut self, execution_id: AbilityExecutionId) -> Option<&mut ActiveHitbox> {
        self.active.get_mut(&execution_id)
    }

    /// Check whether this hitbox has already hit a given target.
    pub fn has_hit(&self, execution_id: AbilityExecutionId, target: EntityId) -> bool {
        self.active
            .get(&execution_id)
            .is_some_and(|hb| hb.already_hit.contains(&target))
    }

    /// Record that this hitbox hit a target.
    /// Returns false if the hitbox doesn't exist or the target was already recorded.
    pub fn record_hit(&mut self, execution_id: AbilityExecutionId, target: EntityId) -> bool {
        match self.active.get_mut(&execution_id) {
            Some(hb) => hb.already_hit.insert(target),
            None => false,
        }
    }

    /// Remove a target from the already-hit set so re-entry can deal damage again.
    /// Only effective on hitboxes with `allow_reentry == true`.
    pub fn clear_hit(&mut self, execution_id: AbilityExecutionId, target: EntityId) {
        if let Some(hb) = self.active.get_mut(&execution_id) {
            if hb.rules.allow_reentry {
                hb.already_hit.remove(&target);
            }
        }
    }

    /// Add a target to the overlapping set (entered the sensor volume).
    pub fn add_overlapping(&mut self, execution_id: AbilityExecutionId, target: EntityId) {
        if let Some(hb) = self.active.get_mut(&execution_id) {
            hb.overlapping.insert(target);
        }
    }

    /// Remove a target from the overlapping set (left the sensor volume).
    pub fn remove_overlapping(&mut self, execution_id: AbilityExecutionId, target: EntityId) {
        if let Some(hb) = self.active.get_mut(&execution_id) {
            hb.overlapping.remove(&target);
        }
    }

    /// Collect periodic-damage hitboxes that are due for a re-damage tick.
    /// Returns `(execution_id, owner, ability_id, targets_to_damage)` for each.
    pub fn collect_periodic_due(
        &self,
        current_tick: TickId,
    ) -> Vec<(AbilityExecutionId, EntityId, u32, Vec<EntityId>)> {
        self.active
            .values()
            .filter(|hb| hb.armed && hb.rules.damage_interval_ticks > 0)
            .filter(|hb| {
                current_tick.0.saturating_sub(hb.last_damage_tick.0)
                    >= hb.rules.damage_interval_ticks as u64
            })
            .map(|hb| {
                let targets: Vec<EntityId> = hb.overlapping.iter().copied().collect();
                (hb.execution_id, hb.owner, hb.ability_id, targets)
            })
            .collect()
    }

    /// Update `last_damage_tick` after periodic damage has been applied.
    pub fn mark_periodic_tick(&mut self, execution_id: AbilityExecutionId, tick: TickId) {
        if let Some(hb) = self.active.get_mut(&execution_id) {
            hb.last_damage_tick = tick;
        }
    }

    /// Remove all hitboxes owned by an entity.
    /// Returns the execution IDs that were removed (used for sensor_handles cleanup).
    pub fn remove_all_for_entity(&mut self, entity_id: EntityId) -> Vec<AbilityExecutionId> {
        let keys: Vec<AbilityExecutionId> = self
            .active
            .iter()
            .filter(|(_, hb)| hb.owner == entity_id)
            .map(|(k, _)| *k)
            .collect();
        for k in &keys {
            self.active.remove(k);
        }
        keys
    }

    /// Iterate all active hitboxes for a given entity.
    pub fn hitboxes_for(&self, entity_id: EntityId) -> Vec<&ActiveHitbox> {
        self.active
            .values()
            .filter(|hb| hb.owner == entity_id)
            .collect()
    }

    /// Iterate all armed hitboxes that require lag-compensated hit detection.
    ///
    /// Detached world sensors keep their own authoritative world pose and must
    /// resolve through their dedicated physics/query paths instead of being
    /// rebuilt from the caster's current position.
    pub fn iter_armed_compensated(&self) -> impl Iterator<Item = &ActiveHitbox> {
        self.active.values().filter(|hb| {
            hb.armed && hb.rewind_ticks > 0 && !hb.world_sensor && hb.projectile.is_none()
        })
    }

    /// Collect execution IDs and sensor handles for all armed world-space projectiles.
    /// Used by the projectile movement sub-phase to advance positions.
    pub fn armed_projectile_ids(&self) -> Vec<AbilityExecutionId> {
        self.active
            .values()
            .filter(|hb| hb.armed && hb.projectile.is_some())
            .map(|hb| hb.execution_id)
            .collect()
    }

    /// Collect execution IDs for armed, non-projectile hitboxes that apply
    /// periodic damage while targets remain inside the sensor volume.
    pub fn armed_periodic_ids(&self) -> Vec<AbilityExecutionId> {
        self.active
            .values()
            .filter(|hb| hb.armed && hb.rules.damage_interval_ticks > 0 && hb.projectile.is_none())
            .map(|hb| hb.execution_id)
            .collect()
    }

    /// Total number of active hitboxes.
    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Returns the set of execution IDs for all armed hitboxes.
    ///
    /// Used by `verify_invariants` to perform a key-set diff against `sensor_handles`.
    /// A count-only check hides identity bugs where two different entries swap;
    /// this set comparison catches those silently.
    pub fn armed_execution_ids(&self) -> HashSet<AbilityExecutionId> {
        self.active
            .iter()
            .filter(|(_, hb)| hb.armed)
            .map(|(k, _)| *k)
            .collect()
    }

    /// Returns the set of execution IDs that currently own a physics sensor handle.
    pub fn sensor_backed_execution_ids(&self) -> HashSet<AbilityExecutionId> {
        self.active
            .iter()
            .filter(|(_, hb)| hb.sensor_handle.is_some())
            .map(|(k, _)| *k)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eid(n: u64) -> EntityId {
        EntityId(n)
    }

    fn exec(n: u64) -> AbilityExecutionId {
        AbilityExecutionId(n)
    }

    #[test]
    fn spawn_and_remove() {
        let mut store = HitboxStore::new();
        store.spawn(
            exec(1),
            eid(1),
            100,
            TickId(5),
            SkillShape::Sphere,
            Vec3f::ZERO,
            0,
            false,
            0,
            false,
            None,
        );
        assert_eq!(store.len(), 1);
        assert!(store.get(exec(1)).is_some());

        assert!(store.remove(exec(1)).is_some());
        assert!(store.is_empty());
        assert!(store.remove(exec(1)).is_none()); // already gone
    }

    #[test]
    fn hit_dedup() {
        let mut store = HitboxStore::new();
        store.spawn(
            exec(42),
            eid(1),
            42,
            TickId(0),
            SkillShape::Sphere,
            Vec3f::ZERO,
            0,
            false,
            0,
            false,
            None,
        );

        assert!(!store.has_hit(exec(42), eid(2)));
        assert!(store.record_hit(exec(42), eid(2))); // first hit → true
        assert!(store.has_hit(exec(42), eid(2)));
        assert!(!store.record_hit(exec(42), eid(2))); // duplicate → false
    }

    #[test]
    fn remove_all_for_entity() {
        let mut store = HitboxStore::new();
        store.spawn(
            exec(10),
            eid(1),
            10,
            TickId(0),
            SkillShape::Sphere,
            Vec3f::ZERO,
            0,
            false,
            0,
            false,
            None,
        );
        store.spawn(
            exec(20),
            eid(1),
            20,
            TickId(1),
            SkillShape::Sphere,
            Vec3f::ZERO,
            0,
            false,
            0,
            false,
            None,
        );
        store.spawn(
            exec(30),
            eid(2),
            10,
            TickId(0),
            SkillShape::Sphere,
            Vec3f::ZERO,
            0,
            false,
            0,
            false,
            None,
        );

        let mut removed = store.remove_all_for_entity(eid(1));
        removed.sort_by_key(|id| id.0);
        assert_eq!(removed, vec![exec(10), exec(20)]);
        assert_eq!(store.len(), 1);
        assert!(store.get(exec(30)).is_some());
    }

    #[test]
    fn two_casts_of_same_ability_coexist() {
        // With execution-ID keys an entity can have two live hitboxes from the
        // same ability simultaneously — the key concern that motivated this refactor.
        let mut store = HitboxStore::new();
        store.spawn(
            exec(1),
            eid(1),
            5,
            TickId(0),
            SkillShape::Sphere,
            Vec3f::ZERO,
            0,
            false,
            0,
            false,
            None,
        );
        store.spawn(
            exec(2),
            eid(1),
            5,
            TickId(1),
            SkillShape::Sphere,
            Vec3f::ZERO,
            0,
            false,
            0,
            false,
            None,
        );

        assert_eq!(
            store.len(),
            2,
            "two casts must produce two independent entries"
        );
        assert!(store.get(exec(1)).is_some());
        assert!(store.get(exec(2)).is_some());

        // Recording a hit on one execution does not bleed into the other.
        store.record_hit(exec(1), eid(9));
        assert!(store.has_hit(exec(1), eid(9)));
        assert!(!store.has_hit(exec(2), eid(9)));
    }

    #[test]
    fn record_hit_on_missing_hitbox() {
        let mut store = HitboxStore::new();
        assert!(!store.record_hit(exec(99), eid(2)));
    }

    #[test]
    fn arm_gates_damage_frame() {
        let mut store = HitboxStore::new();
        let e1 = exec(5);
        store.spawn(
            e1,
            eid(1),
            5,
            TickId(0),
            SkillShape::Sphere,
            Vec3f::ZERO,
            0,
            false,
            0,
            false,
            None,
        );
        assert_eq!(
            store.armed_count(),
            0,
            "freshly spawned hitbox must not be armed"
        );

        assert!(
            store.arm(e1, 100),
            "arm() must return true when hitbox exists and is unarmed"
        );
        assert_eq!(
            store.armed_count(),
            1,
            "armed_count must reflect the armed hitbox"
        );
        assert!(
            !store.arm(e1, 101),
            "arm() on an already-armed hitbox must return false"
        );

        // spawn_armed() starts armed immediately.
        let e2 = exec(7);
        store.spawn_armed(
            e2,
            eid(2),
            7,
            TickId(1),
            SkillShape::CapsuleSweep,
            Vec3f::ZERO,
            200,
        );
        assert_eq!(store.armed_count(), 2, "spawn_armed() must count as armed");

        // armed_execution_ids must match both armed entries.
        let armed_ids = store.armed_execution_ids();
        assert!(armed_ids.contains(&e1));
        assert!(armed_ids.contains(&e2));
        assert_eq!(armed_ids.len(), 2);

        let sensor_ids = store.sensor_backed_execution_ids();
        assert_eq!(
            armed_ids, sensor_ids,
            "armed and sensor-backed key sets must match"
        );
    }

    #[test]
    fn delayed_arm_reanchors_periodic_interval_to_arm_tick() {
        let mut store = HitboxStore::new();
        let execution_id = exec(9);
        store.spawn(
            execution_id,
            eid(1),
            9,
            TickId(3),
            SkillShape::Sphere,
            Vec3f::ZERO,
            0,
            false,
            2,
            false,
            None,
        );

        assert!(store.arm_at_tick(execution_id, 500, TickId(7)));
        assert_eq!(store.get(execution_id).unwrap().last_damage_tick, TickId(7));
        assert!(
            store.collect_periodic_due(TickId(8)).is_empty(),
            "one tick after arm is too early"
        );
        assert_eq!(
            store.collect_periodic_due(TickId(9)).len(),
            1,
            "periodic pulse should unlock from the arm tick"
        );
    }

    #[test]
    fn clear_hit_reentry() {
        let mut store = HitboxStore::new();
        // allow_reentry = true
        store.spawn(
            exec(1),
            eid(1),
            5,
            TickId(0),
            SkillShape::Sphere,
            Vec3f::ZERO,
            0,
            true,
            0,
            false,
            None,
        );
        assert!(store.record_hit(exec(1), eid(9)));
        assert!(
            !store.record_hit(exec(1), eid(9)),
            "duplicate still blocked before clear"
        );

        store.clear_hit(exec(1), eid(9));
        assert!(
            store.record_hit(exec(1), eid(9)),
            "re-entry should succeed after clear"
        );
    }

    #[test]
    fn clear_hit_ignored_when_not_reentry() {
        let mut store = HitboxStore::new();
        // allow_reentry = false (default)
        store.spawn(
            exec(1),
            eid(1),
            5,
            TickId(0),
            SkillShape::Sphere,
            Vec3f::ZERO,
            0,
            false,
            0,
            false,
            None,
        );
        assert!(store.record_hit(exec(1), eid(9)));

        store.clear_hit(exec(1), eid(9));
        assert!(
            !store.record_hit(exec(1), eid(9)),
            "clear_hit must be a no-op when allow_reentry is false"
        );
    }
}
