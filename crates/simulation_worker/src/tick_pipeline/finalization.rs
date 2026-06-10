use super::*;

impl TickPipeline {
    fn death_position(&mut self, id: EntityId, context: &str) -> Vec3f {
        self.physics
            .get_transform(id)
            .map(|t| t.position)
            .unwrap_or_else(|| {
                let fallback = self.last_committed_transforms.get(&id).map(|t| t.position);
                match fallback {
                    Some(p) => {
                        sim_warn!(
                            self,
                            "{context}: physics transform missing for entity {} at death; using last-committed position ({:.2}, {:.2}, {:.2})",
                            id.0,
                            p.x,
                            p.y,
                            p.z,
                        );
                        p
                    }
                    None => {
                        sim_warn!(
                            self,
                            "{context}: no physics transform and no last-committed transform for entity {} at death; anchoring at origin",
                            id.0,
                        );
                        Vec3f {
                            x: 0.0,
                            y: 0.0,
                            z: 0.0,
                        }
                    }
                }
            })
    }

    /// Push zone_counter deltas for a dying entity based on its `EntityKind`.
    /// Looks up the entity's last-known region via `entity_regions`; if the
    /// entity was never region-assigned (synthetic/test entities), emits no
    /// delta rather than defaulting to region (0, 0, 0). Counter names match
    /// the hardcoded `world_clock` rules in `server_module::reducers`.
    fn push_death_zone_counter(&mut self, id: EntityId, idx: EntityIndex) {
        use game_core::entity::lifecycle::EntityKind;
        let Some(cell) = self.entity_regions.get(&id).copied() else {
            return;
        };
        let kind = self.state.entities.kinds[idx.as_usize()];
        // Instance layers are uniquely identified by `layer`; normalize the
        // region cell to (0, 0) so all kills in a dungeon accumulate in one
        // counter row rather than scattering across multiple 50 m cells.
        // Open-world (layer 0) keeps the real region cell for AOI zone events.
        let (rx, rz) = if cell.layer > 0 {
            (0, 0)
        } else {
            (cell.region_x, cell.region_z)
        };
        match kind {
            EntityKind::Npc => {
                self.pending_zone_counter_deltas.push((
                    cell.layer,
                    rx,
                    rz,
                    "kills".to_string(),
                    1.0,
                ));
            }
            EntityKind::Boss => {
                // Boss deaths bump both "kills" (so trash-count semantics hold
                // "a kill is a kill") and "boss_killed" (triggers the higher-
                // priority "completed" world_phase rule).
                self.pending_zone_counter_deltas.push((
                    cell.layer,
                    rx,
                    rz,
                    "kills".to_string(),
                    1.0,
                ));
                self.pending_zone_counter_deltas.push((
                    cell.layer,
                    rx,
                    rz,
                    "boss_killed".to_string(),
                    1.0,
                ));
            }
            // Players and non-combat kinds do not bump the "kills" counter —
            // PvE progression must not be driven by player deaths, and
            // scripted/environmental despawns must not pollute the counter.
            _ => {}
        }
    }

    /// Emit an authoritative `DeathState` row for player deaths.
    ///
    /// Called at the point of death detection so killer attribution
    /// (`last_damage_source`/top-threat) and current position/layer are
    /// captured directly from worker state, not re-derived in the reducer.
    /// Only `Player` kinds produce a death-state row; NPCs and props skip.
    fn push_player_death_state(
        &mut self,
        id: EntityId,
        idx: EntityIndex,
        killer: Option<EntityId>,
    ) {
        use game_core::entity::lifecycle::EntityKind;
        if self.state.entities.kinds[idx.as_usize()] != EntityKind::Player {
            return;
        }
        let pos = self.death_position(id, "death_state");
        let layer = self.layer_of_idx(idx);
        self.pending_death_state_inserts
            .push(super::DeathStateInsertEntry {
                entity_id: id,
                killer_entity: killer,
                layer,
                death_pos_x: pos.x,
                death_pos_y: pos.y,
                death_pos_z: pos.z,
            });
    }

    fn push_death_loot_roll(&mut self, id: EntityId, idx: EntityIndex, killer: Option<EntityId>) {
        let Some(table_id) = self.entity_loot_tables.get(&id).cloned() else {
            return;
        };
        let Some(table) = self.loot_registry.get(&table_id).cloned() else {
            sim_warn!(
                self,
                "loot: entity {} references unknown loot table '{}'; dropping roll",
                id.0,
                table_id,
            );
            return;
        };

        let layer = self.layer_of_idx(idx);
        let contributors = self
            .state
            .combat
            .threat_tables
            .get(idx)
            .into_iter()
            .flat_map(|table| table.entries.iter())
            .filter_map(|entry| {
                let source_idx = self.state.entities.lookup(entry.source)?;
                if self.state.entities.kinds[source_idx.as_usize()]
                    != game_core::entity::lifecycle::EntityKind::Player
                {
                    return None;
                }
                if self.layer_of_idx(source_idx) != layer {
                    return None;
                }
                Some((entry.source, entry.threat))
            });
        let killer = killer.filter(|killer| {
            self.state
                .entities
                .lookup(*killer)
                .is_some_and(|killer_idx| {
                    self.state.entities.kinds[killer_idx.as_usize()]
                        == game_core::entity::lifecycle::EntityKind::Player
                        && self.layer_of_idx(killer_idx) == layer
                })
        });
        let eligible_claimants = game_core::loot::freeze_eligible_claimants(contributors, killer);
        if eligible_claimants.is_empty() {
            sim_warn!(
                self,
                "loot: entity {} table '{}' produced no eligible claimants; dropping roll",
                id.0,
                table_id,
            );
            return;
        }

        let rolled_items =
            game_core::loot::roll_loot_table(&table_id, &table, self.current_tick, id);
        if rolled_items.is_empty() {
            sim_warn!(
                self,
                "loot: entity {} table '{}' produced no items; dropping roll",
                id.0,
                table_id,
            );
            return;
        }

        let position = self.death_position(id, "loot");
        self.pending_loot_rolls
            .push(game_core::loot::LootRollOutput {
                corpse_entity: id,
                layer,
                position,
                rolled_items,
                eligible_claimants,
                claim_window_ticks: table.claim_window_ticks,
            });
    }

    // ── Phase 8a: Cooldown expiry ───────────────────────────────

    /// Phase 8a: Drain the cooldown map of entries that have become ready this tick.
    ///
    /// An entry is ready when `ready_at ≤ current_tick`. At that point the ability is
    /// castable again; we emit a `CooldownReady` event and remove the entry so the map
    /// only holds in-flight cooldowns. CooldownReady uniqueness is guaranteed by the
    /// HashMap key — duplicate entries for the same (entity, ability) are impossible.
    ///
    /// Also drains expired follow-up windows from `CombatState::active_windows` and
    /// clears the hold-to-block flag (`blocking`) so the `Block` intent must re-assert
    /// it each tick. `dodge_stacks` is NOT cleared here — it persists across ticks and
    /// is managed exclusively by `StanceBegin`/`StanceEnd` in Phase 3.
    pub(super) fn phase_expire_cooldowns(&mut self) {
        let current = self.current_tick;
        // Collect first so the borrow on `self.cooldowns` ends before `emit_event` borrows `self`.
        let expired: Vec<(EntityId, u32)> = self
            .cooldowns
            .iter()
            .filter(|&(_, &ready_at)| ready_at <= current)
            .map(|(&k, _)| k)
            .collect();
        for (entity, ability_id) in expired {
            self.cooldowns.remove(&(entity, ability_id));
            audit!(
                self.state,
                Cooldown,
                CooldownTracker,
                8,
                Some(entity),
                "expire"
            );
            self.emit_event(entity, EventPayload::CooldownReady { ability_id });
        }

        // Drain expired follow-up windows (expiry tick ≤ current tick).
        self.state
            .combat
            .active_windows
            .retain(|_, &mut (_, exp)| exp > current);
        // Audit a single Window drain record per tick if any windows were active.
        // (Individual per-window records omitted to avoid log spam at scale.)
        #[cfg(any(debug_assertions, test))]
        {
            // We can't audit here without an entity context; the drain is a bulk operation.
            // The ownership rule (CooldownTracker, phase 8) is enforced structurally — this
            // method is the sole caller for window drains.
            let _ = self; // suppress unused-warning if audit! expands to nothing
        }

        // Clear hold-to-block flag. Phase 2 must re-assert it each tick via Block intent.
        // dodge_stacks is NOT cleared here — it is lifecycle-managed by StanceBegin/StanceEnd
        // in Phase 3 and must persist across multiple ticks for the full iframe duration.
        //
        // Uses a 1-tick grace period to absorb timing gaps between the client's
        // intent throttle and the server tick rate. On the first missed tick,
        // block_grace is set and blocking/root are preserved. On the second
        // consecutive miss, the block sequence truly ends.
        let mut block_ended: Vec<EntityId> = Vec::new();
        for (i, slot) in self.state.combat.tactical.iter_mut().enumerate() {
            if slot.blocking {
                // Block was active this tick — clear the flag so it must be re-asserted,
                // but preserve block_start_tick for perfect-block window continuity.
                // blocking = false is sufficient to remove the block-driven root;
                // is_rooted() derives the composite value from all sources.
                slot.blocking = false;
                slot.block_grace = false;
            } else if slot.block_start_tick.is_some() {
                if !slot.block_grace {
                    // First missed tick — grant grace period.
                    // blocking stays false: Phase 6 and is_rooted() check
                    // block_grace directly so mitigation and root persist
                    // without confusing Phase 8a's state machine on the
                    // next tick. block_start_tick is preserved for
                    // perfect-block window continuity.
                    slot.block_grace = true;
                } else {
                    // Second consecutive miss — block sequence truly ended.
                    slot.block_start_tick = None;
                    slot.block_grace = false;
                    if let Some(eid) = self.state.entities.lookup_by_slot(i) {
                        block_ended.push(eid);
                    }
                }
            }
        }
        for eid in block_ended {
            self.emit_event(eid, EventPayload::BlockEnd);
        }
    }

    /// Emit `AbilityCancelled { reason: Death }` for every active cast
    /// and every active charge owned by `entity`.
    ///
    /// Called in Phase 8b for an entity transitioning to
    /// `DespawnPending`. Must run BEFORE the `EntityDied` event so
    /// clients still hold live cast/charge state when the cancel
    /// arrives. See
    /// `docs/contracts/ability_cast_lifecycle_contract.md` and
    /// `docs/contracts/event_ordering_contract.md`.
    fn emit_death_cancels(&mut self, entity: EntityId) {
        self.cancel_entity_abilities(entity, AbilityCancelReason::Death);
    }

    // ── Phase 8b: State finalization ────────────────────────────

    /// Returns (state_updates, dot_health_updates). The latter captures health for
    /// entities damaged by DoT this tick, since the main `collect_health_updates` runs
    /// before Phase 8b and would miss these changes.
    pub(super) fn phase_state_finalization(
        &mut self,
    ) -> (
        Vec<(EntityId, game_schema::EntityState)>,
        Vec<(EntityId, f32, f32)>,
    ) {
        use game_core::entity::lifecycle::EntityState;

        let mut state_updates: Vec<(EntityId, game_schema::EntityState)> = Vec::new();

        // ── Drain deferred heals queued in Phase 7 (evade-home) ─────────
        let heals = std::mem::take(&mut self.pending_heals);
        let mut deferred_heal_entities: HashSet<EntityId> = HashSet::new();
        for (entity_id, amount, source) in heals {
            if let Some(idx) = self.state.entities.lookup(entity_id) {
                let healed = self.state.combat.health.apply_healing(idx, amount);
                if healed > 0.0 {
                    audit!(
                        self.state,
                        Health,
                        StatusEffects,
                        8,
                        Some(entity_id),
                        "evade_heal"
                    );
                    self.emit_event(
                        entity_id,
                        EventPayload::Healed {
                            amount: healed,
                            source,
                        },
                    );
                    deferred_heal_entities.insert(entity_id);
                }
            }
        }

        // Check for newly dead entities and mark them for despawn.
        let dead_indices: Vec<EntityIndex> = (0..self.state.entities.len())
            .map(|i| self.state.entities.index_at(i))
            .filter(|&idx| {
                // Props are structural entities — HP reaching zero must not
                // trigger the death/despawn lifecycle (gates, switches, barrels).
                self.state.entities.is_active(idx)
                    && self.state.combat.health.is_dead(idx)
                    && self.state.entities.kinds[idx.as_usize()]
                        != game_core::entity::lifecycle::EntityKind::Prop
            })
            .collect();

        for idx in dead_indices {
            let id = self.state.entities.id_of(idx);
            self.state.entities.mark_despawn(idx);
            state_updates.push((id, EntityState::DespawnPending));
            audit!(
                self.state,
                Lifecycle,
                Lifecycle,
                8,
                Some(id),
                "death_despawn"
            );
            self.summary.deaths += 1;
            // Determine killer: prefer last_damage_source, fall back to top-threat.
            let killer =
                self.state.combat.health.last_damage_source[idx.as_usize()].or_else(|| {
                    self.state
                        .combat
                        .threat_tables
                        .get(idx)
                        .and_then(|t| t.top_threat())
                });
            // Cancel-before-died: emit AbilityCancelled events for any
            // in-flight casts and charges before EntityDied so clients
            // can clean up cast/charge UI in the same event_sequence
            // window. Codified in
            // `docs/contracts/event_ordering_contract.md`.
            self.emit_death_cancels(id);
            self.emit_event(id, EventPayload::EntityDied { killer });
            self.forward_death_to_encounter(id);
            self.push_death_zone_counter(id, idx);
            self.push_player_death_state(id, idx, killer);
            self.push_death_loot_roll(id, idx, killer);
        }

        // Clean up DespawnPending entities.
        // DespawnPending entities are already inert (filtered in views, skipped by
        // simulation), so capping removals per tick is safe — excess entities stay
        // DespawnPending and are cleaned up in subsequent ticks.
        const MAX_REMOVALS_PER_TICK: usize = 500;
        let all_despawning = self.state.despawn_pending();
        let despawning: Vec<EntityId> = if all_despawning.len() > MAX_REMOVALS_PER_TICK {
            log::info!(
                "Staggering removal: {} pending, processing {} this tick",
                all_despawning.len(),
                MAX_REMOVALS_PER_TICK,
            );
            all_despawning[..MAX_REMOVALS_PER_TICK].to_vec()
        } else {
            all_despawning
        };
        // Emit per-entity events before batch teardown (events reference entity state
        // that force_remove_entities will destroy).
        for &id in &despawning {
            self.summary.despawns += 1;
            self.emit_event(id, EventPayload::EntityDespawned);
            audit!(self.state, Lifecycle, Lifecycle, 8, Some(id), "remove");
            state_updates.push((id, EntityState::Removed));
        }
        // Batch teardown — O(world) instead of O(N × world).
        self.force_remove_entities(&despawning);
        let bulk_despawn_count = despawning.len();

        // Expire buffs. Clear CC flags for any expired debuff that tracked a CC effect,
        // preventing stale SLEEPING/SILENCED/FEARED flags from persisting after natural expiry.
        let expired_buffs = self.state.expire_buffs(self.current_tick);
        for (entity_id, buff) in &expired_buffs {
            audit!(
                self.state,
                Buff,
                StatusEffects,
                8,
                Some(*entity_id),
                "expire"
            );
            self.emit_event(
                *entity_id,
                EventPayload::BuffExpired {
                    buff_id: buff.buff_id,
                },
            );
        }
        for (entity_id, buff) in &expired_buffs {
            if let Some(cc) = buff.modifiers.cc_effect {
                if let Some(idx) = self.state.entities.lookup(*entity_id) {
                    self.clear_cc_by_effect(*entity_id, idx, cc);
                }
            }
        }

        // ── DoT tick processing ─────────────────────────────────────
        // Iterate all active entities, find buffs with dot_damage whose interval has
        // elapsed, collect the damage actions, then apply them in a second pass.
        let mut dot_damaged_entities: HashSet<EntityId> = HashSet::new();
        {
            use game_core::entity::lifecycle::EntityState as ES;
            let current = self.current_tick;
            // Collect: (entity_id, entity_index, source, damage, damage_type, buff_index)
            let mut dot_hits: Vec<(
                EntityId,
                EntityIndex,
                EntityId,
                f32,
                game_protocol::event::DamageType,
            )> = Vec::new();
            // Track which (entity_index, buff_index) need last_dot_tick updated.
            let mut tick_updates: Vec<(EntityIndex, usize)> = Vec::new();

            for i in 0..self.state.entities.len() {
                if self.state.entities.states[i] == ES::Removed
                    || self.state.entities.states[i] == ES::DespawnPending
                {
                    continue;
                }
                let idx = self.state.entities.index_at(i);
                let entity_id = self.state.entities.id_of(idx);
                let buffs = self.state.status.get_buffs(idx);
                for (bi, buff) in buffs.iter().enumerate() {
                    let Some(dot_dmg) = buff.modifiers.dot_damage else {
                        continue;
                    };
                    let interval = buff.modifiers.dot_interval_ticks.unwrap_or(20) as u64;
                    let last = buff.last_dot_tick.unwrap_or(TickId(0));
                    if current.0.saturating_sub(last.0) >= interval {
                        let dmg_type = buff
                            .modifiers
                            .dot_damage_type
                            .unwrap_or(game_protocol::event::DamageType::Physical);
                        // Multiply damage by stack count.
                        let total_dmg = dot_dmg * buff.stacks as f32;
                        dot_hits.push((entity_id, idx, buff.source, total_dmg, dmg_type));
                        tick_updates.push((idx, bi));
                    }
                }
            }

            // Apply damage and emit events.
            for (entity_id, idx, source, damage, dmg_type) in &dot_hits {
                let actual = self
                    .state
                    .combat
                    .health
                    .apply_damage(*idx, *damage, Some(*source));
                audit!(
                    self.state,
                    Health,
                    StatusEffects,
                    8,
                    Some(*entity_id),
                    "dot_damage"
                );
                dot_damaged_entities.insert(*entity_id);
                // Add threat if source is alive.
                if self.state.entities.lookup(*source).is_some() {
                    if let Some(table) = self.state.combat.threat_tables.get_mut(*idx) {
                        table.add_threat(*source, actual);
                    }
                }
                self.emit_event(
                    *entity_id,
                    EventPayload::Damage {
                        source: *source,
                        amount: actual,
                        damage_type: *dmg_type,
                    },
                );
            }

            // Update last_dot_tick on buffs that fired.
            for (idx, bi) in tick_updates {
                self.state.status.modify_buffs(idx, |buffs| {
                    if let Some(buff) = buffs.get_mut(bi) {
                        buff.last_dot_tick = Some(current);
                    }
                });
            }
        }

        // Snapshot health for entities changed in Phase 8b (DoT damage, deferred heals)
        // before the second death check removes killed entities. The main
        // collect_health_updates ran before Phase 8b and missed these changes.
        let phase8b_entities = dot_damaged_entities.union(&deferred_heal_entities);
        let dot_health_updates: Vec<(EntityId, f32, f32)> = phase8b_entities
            .filter_map(|&eid| {
                let idx = self.state.entities.lookup(eid)?;
                let i = idx.as_usize();
                Some((
                    eid,
                    self.state.combat.health.hp[i],
                    self.state.combat.health.max_hp[i],
                ))
            })
            .collect();

        // Second death check: catch entities killed by DoT damage above.
        // Without this, DoT-killed entities survive one extra tick at 0 HP.
        let dot_dead: Vec<EntityIndex> = (0..self.state.entities.len())
            .map(|i| self.state.entities.index_at(i))
            .filter(|&idx| {
                self.state.entities.is_active(idx)
                    && self.state.combat.health.is_dead(idx)
                    && self.state.entities.kinds[idx.as_usize()]
                        != game_core::entity::lifecycle::EntityKind::Prop
            })
            .collect();
        let mut dot_newly_despawned = Vec::new();
        for idx in dot_dead {
            let id = self.state.entities.id_of(idx);
            self.state.entities.mark_despawn(idx);
            state_updates.push((id, EntityState::DespawnPending));
            audit!(
                self.state,
                Lifecycle,
                Lifecycle,
                8,
                Some(id),
                "dot_death_despawn"
            );
            self.summary.deaths += 1;
            let killer =
                self.state.combat.health.last_damage_source[idx.as_usize()].or_else(|| {
                    self.state
                        .combat
                        .threat_tables
                        .get(idx)
                        .and_then(|t| t.top_threat())
                });
            self.emit_death_cancels(id);
            self.emit_event(id, EventPayload::EntityDied { killer });
            self.forward_death_to_encounter(id);
            self.push_death_zone_counter(id, idx);
            self.push_player_death_state(id, idx, killer);
            dot_newly_despawned.push(id);
        }
        // Remove only the entities that DoT actually killed this tick,
        // not leftovers from the capped first sweep.
        for &id in &dot_newly_despawned {
            self.summary.despawns += 1;
            self.emit_event(id, EventPayload::EntityDespawned);
            audit!(self.state, Lifecycle, Lifecycle, 8, Some(id), "dot_remove");
            state_updates.push((id, EntityState::Removed));
        }
        let dot_count = dot_newly_despawned.len();
        self.force_remove_entities(&dot_newly_despawned);

        // Decay threat tables multiplicatively — all values scale by factor each tick.
        // Multiplicative decay prevents runaway target switching when entries are near-equal,
        // and allows large accumulated threat to bleed down gracefully.
        const THREAT_DECAY_FACTOR: f32 = 0.98;
        for table in self.state.combat.threat_tables.values_mut() {
            table.decay(THREAT_DECAY_FACTOR);
        }
        audit!(self.state, Threat, Lifecycle, 8, None::<EntityId>, "decay");

        // Expire stale lock-on sessions (player started selection but never fired).
        // No cooldown applied — session just disappears silently.
        let current_tick = self.current_tick;
        let mut expired_sessions: Vec<(EntityId, Vec<EntityId>)> = Vec::new();
        self.active_lock_on_sessions.retain(|&caster, session| {
            if current_tick >= session.timeout_at {
                expired_sessions.push((caster, session.tagged.clone()));
                false
            } else {
                true
            }
        });
        for (caster, tagged) in expired_sessions {
            for target in &tagged {
                self.emit_event(
                    caster,
                    EventPayload::LockOnCanceled {
                        source: caster,
                        target: *target,
                    },
                );
            }
        }

        // Activate any Spawning entities (they've had one tick to set up physics).
        let spawning: Vec<EntityIndex> = (0..self.state.entities.len())
            .filter(|&i| self.state.entities.states[i] == EntityState::Spawning)
            .map(|i| self.state.entities.index_at(i))
            .collect();
        for idx in spawning {
            let id = self.state.entities.id_of(idx);
            self.state.entities.activate(idx);
            audit!(self.state, Lifecycle, Lifecycle, 8, Some(id), "activate");
            state_updates.push((id, EntityState::Active));
        }

        // Shrink over-allocated HashMaps after bulk removal to reclaim memory.
        // retain() does not reduce capacity, so maps that held thousands of entries
        // keep their allocation. shrink_to_fit after mass-despawn ticks prevents
        // long-lived excess capacity.
        let total_despawns = bulk_despawn_count + dot_count;
        if total_despawns > 100 {
            self.cooldowns.shrink_to_fit();
            self.state.combat.active_windows.shrink_to_fit();
            self.weapon_swap_cooldowns.shrink_to_fit();
            // Cap the physics body pool so disabled bodies don't accumulate
            // unbounded after large encounter wipes.
            self.physics.drain_pool(200);
        }

        // Dedup: if an entity transitions DespawnPending → Removed within the same tick
        // (the normal combat-death path), drop the intermediate DespawnPending entry.
        // Sending both causes two entity.on_update callbacks on the coordinator and two
        // DB writes, but the final state is the same. Pruning here reduces churn.
        {
            let removed_ids: HashSet<EntityId> = state_updates
                .iter()
                .filter(|(_, s)| *s == game_schema::EntityState::Removed)
                .map(|(id, _)| *id)
                .collect();
            state_updates.retain(|(id, s)| {
                !(*s == game_schema::EntityState::DespawnPending && removed_ids.contains(id))
            });
        }

        (state_updates, dot_health_updates)
    }
}
