use super::*;

/// Consecutive ticks an *active* encounter may have a fully vacant arena (no
/// valid same-layer target) before the arena leash resets it to dormant. At
/// 20 Hz this is ~3 s — long enough to absorb a player briefly crossing a cell
/// edge or a one-tick lookup gap, short enough that an abandoned dungeon boss
/// stops executing mechanics almost immediately after the layer empties.
pub(super) const ENCOUNTER_LEASH_VACANCY_TICKS: u32 = 60;

impl TickPipeline {
    // ── Phase 7.5b: Encounter execution ─────────────────────────

    /// Evaluate encounter rules for all active boss encounters.
    ///
    /// For each boss encounter:
    /// 1. Tick active mechanics with `WorkerMechanicCtx`.
    /// 2. Evaluate rules → `Vec<EncounterOutput>`.
    /// 3. Forward `ChangeBossPhase` / `IncrementZoneCounter` to the main
    ///    commit pipeline as inline Tier 1 tick outputs.
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
        let mut pending_memberships: Vec<game_core::director::PendingAddMembership> = Vec::new();

        // Collect boss entity IDs first to avoid borrow issues.
        let mut boss_ids: Vec<EntityId> = self.encounters.keys().copied().collect();
        boss_ids.sort_by_key(|id| id.0);

        for boss_id in boss_ids {
            let Some(boss_idx) = self.state.entities.lookup(boss_id) else {
                // Boss entity no longer exists — clean up encounter.
                //
                // Normal teardown goes through `force_remove_entities`,
                // which calls `cleanup_encounter_for_boss_removal`
                // before tearing down the entity slot (draining active
                // mechanics, dispatching their stop emissions, and
                // removing the encounter row). Tests and any direct
                // `state.remove_entity` callers, however, can free the
                // boss slot without that pre-cleanup, so this branch
                // remains a required safety net rather than dead code.
                self.encounters.remove(&boss_id);
                self.drop_volumes_for_boss(boss_id);
                continue;
            };

            if self.encounters.get(&boss_id).is_some_and(|enc| !enc.active) {
                if self.encounter_arena_activated(boss_id, boss_idx) {
                    if let Some(enc) = self.encounters.get_mut(&boss_id) {
                        enc.activate(self.current_tick);
                    }
                    self.seed_encounter_activation_threat(boss_id, boss_idx);
                    log::info!(
                        "Encounter: boss {} activated at tick {}",
                        boss_id.0,
                        self.current_tick.0
                    );
                } else {
                    continue;
                }
            }

            // ── Arena leash ─────────────────────────────────────
            // An active encounter whose arena has physically emptied — no
            // same-layer player present and no same-layer threat source left
            // (every player left the layer) — accrues vacancy ticks. Once it
            // crosses the grace window it is reset to dormant so it stops
            // ticking mechanics/telegraphs into an empty layer (the root cause
            // of abandoned dungeon bosses lingering "active"). Re-entry
            // re-activates it through the block above.
            //
            // Retention uses `encounter_arena_occupied` (physical presence),
            // NOT `encounter_arena_activated` (perceivable target): a player
            // who stealths/goes invisible inside the arena is still there and
            // must keep the boss engaged. The threat branch also keeps
            // long-range kiters on the same layer counted as present, so the
            // leash only trips on a genuinely empty arena.
            if self.encounter_arena_occupied(boss_id, boss_idx) {
                self.encounter_vacancy_ticks.remove(&boss_id);
            } else {
                let vacancy = self
                    .encounter_vacancy_ticks
                    .entry(boss_id)
                    .or_insert(0);
                *vacancy += 1;
                if *vacancy >= ENCOUNTER_LEASH_VACANCY_TICKS {
                    self.leash_reset_encounter(boss_id, boss_idx, &mut commit_outputs);
                    continue;
                }
            }

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
                let mut finished: Vec<(String, game_core::encounter::MechanicOutcome)> = Vec::new();
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
            let occupants_by_tag = self.volumes_occupants_by_tag_map(boss_id);
            let mut buff_entities = BTreeSet::new();
            for occupants in occupants_by_tag.values() {
                for entity in occupants {
                    buff_entities.insert(*entity);
                }
            }
            let mut entity_buffs_by_entity = HashMap::new();
            for entity in buff_entities {
                if let Some(idx) = self.state.entities.lookup(entity) {
                    let buffs = self
                        .state
                        .status
                        .get_buffs(idx)
                        .iter()
                        .map(|buff| buff.buff_id)
                        .collect();
                    entity_buffs_by_entity.insert(entity, buffs);
                }
            }
            let volume_events = self.volumes_take_rule_events(boss_id);
            let Some(enc) = self.encounters.get_mut(&boss_id) else {
                continue;
            };
            let eval_inputs = game_core::encounter::EncounterEvalInputs {
                volume_occupancy_by_tag: occupancy_by_tag
                    .iter()
                    .map(|(k, v)| (k.as_str(), *v))
                    .collect(),
                volume_occupants_by_tag: occupants_by_tag
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.clone()))
                    .collect(),
                entity_buffs_by_entity,
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

    /// Single dispatcher for `EncounterOutput`s, regardless of source
    /// (rules or mechanic emissions). Each variant routes to a focused
    /// per-variant handler. Mechanic-emitted effects from `start` /
    /// `on_event` callbacks are gathered and recursively dispatched.
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

        const MECHANIC_DISPATCH_MAX_DEPTH: u8 = 4;

        if depth > MECHANIC_DISPATCH_MAX_DEPTH {
            log::warn!(
                "Encounter: boss {} mechanic emission depth exceeded {} — dropping {} outputs",
                boss_id.0,
                MECHANIC_DISPATCH_MAX_DEPTH,
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
                    let nested = enc.apply_external_effects(start_emissions, self.current_tick);
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

        // 3) Buff effects (ApplyBuff / RemoveBuffs).
        //
        // Must run before StopMechanic so that effects targeting
        // `VolumeOccupants` (which look up occupants in the worker's volume
        // store) observe the mechanic's volumes before StopMechanic
        // despawns them. Same-tick "pulse-then-cleanup" rules (e.g.
        // Manaya's Core ring debuffs) rely on this ordering.
        for output in &outputs {
            match output {
                EO::ApplyBuff {
                    boss_entity_id,
                    target,
                    buff_id,
                    mode,
                } => {
                    self.apply_encounter_buff(*boss_entity_id, target, *buff_id, mode.clone());
                }
                EO::RemoveBuffs {
                    boss_entity_id,
                    target,
                    buff_ids,
                    force,
                } => {
                    self.apply_encounter_remove_buffs(*boss_entity_id, target, buff_ids, *force);
                }
                _ => {}
            }
        }

        // 4) StopMechanic.
        for output in &outputs {
            if let EO::StopMechanic { name, .. } = output {
                let stop_emissions = self.apply_stop_mechanic(boss_id, name);
                if !stop_emissions.is_empty() {
                    if let Some(enc) = self.encounters.get_mut(&boss_id) {
                        let nested = enc.apply_external_effects(stop_emissions, self.current_tick);
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

        // 5) ReplaceAbilityList.
        for output in &outputs {
            if let EO::ReplaceAbilityList {
                boss_entity_id,
                ability_ids,
            } = output
            {
                self.apply_replace_ability_list(*boss_entity_id, ability_ids);
            }
        }

        // 6) SpawnAdds.
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

        // 7) Telegraph.
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

        // 8) Client-facing encounter cues.
        for output in &outputs {
            if let EO::EncounterCue {
                boss_entity_id,
                target,
                cue_id,
                anchor,
                shape,
                lead_ticks,
                duration_ticks,
            } = output
            {
                self.apply_encounter_cue(
                    *boss_entity_id,
                    target,
                    cue_id,
                    anchor.clone(),
                    *shape,
                    *lead_ticks,
                    *duration_ticks,
                );
            }
        }

        // 9) Interactable effects.
        for output in &outputs {
            match output {
                EO::SetInteractableState {
                    boss_entity_id,
                    selector,
                    state,
                } => {
                    self.apply_set_interactable_state(*boss_entity_id, selector, *state);
                }
                EO::ToggleInteractable {
                    boss_entity_id,
                    selector,
                } => {
                    self.apply_toggle_interactable(*boss_entity_id, selector);
                }
                _ => {}
            }
        }

        // 10) SpawnVolume / DespawnVolume.
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

        // 11) CastSkill.
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
        // StopMechanic must surface a MechanicEnded event so that
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
                    enc.bus
                        .push(game_core::encounter::EncounterEvent::MechanicEnded {
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

    /// Finalize active mechanics before an owning boss is hard-removed.
    ///
    /// This mirrors scripted `StopMechanic` cleanup, but it sends each active
    /// mechanic its own stop event because boss removal tears down the whole
    /// encounter rather than one named runtime. Effects emitted here are still
    /// routed through the normal encounter output dispatcher so mechanic-owned
    /// volumes and mechanic-locked buffs get cleaned up in the same place as
    /// rule-driven stops.
    pub(super) fn cleanup_encounter_for_boss_removal(&mut self, boss_id: EntityId) {
        self.run_mechanic_stop_cascade(boss_id);
        self.encounters.remove(&boss_id);
        self.drop_volumes_for_boss(boss_id);
    }

    /// Arena leash reset: return an active encounter whose arena went vacant to
    /// its dormant baseline so it stops executing mechanics offscreen and
    /// re-activates cleanly when a player returns.
    ///
    /// Mirrors the canonical MMO leash: the boss is restored to full HP with a
    /// cleared threat table at its home position, scripted adds are despawned,
    /// active mechanics are stopped through the normal cascade (releasing their
    /// volumes and locked buffs), and the encounter state is reset to dormant
    /// (`EncounterState::deactivate`). Resetting HP closes the chip-damage
    /// exploit where a player wounds a boss, leaves to drop combat, and returns
    /// to a still-damaged boss.
    fn leash_reset_encounter(
        &mut self,
        boss_id: EntityId,
        boss_idx: EntityIndex,
        commit_outputs: &mut Vec<game_core::encounter::EncounterOutput>,
    ) {
        // 1) Stop active mechanics (cleans up mechanic-owned volumes/buffs)
        //    while keeping the encounter row and boss entity alive.
        self.run_mechanic_stop_cascade(boss_id);
        self.drop_volumes_for_boss(boss_id);

        // 2) Despawn scripted adds owned by this boss through the normal
        //    lifecycle, NOT a direct `force_remove_entities` call. Marking them
        //    `DespawnPending` lets Phase 8b (`phase_state_finalization`) emit
        //    the `EntityState::Removed` rows the commit pipeline needs — a hard
        //    remove here would drop the adds from SimState/physics while the DB
        //    kept them active. Marking (rather than killing) also skips the
        //    death/loot/kill-counter path, since this is a leash despawn.
        let adds: Vec<EntityId> = self
            .add_to_boss
            .iter()
            .filter_map(|(add, owner)| (*owner == boss_id).then_some(*add))
            .collect();
        for add in adds {
            self.state.mark_despawn(add);
        }

        // 3) Restore the boss: full HP, cleared threat, Idle AI, home position.
        //    Health is owned by Phase 8b, so the heal is *queued* into
        //    `pending_heals` (the same path the evade-home reset uses) rather
        //    than written here in Phase 7.5b. `phase_state_finalization`
        //    applies it under the StatusEffects/phase-8 owner, emits the
        //    `Healed` event, and surfaces it in this tick's health_updates so
        //    the DB/clients see the restore — a direct dense write would heal
        //    only in worker memory and skip the commit snapshot (and trip the
        //    mutation-ownership audit). `last_damage_source` is dropped so a
        //    stale attacker can't mis-attribute a later kill.
        let slot = boss_idx.as_usize();
        let max_hp = self.state.combat.health.max_hp[slot];
        let current_hp = self.state.combat.health.hp[slot];
        if current_hp < max_hp {
            self.pending_heals
                .push((boss_id, max_hp - current_hp, boss_id));
        }
        self.state.combat.health.last_damage_source[slot] = None;
        if let Some(table) = self.state.combat.threat_tables.get_mut(boss_idx) {
            table.entries.clear();
        }
        if let Some(ai) = self.state.ai.npc_ai.get_mut(boss_idx) {
            *ai = game_schema::NpcAiState::Idle;
        }
        if let Some(home) = self.state.ai.home_positions.get(boss_idx).copied() {
            self.physics.teleport_entity(boss_id, home);
        }

        // 4) Return the encounter to its dormant baseline and persist the phase
        //    reset. `deactivate` resets phase to Phase1 in worker memory only;
        //    the `boss_phase` table is updated solely from
        //    `EncounterOutput::ChangeBossPhase`. Without this a boss leashing
        //    from Phase2/3/Enrage leaves the DB and clients on the stale phase.
        //    Only emit when the phase actually changed.
        let was_past_phase1 = self
            .encounters
            .get(&boss_id)
            .is_some_and(|enc| enc.phase != game_core::encounter::BossPhase::Phase1);
        if let Some(enc) = self.encounters.get_mut(&boss_id) {
            enc.deactivate();
        }
        if was_past_phase1 {
            commit_outputs.push(game_core::encounter::EncounterOutput::ChangeBossPhase {
                boss_entity_id: boss_id,
                new_phase: game_core::encounter::BossPhase::Phase1.to_phase_number(),
                entered_at_tick: self.current_tick.0,
            });
        }
        self.encounter_vacancy_ticks.remove(&boss_id);

        log::info!(
            "Encounter: boss {} leashed at tick {} (arena vacant) — reset to dormant",
            boss_id.0,
            self.current_tick.0
        );
    }

    /// Stop every active mechanic on `boss_id` and route their final stop
    /// emissions through the normal output dispatcher, leaving the encounter
    /// row and the boss entity intact.
    ///
    /// Shared by [`cleanup_encounter_for_boss_removal`] (which then removes the
    /// encounter and drops its volumes) and the arena leash reset (which keeps
    /// the encounter so it can re-activate later). A no-op when the boss has no
    /// registered encounter.
    fn run_mechanic_stop_cascade(&mut self, boss_id: EntityId) {
        let mut active_mechanics = match self.encounters.get_mut(&boss_id) {
            Some(enc) => std::mem::take(&mut enc.active_mechanics),
            None => return,
        };

        let emitted = {
            let mut ctx = WorkerMechanicCtx::new(self, boss_id);
            for mechanic in &mut active_mechanics {
                let event_name = format!("mechanic:{}:stop", mechanic.name());
                mechanic.on_event(&mut ctx, &event_name);
            }
            ctx.take_emitted()
        };

        let outputs = match self.encounters.get_mut(&boss_id) {
            Some(enc) => {
                enc.active_mechanics.clear();
                enc.apply_external_effects(emitted, self.current_tick)
            }
            None => Vec::new(),
        };

        if !outputs.is_empty() {
            let mut commit_outputs = Vec::new();
            let mut encounter_spawns = Vec::new();
            let mut pending_memberships = Vec::new();
            self.dispatch_encounter_outputs(
                boss_id,
                outputs,
                &mut commit_outputs,
                &mut encounter_spawns,
                &mut pending_memberships,
                0,
            );
            // Carry the cleanup-cascade outputs into the current tick's
            // commit pipeline so that Tier-2 effects (e.g. zone counter
            // increments) and any terminal add spawns emitted by mechanic
            // stop callbacks are not silently dropped. The owning tick
            // loop drains these into `boss_phase_updates` /
            // `zone_counter_deltas` / `director_spawns` /
            // `encounter_memberships` after Phase 8b finalization.
            if !commit_outputs.is_empty() {
                self.pending_cleanup_commit_outputs.extend(commit_outputs);
            }
            if !encounter_spawns.is_empty() || !pending_memberships.is_empty() {
                let spawn_offset = self.pending_cleanup_spawns.len() as u32;
                self.pending_cleanup_spawns.extend(encounter_spawns);
                if spawn_offset > 0 {
                    for m in &mut pending_memberships {
                        m.spawn_index = m.spawn_index.saturating_add(spawn_offset);
                    }
                }
                self.pending_cleanup_memberships.extend(pending_memberships);
            }
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
struct WorkerMechanicCtx<'a> {
    pipeline: &'a mut TickPipeline,
    boss_id: EntityId,
    current_tick: TickId,
    /// Effects queued by mechanics this call. Drained by the caller and
    /// routed through `EncounterState::apply_external_effects` so they
    /// share the same code path as rule-emitted effects.
    emitted: Vec<game_core::encounter::Effect>,
}

impl<'a> WorkerMechanicCtx<'a> {
    fn new(pipeline: &'a mut TickPipeline, boss_id: EntityId) -> Self {
        let current_tick = pipeline.current_tick;
        Self {
            pipeline,
            boss_id,
            current_tick,
            emitted: Vec::new(),
        }
    }

    fn take_emitted(&mut self) -> Vec<game_core::encounter::Effect> {
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

    fn volume_occupants(&self, tag: &str) -> Vec<EntityId> {
        self.pipeline.volumes_occupants_by_tag(self.boss_id, tag)
    }

    fn has_buff(&self, entity: EntityId, buff_id: u32) -> bool {
        let Some(idx) = self.pipeline.state.entities.lookup(entity) else {
            return false;
        };
        self.pipeline
            .state
            .status
            .get_buffs(idx)
            .iter()
            .any(|buff| buff.buff_id == buff_id)
    }
}
