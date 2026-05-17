use super::*;

impl TickPipeline {
    // ── Phase 7: AI decisions ───────────────────────────────────

    pub(super) fn phase_ai_decisions(&mut self) {
        use game_core::entity::lifecycle::{EntityKind, NpcAiState};
        use game_core::combat::status::AiOverride;

        let npcs = self.state.active_indices_of_kind(EntityKind::Npc);
        let bosses = self.state.active_indices_of_kind(EntityKind::Boss);

        for idx in npcs.iter().chain(bosses.iter()) {
            // Passive NPCs never run AI (training dummies).
            if self.state.ai.npc_passive.get(*idx).copied() == Some(true) {
                continue;
            }

            let ai_state = match self.state.ai.npc_ai.get(*idx).copied() {
                Some(s) => s,
                None => continue,
            };

            // Check for an ai_override carried by an active buff.
            // First buff with a non-None override wins; check runs before standard AI logic.
            let override_opt = self.state.status.get_buffs(*idx)
                .iter()
                .find_map(|b| b.modifiers.ai_override);

            // 1) State transitions: apply override if present, otherwise run normal transitions.
            //
            // `override_applied` tracks whether the override produced a state change.
            // ForceFocus with a dead target sets this to false so the standard
            // transition logic below can run — otherwise the NPC freezes.
            let mut override_applied = false;
            if let Some(ai_override) = override_opt {
                match ai_override {
                    AiOverride::ForceFlee => {
                        if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) { *ai = NpcAiState::Flee; }
                        audit!(self.state, Ai, AiDecisions, 7, None, "override_flee");
                        override_applied = true;
                    }
                    AiOverride::ForceIdle => {
                        if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) { *ai = NpcAiState::Idle; }
                        audit!(self.state, Ai, AiDecisions, 7, None, "override_idle");
                        override_applied = true;
                    }
                    AiOverride::ForceFocus { target } => {
                        // Only force-focus if the target is still alive — the buff
                        // may outlive the target entity. When the target is dead,
                        // fall through to standard AI transitions below.
                        if self.state.entities.lookup(target).is_none() {
                            audit!(self.state, Ai, AiDecisions, 7, None, "override_focus_dead_target");
                            // override_applied stays false → standard transitions run below.
                        } else {
                            if let Some(table) = self.state.combat.threat_tables.get_mut(*idx) {
                                let max_other_threat = table.entries.iter()
                                    .filter(|e| e.source != target)
                                    .filter(|e| e.threat.is_finite())
                                    .map(|e| e.threat)
                                    .fold(0.0f32, f32::max);
                                let forced_threat = max_other_threat + 10.0;
                                if let Some(entry) = table.entries.iter_mut().find(|e| e.source == target) {
                                    entry.threat = entry.threat.max(forced_threat);
                                } else {
                                    table.entries.push(game_core::combat::status::ThreatEntry {
                                        source: target,
                                        threat: forced_threat,
                                    });
                                }
                            }
                            if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) { *ai = NpcAiState::Combat; }
                            audit!(self.state, Threat, AiDecisions, 7, None, "override_focus_threat");
                            audit!(self.state, Ai, AiDecisions, 7, None, "override_focus");
                            override_applied = true;
                        }
                    }
                }
            }
            if !override_applied {
                // Standard state transitions (do not perform movement here).
                match ai_state {
                    NpcAiState::Idle | NpcAiState::Patrol => {
                        // Check if anyone is on the threat table → transition to Combat.
                        if let Some(table) = self.state.combat.threat_tables.get(*idx)
                            && table.top_threat().is_some() {
                                if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) { *ai = NpcAiState::Combat; }
                                audit!(self.state, Ai, AiDecisions, 7, None, "to_combat");
                            }
                    }
                    NpcAiState::Combat => {
                        // If threat table is empty, return to Idle.
                        let top_target = self.state.combat.threat_tables.get(*idx)
                            .and_then(|t| t.top_threat());
                        match top_target {
                            None => {
                                if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) { *ai = NpcAiState::Idle; }
                                audit!(self.state, Ai, AiDecisions, 7, None, "combat_to_idle");
                            }
                            Some(_) => {
                                // stay in Combat; action execution below will handle movement
                            }
                        }
                    }
                    NpcAiState::Flee => {
                        // Flee behavior: remain fleeing while any top threat exists.
                        // Only return to Idle when no threats remain.
                        let top_target = self.state.combat.threat_tables.get(*idx)
                            .and_then(|t| t.top_threat());
                        if top_target.is_none() {
                            if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) { *ai = NpcAiState::Idle; }
                            audit!(self.state, Ai, AiDecisions, 7, None, "flee_to_idle");
                        }
                    }
                    NpcAiState::Scripted => {
                        // No scripted behavior yet — placeholder.
                    }
                }
            }

            // 2) Action execution: run movement/behavior for the current state.
            //
            // CC-disabled NPCs (stunned, knocked down, launched, sleeping, feared)
            // must not move or cast. Movement is also blocked by `is_rooted` inside
            // `npc_step_toward`, but we skip the entire block here to also prevent
            // casting while CC'd. Silenced NPCs can still move but cannot cast.
            let cc_disabled = self.is_cc_disabled(*idx);
            if cc_disabled {
                continue;
            }
            let silenced = self.is_silenced(*idx);

            let current_state = self.state.ai.npc_ai.get(*idx).copied().unwrap();
            match current_state {
                NpcAiState::Combat => {
                    if let Some(target_id) = self.state.combat.threat_tables.get(*idx).and_then(|t| t.top_threat()) {
                        let npc_id = self.state.entities.id_of(*idx);
                        let no_chase = self.state.ai.npc_no_chase.get(*idx).copied() == Some(true);
                        if !no_chase {
                            self.npc_move_toward(npc_id, *idx, target_id, self.dt);
                            audit!(self.state, Transform, AiDecisions, 7, Some(npc_id), "chase");
                        }

                        // Cast from this NPC's configured ability list.
                        // Silenced NPCs can move but cannot cast.
                        if !silenced {
                            let ability_ids = self.state.ai.npc_ability_ids.get(*idx)
                                .cloned()
                                .unwrap_or_else(|| vec![1]);
                            let targeting = ResolvedTargeting::Entity { target: target_id };
                            for &aid in &ability_ids {
                                if self.cast_ability(npc_id, aid, targeting.clone(), 0, 0) {
                                    audit!(self.state, Execution, AiDecisions, 7, Some(npc_id), "npc_cast");
                                    break; // one cast per tick
                                }
                            }
                        }
                    }
                }
                NpcAiState::Flee => {
                    if let Some(threat_source) = self.state.combat.threat_tables.get(*idx).and_then(|t| t.top_threat()) {
                        let npc_id = self.state.entities.id_of(*idx);
                        self.npc_move_away(npc_id, *idx, threat_source, self.dt);
                        audit!(self.state, Transform, AiDecisions, 7, Some(npc_id), "flee");
                    }
                }
                NpcAiState::Patrol => {
                    if let Some(&home) = self.state.ai.home_positions.get(*idx) {
                        let npc_id = self.state.entities.id_of(*idx);
                        self.npc_move_toward_pos(npc_id, home, self.dt);
                        audit!(self.state, Transform, AiDecisions, 7, Some(npc_id), "patrol");
                    }
                }
                _ => {}
            }
        }
    }

    // ── NPC movement helpers (Phase 7) ─────────────────────────

    /// Move `npc_id` one step toward `target_id`'s current physics position.
    /// Uses the NPC's authoritative base speed. No-ops if either transform is unavailable.
    fn npc_move_toward(&mut self, npc_id: EntityId, npc_idx: game_core::entity::entity_index::EntityIndex, target_id: EntityId, dt: f32) {
        let (npc_pos, target_pos) = match (
            self.physics.get_transform(npc_id),
            self.physics.get_transform(target_id),
        ) {
            (Some(a), Some(b)) => (a.position, b.position),
            _ => return,
        };
        // Stop if already within melee range to prevent overshoot jitter.
        const CHASE_ARRIVE_RADIUS: f32 = 1.0;
        let dx = target_pos.x - npc_pos.x;
        let dz = target_pos.z - npc_pos.z;
        if dx * dx + dz * dz <= CHASE_ARRIVE_RADIUS * CHASE_ARRIVE_RADIUS {
            return;
        }
        self.npc_step_toward(npc_id, npc_idx, npc_pos, target_pos, dt);
    }

    /// Move `npc_id` one step away from `threat_source`'s current physics position.
    /// Uses the NPC's authoritative base speed. No-ops if either transform is unavailable.
    fn npc_move_away(&mut self, npc_id: EntityId, npc_idx: game_core::entity::entity_index::EntityIndex, threat_source: EntityId, dt: f32) {
        let (npc_pos, threat_pos) = match (
            self.physics.get_transform(npc_id),
            self.physics.get_transform(threat_source),
        ) {
            (Some(a), Some(b)) => (a.position, b.position),
            _ => return,
        };
        // Mirror the direction: flee = move in the direction opposite to the threat.
        let away = Vec3f {
            x: npc_pos.x + (npc_pos.x - threat_pos.x),
            y: npc_pos.y,
            z: npc_pos.z + (npc_pos.z - threat_pos.z),
        };
        self.npc_step_toward(npc_id, npc_idx, npc_pos, away, dt);
    }

    /// Move `npc_id` toward an explicit world position (used for patrol).
    /// No-ops when the NPC is already within `PATROL_ARRIVE_RADIUS` of the target.
    fn npc_move_toward_pos(&mut self, npc_id: EntityId, dest: Vec3f, dt: f32) {
        const PATROL_ARRIVE_RADIUS: f32 = 0.5;
        let npc_pos = match self.physics.get_transform(npc_id) {
            Some(t) => t.position,
            None => return,
        };
        // Stop if already close enough — prevents micro-jitter at the home point.
        let dx = dest.x - npc_pos.x;
        let dz = dest.z - npc_pos.z;
        if dx * dx + dz * dz <= PATROL_ARRIVE_RADIUS * PATROL_ARRIVE_RADIUS {
            return;
        }
        let idx = if let Some(idx) = self.state.entities.lookup(npc_id) {
            idx
        } else {
            return;
        };
        self.npc_step_toward(npc_id, idx, npc_pos, dest, dt);
    }

    /// Shared step: move `npc_id` from `from_pos` toward `to_pos` by one tick of movement.
    /// Uses cached `StatBlock::movement_speed` (recalculated in Phase 1.5).
    pub(super) fn npc_step_toward(&mut self, npc_id: EntityId, npc_idx: game_core::entity::entity_index::EntityIndex, from: Vec3f, to: Vec3f, dt: f32) {
        // Rooted NPCs cannot move.
        if self.is_rooted(npc_idx) {
            return;
        }
        let dx = to.x - from.x;
        let dz = to.z - from.z;
        let dist_sq = dx * dx + dz * dz;
        if dist_sq < 1e-6 {
            return;
        }
        let speed = self.state.stats.get(npc_idx).movement_speed;
        let dist = dist_sq.sqrt();
        // Clamp step to remaining distance so the NPC never overshoots the target.
        let move_dist = (speed * dt).min(dist);
        let inv = 1.0 / dist;
        let ground_pull = game_core::physics_constants::GROUND_PULL;
        let desired = Vec3f {
            x: dx * inv * move_dist,
            y: -ground_pull * dt,
            z: dz * inv * move_dist,
        };
        if let Some(result) = self.physics.move_character(npc_id, desired) {
            self.state.combat.tactical[npc_idx.as_usize()].is_grounded = result.grounded;
        }
    }

    // ── Phase 7.5: World orchestration ──────────────────────────

    /// Evaluate dynamic event triggers and spawn NPCs/bosses into the simulation.
    ///
    /// Sweeps `entity_regions` to count active players per region cell,
    /// evaluates all registered `DirectorState` events against those counts,
    /// and calls `spawn_entity_from_snapshot` for each spawn directive.
    ///
    /// Returns the list of spawns so Phase 10 can include them in the TickResult
    /// for the coordinator to persist as DB rows.
    pub(super) fn phase_world_orchestration(&mut self) -> Vec<DirectorSpawn> {
        // Build region player counts from current entity_regions.
        let mut region_player_counts: HashMap<(i32, i32, u32), u32> = HashMap::new();
        for (eid, cell) in &self.entity_regions {
            if let Some(idx) = self.state.entities.lookup(*eid) {
                let i = idx.as_usize();
                if self.state.entities.kinds[i] == game_core::entity::lifecycle::EntityKind::Player
                    && self.state.entities.states[i] == game_core::entity::lifecycle::EntityState::Active
                {
                    *region_player_counts
                        .entry((cell.region_x, cell.region_z, cell.layer))
                        .or_insert(0) += 1;
                }
            }
        }

        // Director spawns are deferred: we return the spawn requests so the
        // coordinator can send them to SpacetimeDB via commit_tick_results.
        // The DB assigns canonical IDs and broadcasts entity.on_insert, which
        // the coordinator handles to materialize them into the local sim.
        self.director.evaluate(&region_player_counts, self.current_tick)
    }

    /// Get mutable access to the director state for event registration.
    pub fn director_mut(&mut self) -> &mut DirectorState {
        &mut self.director
    }

    /// Get read access to the director state.
    pub fn director(&self) -> &DirectorState {
        &self.director
    }

    /// Mark an entity for stat recalculation on the next tick's Phase 1.5.
    ///
    /// Called by the coordinator (via `SimulationRunner`) when equipment changes
    /// are observed between ticks. Also used internally when buffs change.
    pub fn mark_stats_dirty(&mut self, entity_id: EntityId) {
        if self.state.entities.contains(entity_id) {
            self.stats_dirty.insert(entity_id);
        }
    }

    /// Update the aggregated equipment modifiers for an entity.
    ///
    /// Called by the coordinator when `player_equipment` rows change. The new
    /// modifiers take effect on the next `phase_stat_recalc` pass.
    pub fn set_equipment_modifiers(&mut self, entity_id: EntityId, modifiers: game_core::stats::EquipmentModifiers) {
        self.equipment_modifiers.insert(entity_id, modifiers);
    }

    /// Assign a weapon loadout to a player entity.
    ///
    /// Entities with a loadout have their `UseAbility` intents validated against
    /// the active weapon set. Entities without a loadout are unrestricted.
    pub fn set_weapon_loadout(&mut self, entity_id: EntityId, loadout: game_core::combat::loadout::WeaponLoadout) {
        if let Some(idx) = self.state.entities.lookup(entity_id) {
            self.state.combat.loadouts.insert(idx, loadout);
        }
    }

    // ── Phase 1.5: Stat recalculation ───────────────────────────

    /// Recalculate cached `StatBlock` for entities whose buffs or equipment changed.
    ///
    /// Drains `stats_dirty` and recomputes each entity's stats from its kind,
    /// spawn-time max_hp, and current active buffs using `StatBlock::compute`.
    pub(super) fn phase_stat_recalc(&mut self) {
        if self.stats_dirty.is_empty() {
            return;
        }
        let dirty: Vec<EntityId> = self.stats_dirty.drain().collect();
        let no_equip = game_core::stats::EquipmentModifiers::default();
        for eid in dirty {
            let Some(idx) = self.state.entities.lookup(eid) else { continue };
            let i = idx.as_usize();
            let kind = self.state.entities.kinds[i];
            let max_hp = self.state.combat.health.max_hp[i];
            let buffs = self.state.status.get_buffs(idx);
            let equip = self.equipment_modifiers.get(&eid).unwrap_or(&no_equip);
            let block = game_core::stats::StatBlock::compute(kind, max_hp, buffs, equip);
            self.state.stats.set(idx, block);
        }
    }
}
