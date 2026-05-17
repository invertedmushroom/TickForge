//! Worker-side volume runtime: occupant sync, encounter integration, and
//! handlers for `EncounterOutput::SpawnVolume` / `DespawnVolume`.
//!
//! Volume lifetimes are scoped to their owning boss encounter. Storage is a
//! `HashMap<boss_id, VolumeStore>` on `TickPipeline`. World sensors are
//! created/destroyed via the standard `PhysicsBackend` sensor APIs and
//! tagged with `ColliderKind::Volume(volume_id)` so future systems can
//! distinguish them from hitboxes / hurtboxes when scanning collision
//! events.

use std::collections::HashMap;

use game_core::encounter::{VolumeAnchor, VolumeRuleEvent};
use game_core::physics_backend::SensorShape;
use game_core::volume::{EntityKindFilter, Volume, VolumeId, VolumeShape};
use game_protocol::entity_id::EntityId;
use game_protocol::event::EventPayload;
use game_protocol::types::Vec3f;

use super::{ColliderKind, TickId, TickPipeline};

impl TickPipeline {
    /// Drain queued [`VolumeRuleEvent`]s for a single boss. Called once per
    /// encounter rule evaluation; the queue is rebuilt next tick by
    /// `phase_volume_sync`.
    pub(super) fn volumes_take_rule_events(&mut self, boss_id: EntityId) -> Vec<VolumeRuleEvent> {
        self.pending_volume_events
            .remove(&boss_id)
            .unwrap_or_default()
    }

    /// Snapshot `tag → distinct-occupant-count` for this boss's volumes.
    /// Used by `Cond::OccupancyCmp`. Same entity in two volumes with the
    /// same tag is counted once.
    pub(super) fn volumes_occupancy_by_tag(&self, boss_id: EntityId) -> HashMap<String, u32> {
        let mut out: HashMap<String, u32> = HashMap::new();
        let Some(store) = self.volumes.get(&boss_id) else {
            return out;
        };
        // Collect distinct occupants per tag (deduplicated across volumes).
        let mut by_tag: HashMap<String, std::collections::BTreeSet<EntityId>> = HashMap::new();
        for v in store.iter_sorted() {
            let entry = by_tag.entry(v.tag.clone()).or_default();
            for e in &v.occupants {
                entry.insert(*e);
            }
        }
        for (tag, set) in by_tag {
            out.insert(tag, set.len() as u32);
        }
        out
    }

    /// Snapshot `tag -> distinct occupants` for this boss's volumes.
    /// Used by rule-visible set-membership and buff conditions.
    pub(super) fn volumes_occupants_by_tag_map(
        &self,
        boss_id: EntityId,
    ) -> HashMap<String, Vec<EntityId>> {
        let mut out: HashMap<String, Vec<EntityId>> = HashMap::new();
        let Some(store) = self.volumes.get(&boss_id) else {
            return out;
        };
        let mut by_tag: HashMap<String, std::collections::BTreeSet<EntityId>> = HashMap::new();
        for v in store.iter_sorted() {
            let entry = by_tag.entry(v.tag.clone()).or_default();
            for e in &v.occupants {
                entry.insert(*e);
            }
        }
        for (tag, set) in by_tag {
            out.insert(tag, set.into_iter().collect());
        }
        out
    }

    /// Phase 7.5a: refresh occupants for every live volume, emit
    /// `VolumeEnter`/`VolumeExit` simulation events on the entering /
    /// exiting entity, and queue per-encounter `VolumeRuleEvent`s for the
    /// rule evaluator to consume in Phase 7.5b. Also expires volumes whose
    /// `expires_at` has been reached.
    pub(super) fn phase_volume_sync(&mut self) {
        if self.volumes.is_empty() {
            return;
        }
        // Two-stage so we can drop the immutable borrow on `self.physics`
        // before mutating `self.pending_events` / `self.volumes`.
        let boss_ids: Vec<EntityId> = {
            let mut ids: Vec<EntityId> = self.volumes.keys().copied().collect();
            ids.sort_by_key(|e| e.0);
            ids
        };
        let current_tick = self.current_tick;

        for boss_id in boss_ids {
            // ── Expire volumes whose lifetime has ended. ──────────────
            let expired: Vec<VolumeId> = match self.volumes.get(&boss_id) {
                Some(store) => store
                    .iter_sorted()
                    .iter()
                    .filter(|v| match v.expires_at {
                        Some(t) => current_tick.0 >= t.0,
                        None => false,
                    })
                    .map(|v| v.id)
                    .collect(),
                None => Vec::new(),
            };
            for id in expired {
                self.despawn_volume_internal(boss_id, id);
            }

            // ── Track follow-owner volumes to the boss. ───────────────
            // Re-resolve `boss` position once per boss; sweep all volumes
            // owned by it that opted into `FollowBoss` and update their
            // sensor + cached position.
            if let Some(transform) = self.physics.get_transform(boss_id) {
                let follow_updates: Vec<(VolumeId, u64)> = match self.volumes.get(&boss_id) {
                    Some(store) => store
                        .iter_sorted()
                        .iter()
                        .filter(|v| v.follow_owner)
                        .map(|v| (v.id, v.sensor_handle))
                        .collect(),
                    None => Vec::new(),
                };
                for (id, handle) in follow_updates {
                    self.physics.set_sensor_position(handle, transform.position);
                    if let Some(store) = self.volumes.get_mut(&boss_id) {
                        if let Some(v) = store.get_mut(id) {
                            v.position = transform.position;
                        }
                    }
                }
            }

            // ── Sync remaining volumes. ───────────────────────────────
            // Collect (volume_id, sensor_handle, tag, owner, filter) so we
            // can release the borrow before calling sensor_intersections +
            // update_occupants. The filter is applied here so edge events
            // and OccupancyCmp see only matching entities.
            let snapshots: Vec<(
                VolumeId,
                u64,
                String,
                Option<EntityId>,
                EntityKindFilter,
                VolumeShape,
                Vec3f,
                u32,
            )> = match self.volumes.get(&boss_id) {
                Some(store) => store
                    .iter_sorted()
                    .iter()
                    .map(|v| {
                        (
                            v.id,
                            v.sensor_handle,
                            v.tag.clone(),
                            v.owner,
                            v.entity_filter.clone(),
                            v.shape,
                            v.position,
                            v.layer,
                        )
                    })
                    .collect(),
                None => Vec::new(),
            };
            let mut diffs: Vec<(VolumeId, String, Vec<EntityId>, Vec<EntityId>)> =
                Vec::with_capacity(snapshots.len());
            for (id, handle, tag, owner, filter, shape, position, volume_layer) in snapshots {
                let raw_occupants = self.physics.sensor_intersections(handle);
                let filtered: Vec<EntityId> = raw_occupants
                    .into_iter()
                    .filter(|entity| {
                        let Some(idx) = self.state.entities.lookup(*entity) else {
                            return false;
                        };
                        if self.layer_of_idx(idx) != volume_layer {
                            return false;
                        }
                        let kind = self.state.entities.kinds[idx.as_usize()];
                        filter.accepts(kind, *entity, owner)
                            && self.volume_shape_accepts(shape, position, *entity)
                    })
                    .collect();
                let store = self
                    .volumes
                    .get_mut(&boss_id)
                    .expect("store present (we just iterated it)");
                if let Some((entered, exited)) = store.update_occupants(id, filtered) {
                    diffs.push((id, tag, entered, exited));
                }
            }
            for (id, tag, entered, exited) in diffs {
                let queue = self.pending_volume_events.entry(boss_id).or_default();
                for entity in &entered {
                    queue.push(VolumeRuleEvent::Enter {
                        tag: tag.clone(),
                        entity: *entity,
                        volume_id: id,
                    });
                }
                for entity in &exited {
                    queue.push(VolumeRuleEvent::Exit {
                        tag: tag.clone(),
                        entity: *entity,
                        volume_id: id,
                    });
                }
                for entity in entered {
                    self.emit_event(
                        entity,
                        EventPayload::VolumeEnter {
                            volume_id: id.0,
                            entity,
                        },
                    );
                }
                for entity in exited {
                    self.emit_event(
                        entity,
                        EventPayload::VolumeExit {
                            volume_id: id.0,
                            entity,
                        },
                    );
                }
            }
        }
    }

    /// Spawn a volume owned by `boss_id`. Returns the new [`VolumeId`].
    pub(super) fn spawn_volume(
        &mut self,
        boss_id: EntityId,
        tag: String,
        shape: VolumeShape,
        position: Vec3f,
        lifetime_ticks: Option<u32>,
        entity_filter: EntityKindFilter,
        follow_owner: bool,
    ) -> VolumeId {
        let layer = self.layer_of(boss_id);
        let store = self.volumes.entry(boss_id).or_default();
        let id = store.allocate_id();
        let sensor_shape = match shape {
            VolumeShape::Sphere { radius } => SensorShape::Sphere { radius },
            VolumeShape::Capsule {
                half_height,
                radius,
            } => SensorShape::Capsule {
                half_height,
                radius,
            },
            VolumeShape::Ring {
                outer_radius,
                half_height,
                ..
            } => SensorShape::Capsule {
                half_height,
                radius: outer_radius,
            },
        };
        let handle = self.physics.spawn_world_sensor(
            position,
            sensor_shape,
            ColliderKind::Volume(id.0),
            boss_id,
        );
        let expires_at =
            lifetime_ticks.map(|n| TickId(self.current_tick.0.saturating_add(n as u64)));
        store.insert(Volume {
            id,
            tag,
            shape,
            position,
            layer,
            sensor_handle: handle,
            spawned_at: self.current_tick,
            expires_at,
            occupants: Vec::new(),
            owner: Some(boss_id),
            entity_filter,
            follow_owner,
        });
        id
    }

    fn volume_shape_accepts(&self, shape: VolumeShape, center: Vec3f, entity: EntityId) -> bool {
        match shape {
            VolumeShape::Sphere { .. } | VolumeShape::Capsule { .. } => true,
            VolumeShape::Ring {
                inner_radius,
                outer_radius,
                half_height,
            } => {
                let Some(transform) = self.physics.get_transform(entity) else {
                    return false;
                };
                let dx = transform.position.x - center.x;
                let dz = transform.position.z - center.z;
                let dy = (transform.position.y - center.y).abs();
                let dist_sq = dx * dx + dz * dz;
                dy <= half_height
                    && dist_sq >= inner_radius * inner_radius
                    && dist_sq <= outer_radius * outer_radius
            }
        }
    }

    pub(super) fn volumes_occupants_by_tag(&self, boss_id: EntityId, tag: &str) -> Vec<EntityId> {
        let Some(store) = self.volumes.get(&boss_id) else {
            return Vec::new();
        };
        let mut occupants = std::collections::BTreeSet::new();
        for v in store.iter_sorted() {
            if v.tag == tag {
                for entity in &v.occupants {
                    occupants.insert(*entity);
                }
            }
        }
        occupants.into_iter().collect()
    }

    /// Resolve a [`VolumeAnchor`] against the current sim state for this boss.
    /// Returns `None` if the boss has no transform (e.g. just despawned).
    pub(super) fn resolve_volume_anchor(
        &self,
        boss_id: EntityId,
        anchor: &VolumeAnchor,
    ) -> Option<Vec3f> {
        match anchor {
            VolumeAnchor::World { position } => Some(Vec3f {
                x: position[0],
                y: position[1],
                z: position[2],
            }),
            VolumeAnchor::Boss => self.physics.get_transform(boss_id).map(|t| t.position),
            VolumeAnchor::FollowBoss => self.physics.get_transform(boss_id).map(|t| t.position),
            VolumeAnchor::BossOffset { offset } => {
                self.physics.get_transform(boss_id).map(|t| Vec3f {
                    x: t.position.x + offset[0],
                    y: t.position.y + offset[1],
                    z: t.position.z + offset[2],
                })
            }
        }
    }

    /// Despawn all volumes matching `tag` for this boss. Removes their
    /// physics sensors and emits `VolumeExit` for any remaining occupants.
    pub(super) fn despawn_volumes_by_tag(&mut self, boss_id: EntityId, tag: &str) {
        let removed: Vec<Volume> = match self.volumes.get_mut(&boss_id) {
            Some(store) => store.remove_by_tag(tag),
            None => return,
        };
        let current_tick = self.current_tick;
        for volume in removed {
            self.physics.remove_sensor(volume.sensor_handle);
            for entity in &volume.occupants {
                self.emit_event(
                    *entity,
                    EventPayload::VolumeExit {
                        volume_id: volume.id.0,
                        entity: *entity,
                    },
                );
                self.pending_volume_events.entry(boss_id).or_default().push(
                    VolumeRuleEvent::Exit {
                        tag: volume.tag.clone(),
                        entity: *entity,
                        volume_id: volume.id,
                    },
                );
            }
            log::debug!(
                "Volume despawn: boss {} tag '{}' id {} at tick {}",
                boss_id.0,
                volume.tag,
                volume.id.0,
                current_tick.0,
            );
        }
    }

    /// Despawn a single volume by id (used by lifetime expiry).
    fn despawn_volume_internal(&mut self, boss_id: EntityId, id: VolumeId) {
        let removed: Option<Volume> = match self.volumes.get_mut(&boss_id) {
            Some(store) => store.remove(id),
            None => return,
        };
        if let Some(volume) = removed {
            self.physics.remove_sensor(volume.sensor_handle);
            for entity in &volume.occupants {
                self.emit_event(
                    *entity,
                    EventPayload::VolumeExit {
                        volume_id: volume.id.0,
                        entity: *entity,
                    },
                );
                self.pending_volume_events.entry(boss_id).or_default().push(
                    VolumeRuleEvent::Exit {
                        tag: volume.tag.clone(),
                        entity: *entity,
                        volume_id: volume.id,
                    },
                );
            }
        }
    }

    /// Tear down every volume owned by `boss_id` and free the `VolumeStore`
    /// entry. Called from `force_remove_entity` when the owning boss is
    /// removed.
    pub(super) fn drop_volumes_for_boss(&mut self, boss_id: EntityId) {
        let Some(store) = self.volumes.remove(&boss_id) else {
            return;
        };
        for volume in store.iter_sorted() {
            self.physics.remove_sensor(volume.sensor_handle);
        }
        self.pending_volume_events.remove(&boss_id);
    }
}
