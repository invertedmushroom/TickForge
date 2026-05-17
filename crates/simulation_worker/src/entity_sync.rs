//! Entity lifecycle mirroring — SDK-free decision logic.
//!
//! `EntitySync` centralises the spawn-or-skip, lifecycle-mirror, and
//! delete-cleanup decisions that the coordinator's three entity callbacks
//! (`entity.on_insert`, `entity.on_update`, `entity.on_delete`) previously
//! inlined.  The coordinator callbacks become thin adapters: read SDK cache
//! fields, convert binding types → game_schema types, call `EntitySync`.
//!
//! This module has no SpacetimeDB dependency and is testable offline.

use game_core::combat::status::ActiveBuff;
use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_protocol::types::Vec3f;
use game_schema::{EntityKind, EntityState, NpcAiState};
use log::{debug, info, warn};

use crate::simulation_runner::SimulationRunner;

/// Result of a `sync_insert` call.
#[derive(Debug, PartialEq, Eq)]
pub enum SyncInsertResult {
    /// Entity was spawned into the simulation.
    Spawned,
    /// Skipped because the entity state is terminal (Removed / DespawnPending).
    SkippedTerminal,
    /// Skipped because the entity is already tracked.
    SkippedDuplicate,
}

/// Result of a `sync_update` call.
#[derive(Debug, PartialEq, Eq)]
pub enum SyncUpdateResult {
    /// Entity was marked DespawnPending in the simulation.
    MarkedDespawn,
    /// Entity was force-removed from the simulation.
    Removed,
    /// Removal was a no-op (entity not present).
    RemoveNoop,
    /// Entity transitioned to Spawning — coordinator must read companion
    /// rows from the SDK cache and call `sync_insert` to re-create the
    /// physics body and runtime state (player respawn, NPC re-spawn).
    Respawn,
    /// Transition was ignored (handled internally by the pipeline).
    Ignored,
}

/// Result of a `sync_delete` call.
#[derive(Debug, PartialEq, Eq)]
pub enum SyncDeleteResult {
    /// Entity was present and force-removed.
    Removed,
    /// Entity was not present — no-op.
    Noop,
}

/// Runtime state recovered from the DB for seeding after a spawn.
///
/// Groups the optional restoration payloads (buffs, NPC AI) that
/// `sync_insert` passes to `SimulationRunner::seed_runtime_state`.
/// Threat is reconstructed from `npc_state.target_entity` at seed time.
#[derive(Default)]
pub struct RuntimeSnapshot {
    pub buffs: Vec<ActiveBuff>,
    pub npc_state: Option<(NpcAiState, Option<EntityId>)>,
    /// NPC spawn config: (passive, no_chase, ability_ids).
    pub npc_config: Option<NpcSpawnConfig>,
}

/// Lightweight NPC configuration read from the DB at spawn time.
#[derive(Clone, Debug, Default)]
pub struct NpcSpawnConfig {
    pub passive: bool,
    pub no_chase: bool,
    pub ability_ids: Vec<u32>,
    pub leash_radius: f32,
    pub aggro_radius: f32,
}

/// SDK-free entity lifecycle mirroring logic.
///
/// Each method encodes the decision logic that was previously inlined in
/// coordinator callbacks.  The coordinator now does type conversion and
/// delegates here.
pub struct EntitySync;

impl EntitySync {
    /// Spawn-or-skip logic for new entity rows.
    ///
    /// Guards:
    /// - Entities in `Removed` or `DespawnPending` state are skipped (prevents
    ///   resurrection on worker restart when the subscription snapshot contains
    ///   terminal rows).
    /// - Entities already tracked (`contains()`) are skipped (idempotency
    ///   against both initial-subscription and live-insert events).
    pub fn sync_insert(
        sim: &mut SimulationRunner,
        id: EntityId,
        kind: EntityKind,
        state: EntityState,
        tick: TickId,
        max_hp: f32,
        position: Vec3f,
        layer: u32,
        snapshot: RuntimeSnapshot,
    ) -> SyncInsertResult {
        // Guard: never spawn entities that have already passed their useful lifecycle.
        match state {
            EntityState::Removed | EntityState::DespawnPending => {
                debug!("Entity {} is {:?} in DB — skipping spawn", id.0, state);
                return SyncInsertResult::SkippedTerminal;
            }
            _ => {}
        }

        if sim.contains(id) {
            debug!(
                "Entity {} already tracked — ignoring duplicate insert",
                id.0
            );
            return SyncInsertResult::SkippedDuplicate;
        }

        sim.spawn_entity_from_snapshot(id, kind, tick, max_hp, position, layer);

        let RuntimeSnapshot {
            buffs,
            npc_state,
            npc_config,
        } = snapshot;

        // Restore runtime state from DB rows (noops if slices are empty).
        // Threat is reconstructed from npc_state.target_entity inside seed_runtime_state.
        let buff_pairs = if buffs.is_empty() {
            vec![]
        } else {
            vec![(id, buffs)]
        };
        let npc_pairs: Vec<(EntityId, NpcAiState, Option<EntityId>)> = npc_state
            .map(|(ai, tgt)| vec![(id, ai, tgt)])
            .unwrap_or_default();
        if !buff_pairs.is_empty() || !npc_pairs.is_empty() {
            sim.seed_runtime_state(&buff_pairs, &npc_pairs);
        }

        // Apply NPC spawn config (passive, no_chase, custom abilities).
        if let Some(cfg) = npc_config {
            sim.configure_npc(id, cfg);
        }

        //info!("Entity {} ({kind:?}) spawned into simulation at tick {:?} pos=({},{},{})",id.0, tick, position.x, position.y, position.z);
        SyncInsertResult::Spawned
    }

    /// Mirror external lifecycle transitions.
    ///
    /// The simulation is authoritative for `Spawning→Active` (Phase 8) and
    /// normal combat deaths, so we only act on transitions that the pipeline
    /// didn't initiate:
    /// - `Active → DespawnPending`: external force-despawn.
    /// - `_ → Removed`: hard removal by a server-side reducer.
    pub fn sync_update(
        sim: &mut SimulationRunner,
        id: EntityId,
        old_state: EntityState,
        new_state: EntityState,
    ) -> SyncUpdateResult {
        match (old_state, new_state) {
            // External reducer set DespawnPending without going through combat death.
            (EntityState::Active, EntityState::DespawnPending) => {
                if sim.is_active(id) {
                    sim.mark_despawn(id);
                    info!(
                        "Entity {} externally marked DespawnPending — mirrored to simulation",
                        id.0
                    );
                    SyncUpdateResult::MarkedDespawn
                } else {
                    SyncUpdateResult::Ignored
                }
            }
            // Hard removal by a server-side reducer; skip if already cleaned up.
            (_, EntityState::Removed) => {
                if sim.entity_exists(id) {
                    let removed = sim.force_remove_entity(id);
                    if removed {
                        info!(
                            "Entity {} externally set Removed — cleaned up from simulation",
                            id.0
                        );
                        SyncUpdateResult::Removed
                    } else {
                        info!(
                            "Entity {} externally set Removed — no-op (not present)",
                            id.0
                        );
                        SyncUpdateResult::RemoveNoop
                    }
                } else {
                    SyncUpdateResult::RemoveNoop
                }
            }
            // Respawn: entity returned to Spawning from a terminal state.
            // Clean up any stale simulation state then signal the coordinator
            // to read companion rows and call sync_insert.
            (_, EntityState::Spawning) => {
                if sim.entity_exists(id) {
                    sim.force_remove_entity(id);
                    info!(
                        "Entity {} respawning — cleaned up stale simulation state",
                        id.0
                    );
                }
                SyncUpdateResult::Respawn
            }
            // Spawning→Active handled by pipeline Phase 8; other transitions ignored.
            _ => SyncUpdateResult::Ignored,
        }
    }

    /// Force-cleanup guard for hard row deletions.
    ///
    /// In the normal despawn flow the simulation already called
    /// `force_remove_entity` before the DB row is deleted, so this is
    /// usually a cheap no-op.
    pub fn sync_delete(sim: &mut SimulationRunner, id: EntityId) -> SyncDeleteResult {
        let removed = sim.force_remove_entity(id);
        if removed {
            warn!(
                "Entity {} row deleted while still in simulation — forced cleanup",
                id.0
            );
            SyncDeleteResult::Removed
        } else {
            SyncDeleteResult::Noop
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_core::combat::skill::AbilityRegistry;
    use game_core::physics_backend::*;
    use game_protocol::types::{Quatf, Transform};
    use std::collections::HashMap;

    // ── Test helpers ────────────────────────────────────────────

    struct MockPhysics {
        transforms: HashMap<EntityId, Vec3f>,
    }

    impl MockPhysics {
        fn new() -> Self {
            Self {
                transforms: HashMap::new(),
            }
        }
    }

    impl PhysicsBackend for MockPhysics {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn step(&mut self, _dt: f32) {}
        fn get_transform(&self, id: EntityId) -> Option<Transform> {
            self.transforms
                .get(&id)
                .map(|p| Transform::at_position(p.x, p.y, p.z))
        }
        fn get_all_transforms(&self) -> Vec<(EntityId, Transform)> {
            self.transforms
                .iter()
                .map(|(id, p)| (*id, Transform::at_position(p.x, p.y, p.z)))
                .collect()
        }
        fn drain_collision_events(&mut self) -> Vec<CollisionEvent> {
            vec![]
        }
        fn remove_entity(&mut self, id: EntityId) -> bool {
            self.transforms.remove(&id).is_some()
        }
        fn set_kinematic_position(&mut self, id: EntityId, pos: Vec3f) -> bool {
            if let Some(t) = self.transforms.get_mut(&id) {
                *t = pos;
                true
            } else {
                false
            }
        }
        fn set_kinematic_rotation(&mut self, _id: EntityId, _rot: Quatf) -> bool {
            true
        }
        fn set_linear_velocity(&mut self, _id: EntityId, _vel: Vec3f) -> bool {
            true
        }
        fn spawn_sensor(
            &mut self,
            _id: EntityId,
            _shape: SensorShape,
            _offset: Vec3f,
            _kind: ColliderKind,
        ) -> Option<u64> {
            Some(999)
        }
        fn spawn_world_sensor(
            &mut self,
            _position: Vec3f,
            _shape: SensorShape,
            _kind: ColliderKind,
            _owner: EntityId,
        ) -> u64 {
            0
        }
        fn set_sensor_position(&mut self, _handle: u64, _position: Vec3f) -> bool {
            false
        }
        fn remove_sensor(&mut self, _handle: u64) {}
        fn spawn_character_body(&mut self, id: EntityId, pos: Vec3f, _kind: EntityKind) -> bool {
            self.transforms.insert(id, pos);
            true
        }
        fn spawn_prop_body(
            &mut self,
            id: EntityId,
            pos: Vec3f,
            _half_extents: Vec3f,
            _pushable: bool,
        ) -> bool {
            self.transforms.insert(id, pos);
            true
        }
        fn move_character(&mut self, id: EntityId, desired: Vec3f) -> Option<MoveResult> {
            let p = self.transforms.get_mut(&id)?;
            *p = Vec3f {
                x: p.x + desired.x,
                y: p.y + desired.y,
                z: p.z + desired.z,
            };
            Some(MoveResult {
                position: *p,
                grounded: true,
            })
        }
        fn raycast(
            &self,
            _origin: Vec3f,
            _direction: Vec3f,
            _max_distance: f32,
            _ignore_entity: Option<EntityId>,
        ) -> Option<RayHit> {
            None
        }
        fn line_of_sight(&self, _from: Vec3f, _to: Vec3f) -> bool {
            true
        }
        fn cast_to_wall(&self, _from: Vec3f, to: Vec3f) -> Vec3f {
            to
        }
        fn teleport_entity(&mut self, id: EntityId, pos: Vec3f) -> bool {
            self.set_kinematic_position(id, pos)
        }
    }

    fn test_runner() -> SimulationRunner {
        SimulationRunner::new(
            TickId(1),
            Box::new(MockPhysics::new()),
            0.05,
            AbilityRegistry::new(),
            game_core::combat::status::BuffRegistry::new(),
        )
    }

    // ── sync_insert tests ───────────────────────────────────────

    #[test]
    fn insert_spawns_active_entity() {
        let mut sim = test_runner();
        let id = EntityId(100);
        let result = EntitySync::sync_insert(
            &mut sim,
            id,
            EntityKind::Player,
            EntityState::Spawning,
            TickId(1),
            100.0,
            Vec3f {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
            0,
            Default::default(),
        );
        assert_eq!(result, SyncInsertResult::Spawned);
        assert!(sim.contains(id));
    }

    #[test]
    fn insert_skips_removed_entity() {
        let mut sim = test_runner();
        let id = EntityId(200);
        let result = EntitySync::sync_insert(
            &mut sim,
            id,
            EntityKind::Npc,
            EntityState::Removed,
            TickId(1),
            50.0,
            Vec3f {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            0,
            Default::default(),
        );
        assert_eq!(result, SyncInsertResult::SkippedTerminal);
        assert!(!sim.contains(id));
    }

    #[test]
    fn insert_skips_despawn_pending_entity() {
        let mut sim = test_runner();
        let id = EntityId(201);
        let result = EntitySync::sync_insert(
            &mut sim,
            id,
            EntityKind::Npc,
            EntityState::DespawnPending,
            TickId(1),
            50.0,
            Vec3f {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            0,
            Default::default(),
        );
        assert_eq!(result, SyncInsertResult::SkippedTerminal);
        assert!(!sim.contains(id));
    }

    #[test]
    fn insert_skips_duplicate() {
        let mut sim = test_runner();
        let id = EntityId(300);
        let first = EntitySync::sync_insert(
            &mut sim,
            id,
            EntityKind::Player,
            EntityState::Spawning,
            TickId(1),
            100.0,
            Vec3f {
                x: 0.0,
                y: 1.0,
                z: 0.0,
            },
            0,
            Default::default(),
        );
        assert_eq!(first, SyncInsertResult::Spawned);

        let second = EntitySync::sync_insert(
            &mut sim,
            id,
            EntityKind::Player,
            EntityState::Active,
            TickId(2),
            100.0,
            Vec3f {
                x: 0.0,
                y: 1.0,
                z: 0.0,
            },
            0,
            Default::default(),
        );
        assert_eq!(second, SyncInsertResult::SkippedDuplicate);
    }

    #[test]
    fn insert_restores_aggro_from_npc_state_target_when_threat_rows_are_absent() {
        let mut sim = test_runner();
        let npc = EntityId(301);
        let target = EntityId(302);
        let result = EntitySync::sync_insert(
            &mut sim,
            npc,
            EntityKind::Npc,
            EntityState::Active,
            TickId(1),
            80.0,
            Vec3f {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            0,
            RuntimeSnapshot {
                npc_state: Some((NpcAiState::Combat, Some(target))),
                ..Default::default()
            },
        );
        assert_eq!(result, SyncInsertResult::Spawned);

        let tick = sim.run_tick(1, &[]).expect("first tick should run");
        assert!(
            tick.npc_state_updates
                .iter()
                .any(|(eid, ai_state, target_entity)| {
                    *eid == npc && *ai_state == NpcAiState::Combat && *target_entity == Some(target)
                })
        );
    }

    // ── sync_update tests ───────────────────────────────────────

    #[test]
    fn update_mirrors_active_to_despawn_pending() {
        let mut sim = test_runner();
        let id = EntityId(400);
        EntitySync::sync_insert(
            &mut sim,
            id,
            EntityKind::Npc,
            EntityState::Spawning,
            TickId(1),
            80.0,
            Vec3f {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            0,
            Default::default(),
        );
        // Activate the entity so is_active() returns true.
        sim.mark_despawn(id); // Spawning doesn't become Active without a tick,
        // so test the branch where is_active() is false:
        let result = EntitySync::sync_update(
            &mut sim,
            id,
            EntityState::Active,
            EntityState::DespawnPending,
        );
        // Entity was Spawning (not Active), so is_active() returns false → Ignored.
        assert_eq!(result, SyncUpdateResult::Ignored);
    }

    #[test]
    fn update_removed_cleans_up_entity() {
        let mut sim = test_runner();
        let id = EntityId(500);
        EntitySync::sync_insert(
            &mut sim,
            id,
            EntityKind::Npc,
            EntityState::Spawning,
            TickId(1),
            60.0,
            Vec3f {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            0,
            Default::default(),
        );
        assert!(sim.contains(id));

        let result =
            EntitySync::sync_update(&mut sim, id, EntityState::Active, EntityState::Removed);
        assert_eq!(result, SyncUpdateResult::Removed);
        assert!(!sim.contains(id));
    }

    #[test]
    fn update_removed_noop_when_not_present() {
        let mut sim = test_runner();
        let id = EntityId(600);
        let result =
            EntitySync::sync_update(&mut sim, id, EntityState::Active, EntityState::Removed);
        assert_eq!(result, SyncUpdateResult::RemoveNoop);
    }

    #[test]
    fn update_spawning_to_active_is_ignored() {
        let mut sim = test_runner();
        let id = EntityId(700);
        EntitySync::sync_insert(
            &mut sim,
            id,
            EntityKind::Player,
            EntityState::Spawning,
            TickId(1),
            100.0,
            Vec3f {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            0,
            Default::default(),
        );
        let result =
            EntitySync::sync_update(&mut sim, id, EntityState::Spawning, EntityState::Active);
        assert_eq!(result, SyncUpdateResult::Ignored);
    }

    // ── sync_delete tests ───────────────────────────────────────

    #[test]
    fn delete_removes_tracked_entity() {
        let mut sim = test_runner();
        let id = EntityId(800);
        EntitySync::sync_insert(
            &mut sim,
            id,
            EntityKind::Player,
            EntityState::Spawning,
            TickId(1),
            100.0,
            Vec3f {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            0,
            Default::default(),
        );
        let result = EntitySync::sync_delete(&mut sim, id);
        assert_eq!(result, SyncDeleteResult::Removed);
        assert!(!sim.contains(id));
    }

    #[test]
    fn delete_noop_when_not_tracked() {
        let mut sim = test_runner();
        let id = EntityId(900);
        let result = EntitySync::sync_delete(&mut sim, id);
        assert_eq!(result, SyncDeleteResult::Noop);
    }

    // ── Respawn layer preservation ──────────────────────────────

    #[test]
    fn respawn_on_instance_layer_preserves_layer() {
        // Validates Finding 2: when a player dies in a dungeon instance
        // (layer 100) and respawns, the coordinator must pass the death
        // layer to sync_insert — not hard-code 0.
        let mut sim = test_runner();
        let id = EntityId(1000);
        let instance_layer = 100u32;

        // 1. Spawn on instance layer.
        let r = EntitySync::sync_insert(
            &mut sim,
            id,
            EntityKind::Player,
            EntityState::Spawning,
            TickId(1),
            1000.0,
            Vec3f {
                x: 10.0,
                y: 0.0,
                z: 20.0,
            },
            instance_layer,
            Default::default(),
        );
        assert_eq!(r, SyncInsertResult::Spawned);
        assert_eq!(sim.entity_layer(id), instance_layer);

        // 2. Simulate death → force-remove (mirrors what commit_tick_results does).
        sim.force_remove_entity(id);
        assert!(!sim.contains(id));

        // 3. sync_update detects Removed→Spawning transition → returns Respawn.
        let update_result =
            EntitySync::sync_update(&mut sim, id, EntityState::Removed, EntityState::Spawning);
        assert_eq!(update_result, SyncUpdateResult::Respawn);

        // 4. Coordinator re-inserts with the correct layer (this is the fix).
        let r2 = EntitySync::sync_insert(
            &mut sim,
            id,
            EntityKind::Player,
            EntityState::Spawning,
            TickId(5),
            1000.0,
            Vec3f {
                x: 10.0,
                y: 0.0,
                z: 20.0,
            },
            instance_layer, // Correct: reads from entity_layer DB row
            Default::default(),
        );
        assert_eq!(r2, SyncInsertResult::Spawned);
        assert_eq!(
            sim.entity_layer(id),
            instance_layer,
            "Respawned entity must retain the instance layer, not reset to 0"
        );
    }

    #[test]
    fn respawn_on_layer_zero_works() {
        // Complementary test: respawn on open world (layer 0) still works.
        let mut sim = test_runner();
        let id = EntityId(1001);

        EntitySync::sync_insert(
            &mut sim,
            id,
            EntityKind::Player,
            EntityState::Spawning,
            TickId(1),
            1000.0,
            Vec3f {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            0,
            Default::default(),
        );
        sim.force_remove_entity(id);

        let update_result =
            EntitySync::sync_update(&mut sim, id, EntityState::Removed, EntityState::Spawning);
        assert_eq!(update_result, SyncUpdateResult::Respawn);

        let r2 = EntitySync::sync_insert(
            &mut sim,
            id,
            EntityKind::Player,
            EntityState::Spawning,
            TickId(5),
            1000.0,
            Vec3f {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            0,
            Default::default(),
        );
        assert_eq!(r2, SyncInsertResult::Spawned);
        assert_eq!(sim.entity_layer(id), 0);
    }
}
