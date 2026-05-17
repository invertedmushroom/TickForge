use super::*;

impl TickPipeline {
    // ── Transform delta collection ──────────────────────────────

    /// Collect only transforms that changed since the last emitted commit payload.
    ///
    /// The physics world remains the authoritative source of current transforms,
    /// so we still snapshot all transforms for lag compensation and region checks.
    /// Only the reducer payload is delta-compressed.
    pub(super) fn collect_transform_updates(
        &mut self,
        transforms: &[(EntityId, Transform)],
    ) -> Vec<(EntityId, Transform)> {
        let mut out = Vec::new();
        out.reserve(transforms.len());

        for &(eid, tf) in transforms {
            let changed = self.last_committed_transforms.get(&eid) != Some(&tf);
            if changed {
                self.last_committed_transforms.insert(eid, tf);
                out.push((eid, tf));
            }
        }

        out
    }

    // ── Health delta collection ─────────────────────────────────

    /// Snapshot current hp/max_hp for every entity that received a Damage event this tick.
    ///
    /// Must be called AFTER all combat phases (1-7) and BEFORE `phase_state_finalization`,
    /// because finalization removes dead entities from the EntityStore. Dead entities (hp=0)
    /// are intentionally included so the DB commit reflects their final health atomically
    /// with the EntityDied event in the same reducer call.
    pub(super) fn collect_health_updates(&self) -> Vec<(EntityId, f32, f32)> {
        let damaged: HashSet<EntityId> = self.pending_events.iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Damage { .. } | EventPayload::FallDamage { .. } => Some(e.entity_id),
                _ => None,
            })
            .collect();
        damaged.iter().filter_map(|&eid| {
            let idx = self.state.entities.lookup(eid)?;
            let i = idx.as_usize();
            Some((eid, self.state.combat.health.hp[i], self.state.combat.health.max_hp[i]))
        }).collect()
    }

    // ── Runtime domain snapshots ────────────────────────────────

    /// Collect buff updates for entities whose buff arrays changed this tick.
    ///
    /// Only dirty entities are emitted. Entities with zero remaining buffs are
    /// still included so the reducer's delete-all-then-insert reliably clears
    /// stale rows when every buff on an entity expires in the same tick.
    pub(super) fn collect_buff_updates(&mut self) -> Vec<(EntityId, Vec<game_core::combat::status::ActiveBuff>)> {
        let dirty = self.state.status.take_dirty();
        let mut out = Vec::with_capacity(dirty.len());
        for i in &dirty {
            if self.state.entities.states[*i] == game_core::entity::lifecycle::EntityState::Removed {
                continue;
            }
            // Mark buff-dirty entities for stat recalculation next tick.
            let eid = self.state.entities.id_of(self.state.entities.index_at(*i));
            self.stats_dirty.insert(eid);
            out.push((eid, self.state.status.clone_buffs(*i)));
        }
        out
    }

    /// Snapshot NPC AI state — only emits when state or target changed.
    pub(super) fn collect_npc_state_updates(&mut self) -> Vec<(EntityId, game_schema::NpcAiState, Option<EntityId>)> {
        let mut out = Vec::new();
        for (idx, &ai_state) in self.state.ai.npc_ai.iter() {
            if self.state.entities.states[idx.as_usize()] == game_core::entity::lifecycle::EntityState::Removed {
                continue;
            }
            let eid = self.state.entities.id_of(idx);
            let target = self.state.combat.threat_tables.get(idx)
                .and_then(|t| t.top_threat());
            let current = (ai_state, target);
            let changed = self.npc_state_prev.get(&eid) != Some(&current);
            if changed {
                self.npc_state_prev.insert(eid, current);
                out.push((eid, ai_state, target));
            }
        }
        out
    }

    /// Detect grid-cell transitions using the already-collected transforms.
    ///
    /// Compares each entity's current position against its stored `RegionCell`,
    /// applying hysteresis so oscillation at cell boundaries does not produce
    /// spurious updates. Only changed cells are emitted; the stored cell is
    /// updated in place so the next tick's comparison is correct.
    pub(super) fn collect_region_updates(
        &mut self,
        transforms: &[(EntityId, Transform)],
    ) -> Vec<(EntityId, RegionCell)> {
        let mut out = Vec::new();
        for &(eid, ref tf) in transforms {
            let pos = &tf.position;
            let new_cell = match self.entity_regions.get(&eid) {
                Some(current) => RegionCell::from_position_with_hysteresis(pos, current),
                // Entity not tracked yet (expected on first tick after spawn).
                None => RegionCell::from_position(pos),
            };
            let changed = self.entity_regions.get(&eid) != Some(&new_cell);
            if changed {
                self.entity_regions.insert(eid, new_cell);
                out.push((eid, new_cell));
            }
        }
        out
    }
}
