use super::*;

impl TickPipeline {
    // ── Phase 7: AI decisions ───────────────────────────────────

    pub(super) fn phase_ai_decisions(&mut self) {
        use game_core::combat::status::{AiOverride, ThreatTable};
        use game_core::entity::lifecycle::{EntityKind, NpcAiState};

        let npcs = self.state.active_indices_of_kind(EntityKind::Npc);
        let bosses = self.state.active_indices_of_kind(EntityKind::Boss);
        // Pre-compute player indices once for proximity aggro scanning.
        let players = self.state.active_indices_of_kind(EntityKind::Player);

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
            let override_opt = self
                .state
                .status
                .get_buffs(*idx)
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
                        if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) {
                            *ai = NpcAiState::Flee;
                        }
                        audit!(self.state, Ai, AiDecisions, 7, None, "override_flee");
                        override_applied = true;
                    }
                    AiOverride::ForceIdle => {
                        if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) {
                            *ai = NpcAiState::Idle;
                        }
                        audit!(self.state, Ai, AiDecisions, 7, None, "override_idle");
                        override_applied = true;
                    }
                    AiOverride::ForceFocus { target } => {
                        // Only force-focus if the target is still alive — the buff
                        // may outlive the target entity. When the target is dead,
                        // fall through to standard AI transitions below.
                        if self.state.entities.lookup(target).is_none() {
                            audit!(
                                self.state,
                                Ai,
                                AiDecisions,
                                7,
                                None,
                                "override_focus_dead_target"
                            );
                            // override_applied stays false → standard transitions run below.
                        } else {
                            if let Some(table) = self.state.combat.threat_tables.get_mut(*idx) {
                                let max_other_threat = table
                                    .entries
                                    .iter()
                                    .filter(|e| e.source != target)
                                    .filter(|e| e.threat.is_finite())
                                    .map(|e| e.threat)
                                    .fold(0.0f32, f32::max);
                                let forced_threat = max_other_threat + 10.0;
                                if let Some(entry) =
                                    table.entries.iter_mut().find(|e| e.source == target)
                                {
                                    entry.threat = entry.threat.max(forced_threat);
                                } else {
                                    table.entries.push(game_core::combat::status::ThreatEntry {
                                        source: target,
                                        threat: forced_threat,
                                    });
                                }
                            }
                            if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) {
                                *ai = NpcAiState::Combat;
                            }
                            audit!(
                                self.state,
                                Threat,
                                AiDecisions,
                                7,
                                None,
                                "override_focus_threat"
                            );
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
                            && table.top_threat().is_some()
                        {
                            if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) {
                                *ai = NpcAiState::Combat;
                            }
                            audit!(self.state, Ai, AiDecisions, 7, None, "to_combat");
                        } else {
                            // Proximity aggro: scan for players within aggro_radius.
                            // TODO: O(NPCs × Players) — replace with spatial partitioning
                            // (e.g. region-cell query) when populations exceed ~50 idle NPCs.
                            let aggro = self
                                .state
                                .ai
                                .npc_aggro_radius
                                .get(*idx)
                                .copied()
                                .unwrap_or(0.0);
                            if aggro > 0.0 {
                                let npc_id = self.state.entities.id_of(*idx);
                                let npc_layer = self.layer_of_idx(*idx);
                                if let Some(npc_t) = self.physics.get_transform(npc_id) {
                                    let npc_pos = npc_t.position;
                                    let aggro_sq = aggro * aggro;
                                    for &p_idx in &players {
                                        let p_id = self.state.entities.id_of(p_idx);
                                        // Layer isolation: NPCs only aggro players on the same layer.
                                        if self.layer_of_idx(p_idx) != npc_layer {
                                            continue;
                                        }
                                        if let Some(p_t) = self.physics.get_transform(p_id) {
                                            let dx = p_t.position.x - npc_pos.x;
                                            let dz = p_t.position.z - npc_pos.z;
                                            if dx * dx + dz * dz <= aggro_sq {
                                                // Add initial threat + enter combat.
                                                if !self.state.combat.threat_tables.contains(*idx) {
                                                    self.state
                                                        .combat
                                                        .threat_tables
                                                        .insert(*idx, ThreatTable::default());
                                                }
                                                let table = self
                                                    .state
                                                    .combat
                                                    .threat_tables
                                                    .get_mut(*idx)
                                                    .unwrap();
                                                if table.entries.iter().all(|e| e.source != p_id) {
                                                    table.entries.push(
                                                        game_core::combat::status::ThreatEntry {
                                                            source: p_id,
                                                            threat: 1.0,
                                                        },
                                                    );
                                                }
                                                if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx)
                                                {
                                                    *ai = NpcAiState::Combat;
                                                }
                                                audit!(
                                                    self.state,
                                                    Ai,
                                                    AiDecisions,
                                                    7,
                                                    Some(npc_id),
                                                    "aggro_proximity"
                                                );
                                                break; // One target is enough to enter combat.
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    NpcAiState::Combat => {
                        // If threat table is empty, return to Idle.
                        let top_target = self
                            .state
                            .combat
                            .threat_tables
                            .get(*idx)
                            .and_then(|t| t.top_threat());
                        match top_target {
                            None => {
                                if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) {
                                    *ai = NpcAiState::Idle;
                                }
                                audit!(self.state, Ai, AiDecisions, 7, None, "combat_to_idle");
                            }
                            Some(_) => {
                                // Leash check: if NPC exceeds leash radius from home → Evade.
                                let leash = self
                                    .state
                                    .ai
                                    .npc_leash_radius
                                    .get(*idx)
                                    .copied()
                                    .unwrap_or(0.0);
                                if leash > 0.0 {
                                    if let Some(&home) = self.state.ai.home_positions.get(*idx) {
                                        let npc_id = self.state.entities.id_of(*idx);
                                        if let Some(npc_t) = self.physics.get_transform(npc_id) {
                                            let dx = npc_t.position.x - home.x;
                                            let dz = npc_t.position.z - home.z;
                                            if dx * dx + dz * dz > leash * leash {
                                                if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx)
                                                {
                                                    *ai = NpcAiState::Evade;
                                                }
                                                audit!(
                                                    self.state,
                                                    Ai,
                                                    AiDecisions,
                                                    7,
                                                    Some(npc_id),
                                                    "leash_evade"
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    NpcAiState::Flee => {
                        // Flee behavior: remain fleeing while any top threat exists.
                        // Only return to Idle when no threats remain.
                        let top_target = self
                            .state
                            .combat
                            .threat_tables
                            .get(*idx)
                            .and_then(|t| t.top_threat());
                        if top_target.is_none() {
                            if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) {
                                *ai = NpcAiState::Idle;
                            }
                            audit!(self.state, Ai, AiDecisions, 7, None, "flee_to_idle");
                        }
                    }
                    NpcAiState::Scripted => {
                        // Scripted NPCs check npc_goals for directives.
                        // V1: "go_idle" causes transition back to Idle.
                        let npc_id = self.state.entities.id_of(*idx);
                        if let Some((goal_kind, _priority)) = self.npc_goals.get(&npc_id) {
                            match goal_kind.as_str() {
                                "go_idle" => {
                                    if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) {
                                        *ai = NpcAiState::Idle;
                                    }
                                    audit!(
                                        self.state,
                                        Ai,
                                        AiDecisions,
                                        7,
                                        Some(npc_id),
                                        "goal_idle"
                                    );
                                }
                                _ => {
                                    // Unknown goal kind — log and ignore.
                                    log::trace!(
                                        "NPC {} has unknown goal '{}'",
                                        npc_id.0,
                                        goal_kind
                                    );
                                }
                            }
                        }
                    }
                    NpcAiState::Evade => {
                        // Evade→Idle: handled in action execution when NPC reaches home.
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
                    // Sanitize threat table: remove entries for entities on a different layer
                    // or for non-combat entity kinds (Props should never be valid targets).
                    let npc_id = self.state.entities.id_of(*idx);
                    let npc_layer = self.layer_of_idx(*idx);
                    if let Some(table) = self.state.combat.threat_tables.get_mut(*idx) {
                        let entities = &self.state.entities;
                        let cache = &self.entity_layer_cache;
                        table.entries.retain(|e| {
                            entities.lookup(e.source).map_or(false, |src_idx| {
                                let slot = src_idx.as_usize();
                                let src_layer = if slot < cache.len() { cache[slot] } else { 0 };
                                let src_kind = entities.kinds[slot];
                                src_layer == npc_layer
                                    && src_kind != game_core::entity::lifecycle::EntityKind::Prop
                            })
                        });
                    }
                    if let Some(target_id) = self
                        .state
                        .combat
                        .threat_tables
                        .get(*idx)
                        .and_then(|t| t.top_threat())
                    {
                        let no_chase = self.state.ai.npc_no_chase.get(*idx).copied() == Some(true);
                        if !no_chase {
                            self.npc_move_toward(npc_id, *idx, target_id, self.dt);
                            audit!(self.state, Transform, AiDecisions, 7, Some(npc_id), "chase");
                        }

                        // Cast from this NPC's configured ability list.
                        // Silenced NPCs can move but cannot cast.
                        if !silenced {
                            let ability_ids = self
                                .state
                                .ai
                                .npc_ability_ids
                                .get(*idx)
                                .cloned()
                                .unwrap_or_else(|| vec![1]);
                            let targeting = ResolvedTargeting::Entity { target: target_id };
                            for &aid in &ability_ids {
                                if self.cast_ability(npc_id, aid, targeting.clone(), 0, 0) {
                                    audit!(
                                        self.state,
                                        Execution,
                                        AiDecisions,
                                        7,
                                        Some(npc_id),
                                        "npc_cast"
                                    );
                                    break; // one cast per tick
                                }
                            }
                        }
                    }
                }
                NpcAiState::Flee => {
                    if let Some(threat_source) = self
                        .state
                        .combat
                        .threat_tables
                        .get(*idx)
                        .and_then(|t| t.top_threat())
                    {
                        let npc_id = self.state.entities.id_of(*idx);
                        let npc_layer = self.layer_of_idx(*idx);
                        if self.layer_of(threat_source) != npc_layer {
                            // Threat source is on a different layer; skip flee movement.
                        } else {
                            self.npc_move_away(npc_id, *idx, threat_source, self.dt);
                            audit!(self.state, Transform, AiDecisions, 7, Some(npc_id), "flee");
                        }
                    }
                }
                NpcAiState::Patrol => {
                    if let Some(&home) = self.state.ai.home_positions.get(*idx) {
                        let npc_id = self.state.entities.id_of(*idx);
                        self.npc_move_toward_pos(npc_id, home, self.dt);
                        audit!(
                            self.state,
                            Transform,
                            AiDecisions,
                            7,
                            Some(npc_id),
                            "patrol"
                        );
                    }
                }
                NpcAiState::Evade => {
                    // Walk home, clear threat, reset HP on arrival.
                    let npc_id = self.state.entities.id_of(*idx);
                    if let Some(&home) = self.state.ai.home_positions.get(*idx) {
                        self.npc_move_toward_pos(npc_id, home, self.dt);
                        audit!(
                            self.state,
                            Transform,
                            AiDecisions,
                            7,
                            Some(npc_id),
                            "evade_walk"
                        );

                        // Check arrival.
                        if let Some(npc_t) = self.physics.get_transform(npc_id) {
                            let dx = npc_t.position.x - home.x;
                            let dz = npc_t.position.z - home.z;
                            const R: f32 = game_core::physics_constants::EVADE_ARRIVE_RADIUS;
                            if dx * dx + dz * dz <= R * R {
                                // Arrived home: clear threat, reset HP, return to Idle.
                                if let Some(table) = self.state.combat.threat_tables.get_mut(*idx) {
                                    table.entries.clear();
                                }
                                // Queue full heal — applied in Phase 8b so Health
                                // mutations stay centralised in combat/finalization.
                                let max_hp = self.state.combat.health.max_hp[idx.as_usize()];
                                let current_hp = self.state.combat.health.hp[idx.as_usize()];
                                if current_hp < max_hp {
                                    self.pending_heals
                                        .push((npc_id, max_hp - current_hp, npc_id));
                                }
                                if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) {
                                    *ai = NpcAiState::Idle;
                                }
                                audit!(self.state, Ai, AiDecisions, 7, Some(npc_id), "evade_home");
                            }
                        }
                    } else {
                        // No home recorded — snap to Idle.
                        if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) {
                            *ai = NpcAiState::Idle;
                        }
                    }
                    // Clear threat each tick while evading so NPC doesn't re-enter combat.
                    if let Some(table) = self.state.combat.threat_tables.get_mut(*idx) {
                        table.entries.clear();
                    }
                }
                _ => {}
            }
        }
    }

    // ── NPC movement helpers (Phase 7) ─────────────────────────

    /// Move `npc_id` one step toward `target_id`'s current physics position.
    /// Uses the NPC's authoritative base speed. No-ops if either transform is unavailable.
    fn npc_move_toward(
        &mut self,
        npc_id: EntityId,
        npc_idx: game_core::entity::entity_index::EntityIndex,
        target_id: EntityId,
        dt: f32,
    ) {
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
    fn npc_move_away(
        &mut self,
        npc_id: EntityId,
        npc_idx: game_core::entity::entity_index::EntityIndex,
        threat_source: EntityId,
        dt: f32,
    ) {
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
    pub(super) fn npc_step_toward(
        &mut self,
        npc_id: EntityId,
        npc_idx: game_core::entity::entity_index::EntityIndex,
        from: Vec3f,
        to: Vec3f,
        dt: f32,
    ) {
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
                    && self.state.entities.states[i]
                        == game_core::entity::lifecycle::EntityState::Active
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
        self.director
            .evaluate(&region_player_counts, self.current_tick, &self.world_phases)
    }

    /// Deterministic radial offset for scripted add spawns around the boss.
    fn encounter_spawn_offset(index: u32) -> Vec3f {
        if index == 0 {
            return Vec3f::ZERO;
        }
        let angle = (index as f32) * std::f32::consts::FRAC_PI_4;
        let radius = 2.5;
        Vec3f {
            x: angle.cos() * radius,
            y: 0.0,
            z: angle.sin() * radius,
        }
    }

    // ── Phase 7.5b: Encounter execution ─────────────────────────

    /// Evaluate encounter rules for all active boss encounters.
    ///
    /// For each boss encounter:
    /// 1. Tick active mechanics with `WorkerMechanicCtx`.
    /// 2. Evaluate rules → `Vec<EncounterOutput>`.
    /// 3. Forward `ChangeBossPhase` / `IncrementZoneCounter` to the commit
    ///    pipeline (Tier-2 reducer-owned tables).
    /// 4. Apply worker-owned effects directly: ability list replacement,
    ///    spawn-add requests, scripted casts, telegraph log lines, mechanic
    ///    start/stop.
    pub(super) fn phase_encounter_execution(
        &mut self,
    ) -> (
        Vec<game_core::encounter::EncounterOutput>,
        Vec<game_core::director::DirectorSpawn>,
        Vec<game_core::director::PendingAddMembership>,
    ) {
        let mut commit_outputs = Vec::new();
        let mut encounter_spawns = Vec::new();
        let mut pending_memberships: Vec<game_core::director::PendingAddMembership> =
            Vec::new();

        // Collect boss entity IDs first to avoid borrow issues.
        let mut boss_ids: Vec<EntityId> = self.encounters.keys().copied().collect();
        boss_ids.sort_by_key(|id| id.0);

        for boss_id in boss_ids {
            let Some(boss_idx) = self.state.entities.lookup(boss_id) else {
                // Boss entity no longer exists — clean up encounter.
                self.encounters.remove(&boss_id);
                self.drop_volumes_for_boss(boss_id);
                continue;
            };

            let i = boss_idx.as_usize();
            let hp = self.state.combat.health.hp[i];
            let max_hp = self.state.combat.health.max_hp[i];
            let hp_pct = if max_hp > 0.0 { hp / max_hp } else { 0.0 };

            // ── Mechanic tick pass ──────────────────────────────
            // Active mechanics get a `WorkerMechanicCtx` and may queue
            // arbitrary `Effect`s via `MechanicCtx::emit_effect`. Those
            // emissions are funneled through `apply_external_effects` so
            // they share the rule pipeline.
            let tick_emissions = {
                let mut active_mechanics = match self.encounters.get_mut(&boss_id) {
                    Some(enc) => std::mem::take(&mut enc.active_mechanics),
                    None => continue,
                };
                let emitted = {
                    let mut ctx = WorkerMechanicCtx::new(self, boss_id);
                    for mechanic in &mut active_mechanics {
                        mechanic.tick(&mut ctx);
                    }
                    ctx.take_emitted()
                };
                // Partition finished mechanics out so we can push
                // `MechanicEnded` onto the bus (the bus is what feeds the
                // `OnMechanicEnded` trigger on the next evaluate).
                let mut finished: Vec<(String, game_core::encounter::MechanicOutcome)> =
                    Vec::new();
                let mut live: Vec<Box<dyn game_core::encounter::Mechanic>> =
                    Vec::with_capacity(active_mechanics.len());
                for m in active_mechanics.into_iter() {
                    if m.is_finished() {
                        let name = m.name().to_string();
                        if !name.is_empty() {
                            let outcome = m
                                .outcome()
                                .unwrap_or(game_core::encounter::MechanicOutcome::Completed);
                            finished.push((name, outcome));
                        }
                    } else {
                        live.push(m);
                    }
                }
                if let Some(enc) = self.encounters.get_mut(&boss_id) {
                    enc.active_mechanics = live;
                    for (name, outcome) in finished {
                        enc.bus
                            .push(game_core::encounter::EncounterEvent::MechanicEnded {
                                name,
                                outcome,
                            });
                    }
                }
                emitted
            };

            let mut all_outputs: Vec<game_core::encounter::EncounterOutput> = Vec::new();
            if !tick_emissions.is_empty() {
                if let Some(enc) = self.encounters.get_mut(&boss_id) {
                    all_outputs
                        .extend(enc.apply_external_effects(tick_emissions, self.current_tick));
                }
            }

            // ── Rule evaluation ────────────────────────────────
            // Compute volume inputs before reborrowing encounters.
            let occupancy_by_tag = self.volumes_occupancy_by_tag(boss_id);
            let volume_events = self.volumes_take_rule_events(boss_id);
            let Some(enc) = self.encounters.get_mut(&boss_id) else {
                continue;
            };
            let eval_inputs = game_core::encounter::EncounterEvalInputs {
                volume_occupancy_by_tag: occupancy_by_tag
                    .iter()
                    .map(|(k, v)| (k.as_str(), *v))
                    .collect(),
                volume_events: &volume_events,
            };
            all_outputs.extend(enc.evaluate(hp_pct, self.current_tick, &eval_inputs));

            // ── Dispatch all outputs (rule + mechanic-emitted) ──
            self.dispatch_encounter_outputs(
                boss_id,
                all_outputs,
                &mut commit_outputs,
                &mut encounter_spawns,
                &mut pending_memberships,
                0,
            );
        }

        (commit_outputs, encounter_spawns, pending_memberships)
    }

    /// Maximum recursion depth for mechanic-emitted effects that themselves
    /// produce new outputs (e.g., a mechanic's `start()` emits another
    /// `StartMechanic`). Bounded to keep authoring mistakes survivable.
    const MECHANIC_DISPATCH_MAX_DEPTH: u8 = 4;

    /// Single dispatcher for `EncounterOutput`s, regardless of source
    /// (rules or mechanic emissions). Each variant routes to a focused
    /// per-variant handler. Mechanic-emitted effects from `start` /
    /// `on_event` callbacks are gathered and recursively dispatched up to
    /// `MECHANIC_DISPATCH_MAX_DEPTH`.
    fn dispatch_encounter_outputs(
        &mut self,
        boss_id: EntityId,
        outputs: Vec<game_core::encounter::EncounterOutput>,
        commit_outputs: &mut Vec<game_core::encounter::EncounterOutput>,
        encounter_spawns: &mut Vec<game_core::director::DirectorSpawn>,
        pending_memberships: &mut Vec<game_core::director::PendingAddMembership>,
        depth: u8,
    ) {
        use game_core::encounter::EncounterOutput as EO;

        if depth > Self::MECHANIC_DISPATCH_MAX_DEPTH {
            log::warn!(
                "Encounter: boss {} mechanic emission depth exceeded {} — dropping {} outputs",
                boss_id.0,
                Self::MECHANIC_DISPATCH_MAX_DEPTH,
                outputs.len()
            );
            return;
        }

        // 1) Tier-2 outputs forwarded to the commit pipeline.
        for output in &outputs {
            if matches!(
                output,
                EO::ChangeBossPhase { .. } | EO::IncrementZoneCounter { .. }
            ) {
                commit_outputs.push(output.clone());
            }
        }

        // 2) StartMechanic.
        let pending_starts: Vec<(String, game_core::encounter::MechanicParams)> = outputs
            .iter()
            .filter_map(|o| match o {
                EO::StartMechanic { name, params, .. } => Some((name.clone(), params.clone())),
                _ => None,
            })
            .collect();
        if !pending_starts.is_empty() {
            let start_emissions = self.apply_start_mechanic(boss_id, pending_starts);
            if !start_emissions.is_empty() {
                if let Some(enc) = self.encounters.get_mut(&boss_id) {
                    let nested =
                        enc.apply_external_effects(start_emissions, self.current_tick);
                    self.dispatch_encounter_outputs(
                        boss_id,
                        nested,
                        commit_outputs,
                        encounter_spawns,
                        pending_memberships,
                        depth + 1,
                    );
                }
            }
        }

        // 3) StopMechanic.
        for output in &outputs {
            if let EO::StopMechanic { name, .. } = output {
                let stop_emissions = self.apply_stop_mechanic(boss_id, name);
                if !stop_emissions.is_empty() {
                    if let Some(enc) = self.encounters.get_mut(&boss_id) {
                        let nested =
                            enc.apply_external_effects(stop_emissions, self.current_tick);
                        self.dispatch_encounter_outputs(
                            boss_id,
                            nested,
                            commit_outputs,
                            encounter_spawns,
                            pending_memberships,
                            depth + 1,
                        );
                    }
                }
            }
        }

        // 4) ReplaceAbilityList.
        for output in &outputs {
            if let EO::ReplaceAbilityList {
                boss_entity_id,
                ability_ids,
            } = output
            {
                self.apply_replace_ability_list(*boss_entity_id, ability_ids);
            }
        }

        // 5) SpawnAdds.
        for output in &outputs {
            if let EO::SpawnAdds {
                boss_entity_id,
                archetype,
                count,
                tags,
            } = output
            {
                self.apply_spawn_adds(
                    *boss_entity_id,
                    archetype,
                    *count,
                    tags,
                    encounter_spawns,
                    pending_memberships,
                );
            }
        }

        // 6) Telegraph.
        for output in &outputs {
            if let EO::Telegraph {
                boss_entity_id,
                skill_id,
                target,
                lead_ticks,
            } = output
            {
                self.apply_telegraph(*boss_entity_id, *skill_id, target.clone(), *lead_ticks);
            }
        }

        // 6b) SpawnVolume / DespawnVolume.
        for output in &outputs {
            match output {
                EO::SpawnVolume {
                    boss_entity_id,
                    tag,
                    shape,
                    anchor,
                    lifetime_ticks,
                    entity_filter,
                } => {
                    self.apply_spawn_volume(
                        *boss_entity_id,
                        tag,
                        *shape,
                        anchor,
                        *lifetime_ticks,
                        entity_filter.clone(),
                    );
                }
                EO::DespawnVolume {
                    boss_entity_id,
                    tag,
                } => {
                    self.despawn_volumes_by_tag(*boss_entity_id, tag);
                    log::info!(
                        "Encounter: boss {} despawned volumes tag='{}'",
                        boss_entity_id.0,
                        tag,
                    );
                }
                _ => {}
            }
        }

        // 7) CastSkill.
        for output in outputs {
            if let EO::CastSkill {
                boss_entity_id,
                skill_id,
                target,
            } = output
            {
                self.apply_cast_skill(boss_entity_id, skill_id, target);
            }
        }
    }

    /// Instantiate and start mechanics requested via `StartMechanic`.
    /// Returns any `Effect`s emitted by their `start()` callbacks so the
    /// dispatcher can route them through the encounter.
    fn apply_start_mechanic(
        &mut self,
        boss_id: EntityId,
        pending_starts: Vec<(String, game_core::encounter::MechanicParams)>,
    ) -> Vec<game_core::encounter::Effect> {
        let mut started: Vec<Box<dyn game_core::encounter::Mechanic>> = Vec::new();
        let tick_value = self.current_tick.0;
        let emissions = {
            let mut ctx = WorkerMechanicCtx::new(self, boss_id);
            for (name, params) in pending_starts {
                match ctx.pipeline.mechanics.instantiate(&name, &params) {
                    Some(mut mechanic) => {
                        mechanic.start(&mut ctx);
                        log::info!(
                            "Encounter: boss {} started mechanic '{}' at tick {}",
                            boss_id.0,
                            name,
                            tick_value
                        );
                        started.push(mechanic);
                    }
                    None => {
                        log::warn!(
                            "Encounter: boss {} requested unknown mechanic '{}'",
                            boss_id.0,
                            name
                        );
                    }
                }
            }
            ctx.take_emitted()
        };
        if let Some(enc) = self.encounters.get_mut(&boss_id) {
            enc.active_mechanics.extend(started);
        }
        emissions
    }

    /// Deliver a synthetic `mechanic:<name>:stop` event to active mechanics
    /// so they can finalize, then prune finished mechanics. Returns any
    /// `Effect`s emitted during the on_event callback.
    fn apply_stop_mechanic(
        &mut self,
        boss_id: EntityId,
        name: &str,
    ) -> Vec<game_core::encounter::Effect> {
        let event_name = format!("mechanic:{}:stop", name);
        let mut active_mechanics = match self.encounters.get_mut(&boss_id) {
            Some(enc) => std::mem::take(&mut enc.active_mechanics),
            None => return Vec::new(),
        };
        let emitted = {
            let mut ctx = WorkerMechanicCtx::new(self, boss_id);
            for mechanic in &mut active_mechanics {
                mechanic.on_event(&mut ctx, &event_name);
            }
            ctx.take_emitted()
        };
        // Finding #5: StopMechanic must surface a MechanicEnded event so that
        // `OnMechanicEnded` rules can observe cancellation. We push one event
        // per dropped mechanic with outcome=Cancelled. Mechanics that finished
        // naturally during this on_event will instead be reported as Completed
        // by the natural-completion path; here every drop is treated as a
        // cancellation since the rule explicitly requested a stop.
        let (still_active, dropped): (Vec<_>, Vec<_>) =
            active_mechanics.into_iter().partition(|m| !m.is_finished());
        if !dropped.is_empty() {
            if let Some(enc) = self.encounters.get_mut(&boss_id) {
                for m in &dropped {
                    enc.bus.push(game_core::encounter::EncounterEvent::MechanicEnded {
                        name: m.name().to_string(),
                        outcome: game_core::encounter::MechanicOutcome::Cancelled,
                    });
                }
            }
        }
        if let Some(enc) = self.encounters.get_mut(&boss_id) {
            enc.active_mechanics = still_active;
        }
        emitted
    }

    fn apply_replace_ability_list(
        &mut self,
        boss_entity_id: EntityId,
        ability_ids: &[u32],
    ) {
        let Some(idx) = self.state.entities.lookup(boss_entity_id) else {
            return;
        };
        self.state.ai.npc_ability_ids.remove(idx);
        self.state
            .ai
            .npc_ability_ids
            .insert(idx, ability_ids.to_vec());
        log::info!(
            "Encounter: boss {} replaced AI ability list with {} abilities",
            boss_entity_id.0,
            ability_ids.len()
        );
    }

    fn apply_spawn_adds(
        &mut self,
        boss_entity_id: EntityId,
        archetype: &str,
        count: u32,
        tags: &[String],
        encounter_spawns: &mut Vec<game_core::director::DirectorSpawn>,
        pending_memberships: &mut Vec<game_core::director::PendingAddMembership>,
    ) {
        let Some(profile) = self.npc_archetypes.lookup(archetype).cloned() else {
            log::warn!(
                "Encounter: boss {} requested unknown archetype '{}'",
                boss_entity_id.0,
                archetype
            );
            return;
        };
        let Some(transform) = self.physics.get_transform(boss_entity_id) else {
            log::warn!(
                "Encounter: boss {} spawn '{}' skipped (missing transform)",
                boss_entity_id.0,
                archetype
            );
            return;
        };
        let Some(idx) = self.state.entities.lookup(boss_entity_id) else {
            return;
        };

        let layer = self.layer_of_idx(idx);
        let spawn_count = count.min(64);
        if count > spawn_count {
            log::warn!(
                "Encounter: boss {} requested {} spawns; capped at {}",
                boss_entity_id.0,
                count,
                spawn_count
            );
        }

        // Validate tags against contract caps before queuing memberships.
        // Per spawn_add_membership_contract.md: ≤ 8 tags, each ≤ 32 bytes.
        let sanitized_tags: Vec<String> = tags
            .iter()
            .filter(|t| {
                if t.len() > 32 {
                    log::warn!(
                        "Encounter: boss {} SpawnAdds tag '{}' exceeds 32 bytes — dropping",
                        boss_entity_id.0,
                        t
                    );
                    false
                } else {
                    true
                }
            })
            .take(8)
            .cloned()
            .collect();
        if tags.len() > sanitized_tags.len() && tags.iter().filter(|t| t.len() <= 32).count() > 8 {
            log::warn!(
                "Encounter: boss {} SpawnAdds tags ({}) exceed cap (8) — extras dropped",
                boss_entity_id.0,
                tags.len(),
            );
        }

        for spawn_idx in 0..spawn_count {
            let offset = Self::encounter_spawn_offset(spawn_idx);
            // Local index into the tick's encounter_spawns Vec; the worker
            // shifts by the existing director_spawns length when packaging
            // for commit so `spawn_index` ends up referencing the correct
            // slot in the final `director_spawns` array.
            let local_index = encounter_spawns.len() as u32;
            encounter_spawns.push(game_core::director::DirectorSpawn {
                kind: profile.kind,
                max_hp: profile.max_hp,
                position: Vec3f {
                    x: transform.position.x + offset.x,
                    y: transform.position.y + offset.y,
                    z: transform.position.z + offset.z,
                },
                layer,
            });
            pending_memberships.push(game_core::director::PendingAddMembership {
                spawn_index: local_index,
                boss_entity: boss_entity_id,
                archetype: archetype.to_string(),
                tags: sanitized_tags.clone(),
            });
        }

        log::info!(
            "Encounter: boss {} queued {} scripted spawns (archetype='{}' → {:?}, tags={:?})",
            boss_entity_id.0,
            spawn_count,
            archetype,
            profile.kind,
            sanitized_tags,
        );
    }

    fn apply_spawn_volume(
        &mut self,
        boss_entity_id: EntityId,
        tag: &str,
        shape: game_core::volume::VolumeShape,
        anchor: &game_core::encounter::VolumeAnchor,
        lifetime_ticks: Option<u32>,
        entity_filter: game_core::volume::EntityKindFilter,
    ) {
        let Some(position) = self.resolve_volume_anchor(boss_entity_id, anchor) else {
            log::warn!(
                "Encounter: boss {} spawn_volume tag='{}' skipped (anchor unresolved: {:?})",
                boss_entity_id.0,
                tag,
                anchor,
            );
            return;
        };
        let follow_owner = matches!(anchor, game_core::encounter::VolumeAnchor::FollowBoss);
        let id = self.spawn_volume(
            boss_entity_id,
            tag.to_string(),
            shape,
            position,
            lifetime_ticks,
            entity_filter,
            follow_owner,
        );
        log::info!(
            "Encounter: boss {} spawned volume id={} tag='{}' lifetime={:?}",
            boss_entity_id.0,
            id.0,
            tag,
            lifetime_ticks,
        );
    }

    fn apply_cast_skill(
        &mut self,
        boss_entity_id: EntityId,
        skill_id: u32,
        target: game_core::encounter::Target,
    ) {
        let Some(idx) = self.state.entities.lookup(boss_entity_id) else {
            return;
        };

        if self.is_cc_disabled(idx) || self.is_silenced(idx) {
            log::debug!(
                "Encounter: boss {} cast {} blocked by CC/silence",
                boss_entity_id.0,
                skill_id
            );
            return;
        }

        let resolved_targets = self.resolve_encounter_targets(boss_entity_id, idx, &target);
        if resolved_targets.is_empty() {
            log::debug!(
                "Encounter: boss {} cast {} skipped (target {:?} unresolved)",
                boss_entity_id.0,
                skill_id,
                target,
            );
            return;
        };

        for resolved_target in resolved_targets {
            let casted = self.cast_ability(
                boss_entity_id,
                skill_id,
                ResolvedTargeting::Entity {
                    target: resolved_target,
                },
                0,
                0,
            );
            if !casted {
                log::warn!(
                    "Encounter: boss {} cast {} to target {} rejected (cooldown, missing ability, or invalid cast geometry)",
                    boss_entity_id.0,
                    skill_id,
                    resolved_target.0
                );
            }
        }
    }

    /// Resolve a telegraph target and emit a `TelegraphWarning` event so
    /// clients can surface a wind-up warning ahead of the actual cast.
    fn apply_telegraph(
        &mut self,
        boss_entity_id: EntityId,
        skill_id: u32,
        target: game_core::encounter::Target,
        lead_ticks: u32,
    ) {
        let Some(idx) = self.state.entities.lookup(boss_entity_id) else {
            return;
        };
        let resolved_targets = self.resolve_encounter_targets(boss_entity_id, idx, &target);
        if resolved_targets.is_empty() {
            log::debug!(
                "Encounter: boss {} telegraph skill {} skipped (target {:?} unresolved)",
                boss_entity_id.0,
                skill_id,
                target,
            );
            return;
        };
        let impact_tick = self.current_tick.0.saturating_add(lead_ticks as u64);
        for resolved in resolved_targets {
            log::info!(
                "Encounter: boss {} telegraph skill_id={} → target {} impact_tick={} (lead={})",
                boss_entity_id.0,
                skill_id,
                resolved.0,
                impact_tick,
                lead_ticks,
            );
            self.emit_event(
                resolved,
                EventPayload::TelegraphWarning {
                    source: boss_entity_id,
                    target: resolved,
                    impact_tick,
                },
            );
        }
    }

    /// Resolve an encounter `Target` enum to the concrete entity list for
    /// scripted boss casts. Returns an empty `Vec` when the target cannot be
    /// resolved (e.g., `TopThreat` with empty threat table, no players for
    /// `RandomPlayer`, or no matching volume occupants). Multi-target variants
    /// (`AllPlayers`, `VolumeOccupants`) return every matching entity; the
    /// callers iterate and dispatch one cast / telegraph per entry.
    fn resolve_encounter_targets(
        &self,
        boss_id: EntityId,
        boss_idx: EntityIndex,
        target: &game_core::encounter::Target,
    ) -> Vec<EntityId> {
        match target {
            game_core::encounter::Target::Boss => vec![boss_id],
            game_core::encounter::Target::TopThreat => {
                if let Some(top) = self.state.combat.threat_tables.get(boss_idx).and_then(|table| table.top_threat()) {
                    vec![top]
                } else {
                    Vec::new()
                }
            },
            game_core::encounter::Target::RandomPlayer
            | game_core::encounter::Target::AllPlayers => {
                // Layer scoping: only consider players sharing the boss's
                // layer, so an instanced boss can never accidentally target
                // an open-world player. `RandomPlayer` deterministically
                // picks one entry from the same set keyed by
                // `current_tick ^ boss_id`; `AllPlayers` returns the full
                // layer-scoped list for the caller to broadcast across.
                let boss_layer = self.layer_of_idx(boss_idx);
                let mut players: Vec<EntityId> = self
                    .state
                    .entities
                    .kinds
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, kind)| {
                        if *kind == EntityKind::Player {
                            let player_layer = if slot < self.entity_layer_cache.len() {
                                self.entity_layer_cache[slot]
                            } else {
                                0
                            };
                            if player_layer != boss_layer {
                                return None;
                            }
                            self.state.entities.lookup_by_slot(slot)
                        } else {
                            None
                        }
                    })
                    .collect();
                players.sort_by_key(|id| id.0);
                if matches!(target, game_core::encounter::Target::RandomPlayer) {
                    let seed = self.current_tick.0 ^ boss_id.0;
                    if players.is_empty() {
                        Vec::new()
                    } else {
                        let idx = (seed % players.len() as u64) as usize;
                        vec![players[idx]]
                    }
                } else {
                    players
                }
            }
            game_core::encounter::Target::VolumeOccupants { tag } => {
                // Return every occupant across volumes matching `tag` whose
                // owning boss is `boss_id`. Callers broadcast one cast per
                // entry; layer scoping is implicit (volumes only enroll
                // entities tracked by the boss's encounter on the same layer).
                let store = if let Some(s) = self.volumes.get(&boss_id) { s } else { return Vec::new(); };
                let mut occupants: Vec<EntityId> = Vec::new();
                for v in store.iter_sorted() {
                    if v.tag == *tag {
                        occupants.extend(v.occupants.iter().copied());
                    }
                }
                occupants.sort_by_key(|e| e.0);
                occupants.dedup();
                occupants
            }
        }
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
    pub fn set_equipment_modifiers(
        &mut self,
        entity_id: EntityId,
        modifiers: game_core::stats::EquipmentModifiers,
    ) {
        self.equipment_modifiers.insert(entity_id, modifiers);
    }

    /// Assign a weapon loadout to a player entity.
    ///
    /// Entities with a loadout have their `UseAbility` intents validated against
    /// the active weapon set. Entities without a loadout are unrestricted.
    pub fn set_weapon_loadout(
        &mut self,
        entity_id: EntityId,
        loadout: game_core::combat::loadout::WeaponLoadout,
    ) {
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
            let Some(idx) = self.state.entities.lookup(eid) else {
                continue;
            };
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

// ── Mechanic context bridge ────────────────────────────────────
//
// Adapter that satisfies `game_core::encounter::MechanicCtx` against the
// live `TickPipeline`. Constructed per encounter, per phase pass; never
// stored.
//
// Borrow rules: `WorkerMechanicCtx` holds an exclusive `&mut TickPipeline`,
// so callers must release any prior `&mut self.encounters` borrow before
// constructing one.
pub(super) struct WorkerMechanicCtx<'a> {
    pipeline: &'a mut TickPipeline,
    boss_id: EntityId,
    current_tick: TickId,
    /// Effects queued by mechanics this call. Drained by the caller and
    /// routed through `EncounterState::apply_external_effects` so they
    /// share the same code path as rule-emitted effects.
    pub(super) emitted: Vec<game_core::encounter::Effect>,
}

impl<'a> WorkerMechanicCtx<'a> {
    pub(super) fn new(pipeline: &'a mut TickPipeline, boss_id: EntityId) -> Self {
        let current_tick = pipeline.current_tick;
        Self {
            pipeline,
            boss_id,
            current_tick,
            emitted: Vec::new(),
        }
    }

    pub(super) fn take_emitted(&mut self) -> Vec<game_core::encounter::Effect> {
        std::mem::take(&mut self.emitted)
    }
}

impl<'a> game_core::encounter::MechanicCtx for WorkerMechanicCtx<'a> {
    fn current_tick(&self) -> TickId {
        self.current_tick
    }

    fn boss_entity_id(&self) -> EntityId {
        self.boss_id
    }

    fn emit_effect(&mut self, effect: game_core::encounter::Effect) {
        self.emitted.push(effect);
    }
}
