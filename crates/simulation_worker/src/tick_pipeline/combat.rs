use super::*;

/// Compute effective CC duration after diminishing returns and cc_duration_reduce.
///
/// Returns `(effective_ticks, dr_immune_flag)`. When `dr_immune_flag` is true,
/// DR made the entity fully immune (4th+ application in window) and
/// `effective_ticks` is 0.
fn apply_dr_reduction(
    ticks: u32,
    effect: game_schema::CCEffect,
    dr_immune: bool,
    cc_reduce: f32,
    current_tick: TickId,
    dr_tracker: &mut game_core::combat::tactical::DRTracker,
) -> (u32, bool) {
    use game_core::combat::tactical::CCCategory;
    if ticks == 0 {
        return (0, false);
    }
    let dr_mult = if dr_immune {
        1.0
    } else {
        let category = CCCategory::from_cc_effect(effect);
        dr_tracker.apply(category, current_tick)
    };
    if dr_mult <= 0.0 {
        return (0, true);
    }
    let reduced = (ticks as f32 * dr_mult * (1.0 - cc_reduce)).round() as u32;
    (reduced.max(1), false)
}

impl TickPipeline {
    // ── Phase 3.5: Projectile movement ──────────────────────────

    pub(super) fn phase_projectile_movement(&mut self) {
        let ids = self.state.combat.hitboxes.armed_projectile_ids();
        let mut expired = Vec::new();
        for exec_id in ids {
            let hb = match self.state.combat.hitboxes.get_mut(exec_id) {
                Some(hb) => hb,
                None => continue,
            };
            let proj = match hb.projectile.as_mut() {
                Some(p) => p,
                None => continue,
            };
            // Snapshot current position for swept collision detection.
            proj.prev_position = proj.position;
            // Advance position along travel direction.
            proj.position.x += proj.direction.x * proj.speed;
            proj.position.y += proj.direction.y * proj.speed;
            proj.position.z += proj.direction.z * proj.speed;

            // Check range limit.
            let dx = proj.position.x - proj.origin.x;
            let dy = proj.position.y - proj.origin.y;
            let dz = proj.position.z - proj.origin.z;
            let dist_sq = dx * dx + dy * dy + dz * dz;
            if dist_sq >= proj.max_range_sq {
                expired.push(exec_id);
                continue;
            }

            // Move the world-space sensor.
            if let Some(handle) = hb.sensor_handle {
                self.physics.set_sensor_position(handle, proj.position);
            }
        }
        // Remove projectiles that exceeded max range.
        for exec_id in expired {
            if let Some(removed) = self.state.combat.hitboxes.remove(exec_id) {
                if let Some(handle) = removed.sensor_handle {
                    self.physics.remove_sensor(handle);
                }
                self.emit_event(removed.owner, EventPayload::SkillObjectRemoved {
                    execution_id: exec_id.0,
                });
                self.emit_event(removed.owner, EventPayload::HitboxRemoved {
                    ability_id: removed.ability_id,
                });
            }
        }
    }

    // ── Phase 4: Physics integration ────────────────────────────

    pub(super) fn phase_physics_step(&mut self) {
        self.physics.step(self.dt);
    }

    // ── Phase 5: Contact collection ─────────────────────────────

    pub(super) fn phase_contact_collection(&mut self) {
        self.state.physics.contacts = self.physics.drain_collision_events();
        self.summary.contacts = self.state.physics.contacts.len();
    }

    // ── Phase 6: Combat resolution ──────────────────────────────

    pub(super) fn phase_combat_resolution(&mut self) {
        // Build the cover-blocker list once per tick so apply_hit_damage's
        // cover check iterates only the (typically small) set of blocking
        // entities rather than scanning all tactical slots on every hit.
        self.rebuild_cover_blockers();

        self.resolve_hits();
        self.resolve_projectile_hits();
        self.resolve_compensated_hits();
        self.resolve_periodic_damage();
    }

    /// Rebuild the per-tick blocking-entity index used by the cover check.
    ///
    /// Called once at the start of Phase 6. Also exposed for tests that call
    /// `apply_hit_damage` directly without going through `phase_combat_resolution`.
    pub(crate) fn rebuild_cover_blockers(&mut self) {
        self.cover_blockers.clear();
        for (i, slot) in self.state.combat.tactical.iter().enumerate() {
            if !(slot.blocking || slot.block_grace) { continue; }
            if let Some(id) = self.state.entities.lookup_by_slot(i) {
                self.cover_blockers.push((i, id));
            }
        }
    }

    /// Shared damage pipeline: ability lookup → tactical routing → buff multipliers
    /// → charge tier multiplier → damage application → threat → events.
    pub(super) fn apply_hit_damage(
        &mut self,
        attacker: EntityId,
        target: EntityId,
        target_idx: EntityIndex,
        ability_id: u32,
        compensated: bool,
        exec_id: Option<AbilityExecutionId>,
    ) {
        // Layer isolation: reject cross-layer damage at the central choke point.
        if !self.same_layer(attacker, target) {
            return;
        }

        let ability = match self.abilities.get(ability_id) {
            Some(a) => a,
            None => return,
        };

        // Team-based target filter: skip if the ability cannot affect this target.
        match ability.target_filter {
            TargetFilter::All => {} // no restriction
            TargetFilter::Hostile => {
                let attacker_team = self.state.entities.lookup(attacker)
                    .map(|idx| self.team_of_idx(idx))
                    .unwrap_or(0);
                let target_team = self.team_of_idx(target_idx);
                // Same non-zero team → friendly, reject.
                if attacker_team != 0 && target_team != 0 && attacker_team == target_team {
                    return;
                }
            }
            TargetFilter::Friendly => {
                let attacker_team = self.state.entities.lookup(attacker)
                    .map(|idx| self.team_of_idx(idx))
                    .unwrap_or(0);
                let target_team = self.team_of_idx(target_idx);
                // Must share a non-zero team.
                if attacker_team == 0 || target_team == 0 || attacker_team != target_team {
                    return;
                }
            }
        }

        // Resolve charge-tier damage multiplier from execution context.
        let charge_mult: f32 = exec_id
            .and_then(|eid| self.state.combat.executions.get(eid))
            .map(|ctx| {
                let tier = ctx.params.charge_level as usize;
                ability.charge_tiers.as_ref()
                    .and_then(|tiers| tiers.get(tier))
                    .map(|t| t.damage_mult)
                    .unwrap_or(1.0)
            })
            .unwrap_or(1.0);

        let base_damage = ability.base_damage * charge_mult;
        let damage_type = ability.damage_type;
        let threat_mult = ability.threat_multiplier;
        let on_hit_buffs = ability.on_hit_buffs.clone();
        let knockback_force = ability.knockback_force;
        let pull_force = ability.pull_force;
        let launch_lift = ability.launch_lift;
        let launch_recovery_ticks = ability.launch_recovery_ticks;
        let stun_ticks = ability.stun_ticks;
        let knockdown_ticks = ability.knockdown_ticks;
        let sleep_ticks = ability.sleep_ticks;
        let silence_ticks = ability.silence_ticks;
        let fear_ticks = ability.fear_ticks;

        let attacker_idx = self.state.entities.lookup(attacker);

        // ── Sleep break ────────────────────────────────────────────
        // If the target is sleeping, incoming damage breaks the sleep BEFORE
        // applying the damage. The damage still lands after the break.
        {
            let t = &self.state.combat.tactical[target_idx.as_usize()];
            let is_sleeping = t.movement_conditions.contains(
                game_core::combat::tactical::MovementConditions::SLEEPING,
            );
            if is_sleeping {
                self.clear_cc_by_effect(target, target_idx, game_schema::CCEffect::Sleep);
                self.state.status.remove_cc_debuff(target_idx, game_schema::CCEffect::Sleep);
                self.stats_dirty.insert(target);
                audit!(self.state, Tactical, Combat, 6, Some(target), "sleep_broken_by_damage");
                self.emit_event(target, EventPayload::CCCleared {
                    cc_effect: game_schema::CCEffect::Sleep,
                    source: attacker,
                });
            }
        }

        // Tactical routing: dodge evades entirely, block reduces damage.
        let tactical = self.state.combat.tactical[target_idx.as_usize()];
        if tactical.is_dodging() {
            self.pending_events.push(SimEvent {
                tick_id: self.current_tick,
                event_sequence: self.event_sequence,
                entity_id: target,
                payload: EventPayload::Dodged {
                    source: attacker,
                    ability_id,
                },
            });
            self.event_sequence += 1;
            return;
        }

        // True damage bypasses block and damage-reduction modifiers entirely.
        let is_true_damage = damage_type == game_protocol::event::DamageType::True;

        // Directional block: block only mitigates damage from the front arc.
        // True damage ignores blocking completely.
        // Compute dot product between target's facing direction and the vector
        // from target toward the attacker. If the attacker is behind the
        // blocker (dot < 0), blocking is ineffective.
        const BLOCK_DOT_THRESHOLD: f32 = 0.0; // 0 = 180° front arc
        let is_blocking = tactical.blocking || tactical.block_grace;
        let facing_attacker = if is_blocking && !is_true_damage {
            if let (Some(target_t), Some(attacker_t)) = (
                self.physics.get_transform(target),
                self.physics.get_transform(attacker),
            ) {
                let yaw = 2.0 * target_t.rotation.y.atan2(target_t.rotation.w);
                let facing = (yaw.sin(), yaw.cos()); // (x, z)
                let dx = attacker_t.position.x - target_t.position.x;
                let dz = attacker_t.position.z - target_t.position.z;
                let len = (dx * dx + dz * dz).sqrt();
                if len > 1e-6 {
                    let dot = facing.0 * (dx / len) + facing.1 * (dz / len);
                    dot >= BLOCK_DOT_THRESHOLD
                } else {
                    true // overlapping positions — allow block
                }
            } else {
                true // no transform available — allow block
            }
        } else {
            false // not blocking at all, or true damage
        };

        // Perfect block window: first N ticks of a block sequence deal zero damage.
        const PERFECT_BLOCK_TICKS: u64 = 3;
        let (block_factor, perfect_block) = if is_blocking && facing_attacker {
            let perfect = tactical.block_start_tick
                .map_or(false, |start| self.current_tick.0.saturating_sub(start.0) < PERFECT_BLOCK_TICKS);
            if perfect { (0.0, true) } else { (0.5, false) }
        } else {
            (1.0, false)
        };

        // Apply damage_out_mult from attacker's cached StatBlock.
        let out_mult: f32 = attacker_idx.map_or(1.0, |idx| {
            self.state.stats.get(idx).damage_out_mult
        });
        // True damage ignores damage_in_mult (incoming reduction/amplification).
        let in_mult: f32 = if is_true_damage {
            1.0
        } else {
            self.state.stats.get(target_idx).damage_in_mult
        };
        let effective_damage = base_damage * out_mult * in_mult * block_factor;

        // ── Cover check: allies behind a blocking player take reduced damage ──
        // Iterates blocking entities to see if any friendly blocker is
        // interposed between the attacker and the target within a rear cone.
        // True damage bypasses cover (same as self-block).
        const COVER_RADIUS_SQ: f32 = 16.0;     // 4 units
        const COVER_DOT_THRESHOLD: f32 = -0.3;  // ~110° rear arc
        const COVER_FACTOR: f32 = 0.7;          // 30% damage reduction
        let (cover_factor, cover_source) = if !is_true_damage && block_factor >= 1.0 {
            // Only check cover if the target isn't already self-blocking.
            if let (Some(tp), Some(ap)) = (
                self.physics.get_transform(target),
                self.physics.get_transform(attacker),
            ) {
                let mut best_factor = 1.0f32;
                let mut best_blocker: Option<EntityId> = None;
                for &(i, blocker_id) in &self.cover_blockers {
                    if blocker_id == target || blocker_id == attacker { continue; }
                    // Same-team check: blocker and target must share a non-zero team.
                    // Team 0 (unassigned) never covers anyone.
                    let blocker_idx = self.state.entities.index_at(i);
                    let blocker_team = self.team_of_idx(blocker_idx);
                    let target_team = self.team_of_idx(target_idx);
                    let same_team = blocker_team != 0 && blocker_team == target_team;
                    if !same_team { continue; }
                    let Some(bp) = self.physics.get_transform(blocker_id) else { continue };
                    // Distance: target must be within COVER_RADIUS of blocker.
                    let dx = tp.position.x - bp.position.x;
                    let dz = tp.position.z - bp.position.z;
                    let dist_sq = dx * dx + dz * dz;
                    if dist_sq > COVER_RADIUS_SQ { continue; }
                    // Cone: target must be behind blocker (relative to blocker's facing).
                    let yaw = 2.0 * bp.rotation.y.atan2(bp.rotation.w);
                    let facing = (yaw.sin(), yaw.cos());
                    let len = dist_sq.sqrt();
                    if len < 1e-6 { continue; }
                    let dot = facing.0 * (dx / len) + facing.1 * (dz / len);
                    if dot > COVER_DOT_THRESHOLD { continue; } // target not behind blocker
                    // Interposition: blocker must be closer to attacker than target is.
                    let bax = bp.position.x - ap.position.x;
                    let baz = bp.position.z - ap.position.z;
                    let tax = tp.position.x - ap.position.x;
                    let taz = tp.position.z - ap.position.z;
                    if bax * bax + baz * baz >= tax * tax + taz * taz { continue; }
                    if COVER_FACTOR < best_factor {
                        best_factor = COVER_FACTOR;
                        best_blocker = Some(blocker_id);
                    }
                }
                (best_factor, best_blocker)
            } else {
                (1.0, None)
            }
        } else {
            (1.0, None)
        };
        let effective_damage = effective_damage * cover_factor;

        let actual = self.state.combat.health.apply_damage(target_idx, effective_damage, Some(attacker));
        if compensated {
            audit!(self.state, Health, Combat, 6, Some(target), "damage_compensated");
        } else {
            audit!(self.state, Health, Combat, 6, Some(target), "damage");
        }
        self.summary.damage_events += 1;

        // Only add threat if the attacker is still alive — a delayed projectile or
        // DoT from a dead caster must not re-insert them into the threat table.
        if self.state.entities.lookup(attacker).is_some() {
            if let Some(table) = self.state.combat.threat_tables.get_mut(target_idx) {
                table.add_threat(attacker, actual * threat_mult);
                if compensated {
                    audit!(self.state, Threat, Combat, 6, Some(target), "add_threat_compensated");
                } else {
                    audit!(self.state, Threat, Combat, 6, Some(target), "add_threat");
                }
            }
        }

        // Emit Blocked event when blocking from the front (even if perfect block dealt zero damage).
        if is_blocking && facing_attacker {
            self.pending_events.push(SimEvent {
                tick_id: self.current_tick,
                event_sequence: self.event_sequence,
                entity_id: target,
                payload: EventPayload::Blocked {
                    source: attacker,
                    ability_id,
                    damage_taken: actual,
                    perfect: perfect_block,
                },
            });
            self.event_sequence += 1;
        }
        // Emit Covered event when an ally's block stance reduced our damage.
        if let Some(blocker) = cover_source {
            self.pending_events.push(SimEvent {
                tick_id: self.current_tick,
                event_sequence: self.event_sequence,
                entity_id: target,
                payload: EventPayload::Covered {
                    blocker,
                    ability_id,
                    damage_taken: actual,
                },
            });
            self.event_sequence += 1;
        }
        self.pending_events.push(SimEvent {
            tick_id: self.current_tick,
            event_sequence: self.event_sequence,
            entity_id: target,
            payload: EventPayload::Damage {
                source: attacker,
                amount: actual,
                damage_type,
            },
        });
        self.event_sequence += 1;

        self.pending_events.push(SimEvent {
            tick_id: self.current_tick,
            event_sequence: self.event_sequence,
            entity_id: target,
            payload: EventPayload::SkillHit {
                skill_id: ability_id,
                source: attacker,
            },
        });
        self.event_sequence += 1;

        // Apply on-hit buffs/debuffs to the target.
        for &bid in &on_hit_buffs {
            if let Some(template) = self.buff_registry.get(bid) {
                let active = game_core::combat::status::ActiveBuff::from_template(
                    template, attacker, target, self.current_tick,
                );
                self.state.status.apply_or_stack_buff(target_idx, active);
                self.stats_dirty.insert(target);
                audit!(self.state, Buff, Combat, 6, Some(target), "on_hit_buff");
                let duration = template.duration_ticks.unwrap_or(0);
                self.emit_event(target, EventPayload::BuffApplied {
                    buff_id: bid,
                    source: attacker,
                    duration_ticks: duration,
                });
            } else {
                warn!("on_hit_buff: buff_id {} not found in registry", bid);
            }
        }

        // ── CC application ─────────────────────────────────────────
        // Determine whether any CC effect should be applied from this ability.
        // CC is skipped if the target blocked successfully or if stability absorbs it.
        // Sleep bypasses stability (per design: stability does NOT absorb sleep).
        // Knockback is arc-based displacement CC — it participates in stability
        // checks (like launch/pull) so that stability stacks can prevent it.
        //
        // True damage → CC is unblockable (mirrors damage: block provides no
        // mitigation against true damage). Stability and DR still apply.
        // `facing_attacker` is already false when is_true_damage (see above),
        // so this falls out naturally — stated here for clarity.
        let cc_blocked = is_blocking && facing_attacker;
        let has_stability_cc = stun_ticks > 0 || knockdown_ticks > 0
            || launch_lift > 0.0 || pull_force > 0.0
            || knockback_force > 0.0
            || silence_ticks > 0 || fear_ticks > 0;
        let has_sleep = sleep_ticks > 0;
        if (has_stability_cc || has_sleep) && !cc_blocked {
            // Stability check: only for non-sleep CC.
            let stability_absorbed = if has_stability_cc {
                self.try_consume_stability(target, target_idx)
            } else {
                false
            };
            if !stability_absorbed || has_sleep {
                self.apply_cc_effects(
                    attacker, target, target_idx,
                    if stability_absorbed { 0 } else { stun_ticks },
                    if stability_absorbed { 0 } else { knockdown_ticks },
                    if stability_absorbed { 0.0 } else { launch_lift },
                    if stability_absorbed { 0 } else { launch_recovery_ticks },
                    if stability_absorbed { 0.0 } else { pull_force },
                    if stability_absorbed { 0.0 } else { knockback_force },
                    sleep_ticks,
                    if stability_absorbed { 0 } else { silence_ticks },
                    if stability_absorbed { 0 } else { fear_ticks },
                );
            }
        }
    }

    /// Try to consume one stability stack from the target's active buffs.
    /// Returns `true` if a stack was consumed (CC is absorbed), `false` otherwise.
    fn try_consume_stability(&mut self, target: EntityId, target_idx: EntityIndex) -> bool {
        let buffs = self.state.status.get_buffs(target_idx);
        let stability_buff = buffs.iter().find(|b| b.modifiers.stability == Some(true));
        let Some(buff_id) = stability_buff.map(|b| b.buff_id) else { return false };

        // Consume one stack. If stacks reach 0, remove the buff entirely.
        let removed = self.state.status.consume_stack(target_idx, buff_id);
        self.stats_dirty.insert(target);
        audit!(self.state, Buff, Combat, 6, Some(target), "stability_consumed");
        self.emit_event(target, EventPayload::StabilityConsumed { buff_id });
        if removed {
            self.emit_event(target, EventPayload::BuffExpired { buff_id });
        }
        true
    }

    /// Apply CC effects from an ability hit to the target.
    ///
    /// Arc-based CCs (launch, pull, knockback) are mutually exclusive —
    /// only the highest-priority arc fires. Priority: launch > pull > knockback.
    /// When an arc CC fires, any timed hard-CC on the same ability (stun_ticks,
    /// knockdown_ticks) is stored as *arc recovery* and applied on landing
    /// (same pattern as launch → knockdown recovery).
    ///
    /// If no arc CC fires, stun/knockdown apply as immediate timed CC.
    /// Sleep, silence, and fear always apply regardless of arc selection.
    pub(crate) fn apply_cc_effects(
        &mut self,
        attacker: EntityId,
        target: EntityId,
        target_idx: EntityIndex,
        stun_ticks: u32,
        knockdown_ticks: u32,
        launch_lift: f32,
        launch_recovery_ticks: u32,
        pull_force: f32,
        knockback_force: f32,
        sleep_ticks: u32,
        silence_ticks: u32,
        fear_ticks: u32,
    ) {
        use game_core::combat::tactical::MovementConditions;
        const CC_LAUNCH_GRAVITY: f32 = 20.0;
        const CC_ARC_GRAVITY: f32 = 20.0;

        // ── DR + cc_duration_reduce ────────────────────────────────
        let dr_immune = self.state.combat.tactical[target_idx.as_usize()].dr_immune;
        let cc_reduce = self.state.stats.get(target_idx).cc_duration_reduce;
        let current_tick = self.current_tick;

        // ── Arc-based CCs (mutually exclusive) ─────────────────────
        // When an arc fires, stun/knockdown from the same ability become
        // arc recovery (applied on landing by drive_arc_movement).
        let mut arc_fired = false;

        if launch_lift > 0.0 {
            // Launch recovery defaults to knockdown from launch_recovery_ticks.
            // If the ability also has stun_ticks, prefer stun as recovery instead.
            let (raw_recovery, recovery_effect) = if stun_ticks > 0 {
                (stun_ticks, Some(game_schema::CCEffect::Stun))
            } else if launch_recovery_ticks > 0 {
                (launch_recovery_ticks, Some(game_schema::CCEffect::Knockdown))
            } else if knockdown_ticks > 0 {
                (knockdown_ticks, Some(game_schema::CCEffect::Knockdown))
            } else {
                (0, None)
            };
            // Apply DR + cc_duration_reduce to recovery CC at authoring time
            // so controller.rs doesn't need to re-derive it on landing.
            let (recovery_ticks, recovery_immune) = if let Some(eff) = recovery_effect {
                apply_dr_reduction(raw_recovery, eff, dr_immune, cc_reduce, current_tick,
                    &mut self.state.combat.tactical[target_idx.as_usize()].dr_tracker)
            } else {
                (0, false)
            };
            if recovery_immune {
                self.emit_event(target, EventPayload::CCImmune {
                    cc_effect: recovery_effect.unwrap(),
                    source: attacker,
                });
            }
            let t = &mut self.state.combat.tactical[target_idx.as_usize()];
            t.movement_conditions.insert(MovementConditions::FLOATING);
            t.arc_recovery_ticks = recovery_ticks;
            t.arc_recovery_effect = if recovery_ticks > 0 { recovery_effect } else { None };
            t.arc_attacker = Some(attacker);
            t.arc_state = Some(game_core::combat::tactical::ArcState {
                velocity: Vec3f { x: 0.0, y: launch_lift, z: 0.0 },
                gravity: CC_LAUNCH_GRAVITY,
                gravity_only: false,
            });
            t.is_grounded = false;
            arc_fired = true;
            audit!(self.state, Tactical, Combat, 6, Some(target), "cc_launch");
            self.emit_event(target, EventPayload::Launched { source: attacker });
        } else if pull_force > 0.0 {
            if let (Some(target_t), Some(attacker_t)) = (
                self.physics.get_transform(target),
                self.physics.get_transform(attacker),
            ) {
                let dx = attacker_t.position.x - target_t.position.x;
                let dz = attacker_t.position.z - target_t.position.z;
                let len = (dx * dx + dz * dz).sqrt();
                let (dir_x, dir_z) = if len > 1e-6 {
                    (dx / len, dz / len)
                } else {
                    (0.0, 1.0)
                };
                // Determine recovery CC: stun > knockdown > none.
                let (raw_recovery, recovery_effect) = if stun_ticks > 0 {
                    (stun_ticks, Some(game_schema::CCEffect::Stun))
                } else if knockdown_ticks > 0 {
                    (knockdown_ticks, Some(game_schema::CCEffect::Knockdown))
                } else {
                    (0, None)
                };
                let (recovery_ticks, recovery_immune) = if let Some(eff) = recovery_effect {
                    apply_dr_reduction(raw_recovery, eff, dr_immune, cc_reduce, current_tick,
                        &mut self.state.combat.tactical[target_idx.as_usize()].dr_tracker)
                } else {
                    (0, false)
                };
                if recovery_immune {
                    self.emit_event(target, EventPayload::CCImmune {
                        cc_effect: recovery_effect.unwrap(),
                        source: attacker,
                    });
                }
                let t = &mut self.state.combat.tactical[target_idx.as_usize()];
                t.movement_conditions.insert(MovementConditions::FLOATING);
                t.arc_recovery_ticks = recovery_ticks;
                t.arc_recovery_effect = if recovery_ticks > 0 { recovery_effect } else { None };
                t.arc_attacker = Some(attacker);
                const PULL_LIFT: f32 = 2.0;
                t.arc_state = Some(game_core::combat::tactical::ArcState {
                    velocity: Vec3f { x: dir_x * pull_force, y: PULL_LIFT, z: dir_z * pull_force },
                    gravity: CC_ARC_GRAVITY,
                    gravity_only: false,
                });
                t.is_grounded = false;
                arc_fired = true;
                audit!(self.state, Tactical, Combat, 6, Some(target), "cc_pull");
                self.emit_event(target, EventPayload::Pulled { source: attacker });
            }
        } else if knockback_force > 0.0 {
            if let (Some(target_t), Some(attacker_t)) = (
                self.physics.get_transform(target),
                self.physics.get_transform(attacker),
            ) {
                let dx = target_t.position.x - attacker_t.position.x;
                let dz = target_t.position.z - attacker_t.position.z;
                let len = (dx * dx + dz * dz).sqrt();
                let (dir_x, dir_z) = if len > 1e-6 {
                    (dx / len, dz / len)
                } else {
                    (0.0, 1.0)
                };
                // Determine recovery CC: stun > knockdown > none.
                let (raw_recovery, recovery_effect) = if stun_ticks > 0 {
                    (stun_ticks, Some(game_schema::CCEffect::Stun))
                } else if knockdown_ticks > 0 {
                    (knockdown_ticks, Some(game_schema::CCEffect::Knockdown))
                } else {
                    (0, None)
                };
                let (recovery_ticks, recovery_immune) = if let Some(eff) = recovery_effect {
                    apply_dr_reduction(raw_recovery, eff, dr_immune, cc_reduce, current_tick,
                        &mut self.state.combat.tactical[target_idx.as_usize()].dr_tracker)
                } else {
                    (0, false)
                };
                if recovery_immune {
                    self.emit_event(target, EventPayload::CCImmune {
                        cc_effect: recovery_effect.unwrap(),
                        source: attacker,
                    });
                }
                const KNOCKBACK_LIFT: f32 = 3.0;
                const KNOCKBACK_GRAVITY: f32 = 20.0;
                let t = &mut self.state.combat.tactical[target_idx.as_usize()];
                t.movement_conditions.insert(MovementConditions::FLOATING);
                t.arc_recovery_ticks = recovery_ticks;
                t.arc_recovery_effect = if recovery_ticks > 0 { recovery_effect } else { None };
                t.arc_attacker = Some(attacker);
                t.arc_state = Some(game_core::combat::tactical::ArcState {
                    velocity: Vec3f {
                        x: dir_x * knockback_force,
                        y: KNOCKBACK_LIFT,
                        z: dir_z * knockback_force,
                    },
                    gravity: KNOCKBACK_GRAVITY,
                    gravity_only: false,
                });
                t.is_grounded = false;
                arc_fired = true;
                audit!(self.state, Tactical, Combat, 6, Some(target), "cc_knockback");
                self.emit_event(target, EventPayload::Knockback {
                    source: attacker,
                    force: knockback_force,
                });
            }
        }

        // ── Timed hard-CC (only if no arc consumed them as recovery) ──
        if !arc_fired {
            if stun_ticks > 0 {
                let (effective, immune) = apply_dr_reduction(
                    stun_ticks, game_schema::CCEffect::Stun, dr_immune, cc_reduce, current_tick,
                    &mut self.state.combat.tactical[target_idx.as_usize()].dr_tracker,
                );
                if immune {
                    self.emit_event(target, EventPayload::CCImmune {
                        cc_effect: game_schema::CCEffect::Stun,
                        source: attacker,
                    });
                } else if effective > 0 {
                    let until = TickId(current_tick.0 + effective as u64);
                    let extends = self.cc_debuff_expiry(target_idx, 500).map_or(true, |prev| until > prev);
                    if extends {
                        self.state.combat.tactical[target_idx.as_usize()]
                            .movement_conditions.insert(MovementConditions::STUNNED);
                        self.insert_cc_debuff(target, target_idx, attacker, 500, until);
                    }
                    audit!(self.state, Tactical, Combat, 6, Some(target), "cc_stun");
                    self.emit_event(target, EventPayload::Stunned {
                        source: attacker,
                        duration_ticks: effective,
                    });
                }
            }

            if knockdown_ticks > 0 {
                let (effective, immune) = apply_dr_reduction(
                    knockdown_ticks, game_schema::CCEffect::Knockdown, dr_immune, cc_reduce, current_tick,
                    &mut self.state.combat.tactical[target_idx.as_usize()].dr_tracker,
                );
                if immune {
                    self.emit_event(target, EventPayload::CCImmune {
                        cc_effect: game_schema::CCEffect::Knockdown,
                        source: attacker,
                    });
                } else if effective > 0 {
                    let until = TickId(current_tick.0 + effective as u64);
                    let extends = self.cc_debuff_expiry(target_idx, 501).map_or(true, |prev| until > prev);
                    if extends {
                        self.state.combat.tactical[target_idx.as_usize()]
                            .movement_conditions.insert(MovementConditions::KNOCKED_DOWN);
                        self.insert_cc_debuff(target, target_idx, attacker, 501, until);
                    }
                    audit!(self.state, Tactical, Combat, 6, Some(target), "cc_knockdown");
                    self.emit_event(target, EventPayload::KnockedDown {
                        source: attacker,
                        duration_ticks: effective,
                    });
                }
            }
        }

        // ── Sleep (always applies, regardless of arc) ──────────────
        if sleep_ticks > 0 {
            let (effective, immune) = apply_dr_reduction(
                sleep_ticks, game_schema::CCEffect::Sleep, dr_immune, cc_reduce, current_tick,
                &mut self.state.combat.tactical[target_idx.as_usize()].dr_tracker,
            );
            if immune {
                self.emit_event(target, EventPayload::CCImmune {
                    cc_effect: game_schema::CCEffect::Sleep,
                    source: attacker,
                });
            } else if effective > 0 {
                let until = TickId(current_tick.0 + effective as u64);
                let extends = self.cc_debuff_expiry(target_idx, 502).map_or(true, |prev| until > prev);
                if extends {
                    self.state.combat.tactical[target_idx.as_usize()]
                        .movement_conditions.insert(MovementConditions::SLEEPING);
                    self.insert_cc_debuff(target, target_idx, attacker, 502, until);
                }
                audit!(self.state, Tactical, Combat, 6, Some(target), "cc_sleep");
                self.emit_event(target, EventPayload::Slept {
                    source: attacker,
                    duration_ticks: effective,
                });
            }
        }

        // ── Silence (always applies, regardless of arc) ────────────
        if silence_ticks > 0 {
            let (effective, immune) = apply_dr_reduction(
                silence_ticks, game_schema::CCEffect::Silence, dr_immune, cc_reduce, current_tick,
                &mut self.state.combat.tactical[target_idx.as_usize()].dr_tracker,
            );
            if immune {
                self.emit_event(target, EventPayload::CCImmune {
                    cc_effect: game_schema::CCEffect::Silence,
                    source: attacker,
                });
            } else if effective > 0 {
                let until = TickId(current_tick.0 + effective as u64);
                let extends = self.cc_debuff_expiry(target_idx, 503).map_or(true, |prev| until > prev);
                if extends {
                    self.state.combat.tactical[target_idx.as_usize()]
                        .movement_conditions.insert(MovementConditions::SILENCED);
                    self.insert_cc_debuff(target, target_idx, attacker, 503, until);
                }
                audit!(self.state, Tactical, Combat, 6, Some(target), "cc_silence");
                self.emit_event(target, EventPayload::Silenced {
                    source: attacker,
                    duration_ticks: effective,
                });
            }
        }

        // ── Fear (always applies, regardless of arc) ───────────────
        if fear_ticks > 0 {
            let (effective, immune) = apply_dr_reduction(
                fear_ticks, game_schema::CCEffect::Fear, dr_immune, cc_reduce, current_tick,
                &mut self.state.combat.tactical[target_idx.as_usize()].dr_tracker,
            );
            if immune {
                self.emit_event(target, EventPayload::CCImmune {
                    cc_effect: game_schema::CCEffect::Fear,
                    source: attacker,
                });
            } else if effective > 0 {
                let until = TickId(current_tick.0 + effective as u64);
                let extends = self.cc_debuff_expiry(target_idx, 504).map_or(true, |prev| until > prev);
                if extends {
                    self.state.combat.tactical[target_idx.as_usize()]
                        .movement_conditions.insert(MovementConditions::FEARED);
                    self.state.combat.tactical[target_idx.as_usize()].fear_source = Some(attacker);
                    self.insert_cc_debuff(target, target_idx, attacker, 504, until);
                }
                audit!(self.state, Tactical, Combat, 6, Some(target), "cc_fear");
                self.emit_event(target, EventPayload::Feared {
                    source: attacker,
                    duration_ticks: effective,
                });
            }
        }
    }

    /// Insert a CC condition debuff whose `expires_at` mirrors the CC timer.
    /// Removes any existing debuff of the same buff_id first (CC timer extension replaces the old tracker).
    pub(super) fn insert_cc_debuff(
        &mut self,
        target: EntityId,
        target_idx: EntityIndex,
        attacker: EntityId,
        buff_id: u32,
        until: TickId,
    ) {
        if let Some(template) = self.buff_registry.get(buff_id) {
            let mut ab = game_core::combat::status::ActiveBuff::from_template(
                template, attacker, target, self.current_tick,
            );
            // Override expires_at to match the CC timer (template has duration_ticks = None).
            ab.expires_at = Some(until);
            self.state.status.apply_or_stack_buff(target_idx, ab);
            self.stats_dirty.insert(target);
            let duration = (until.0 - self.current_tick.0) as u32;
            self.emit_event(target, EventPayload::BuffApplied {
                buff_id,
                source: attacker,
                duration_ticks: duration,
            });
        }
    }

    /// Hit resolution chain: contacts → dedup → ability lookup → damage → threat → events.
    /// Owner: combat system. Only called from phase 6.
    fn resolve_hits(&mut self) {
        use game_core::physics_backend::ColliderKind;

        // Process sensor contacts — hitbox vs hurtbox overlaps.
        let contacts: Vec<_> = self.state.physics.contacts
            .iter()
            .filter(|c| c.started && c.is_sensor)
            .cloned()
            .collect();

        for contact in &contacts {
            // Normalise the pair: acting collider (Hitbox, future: Projectile) first.
            // Eliminates duplicate match arms — each interaction rule is stated once.
            let (acting, receiving, attacker, target) = normalize_contact_pair(
                contact.kind1, contact.entity1,
                contact.kind2, contact.entity2,
            );

            // Dispatch on interaction type. Today only (Hitbox → Hurtbox/Body) deals damage.
            let (exec_id, attacker, target) = match (acting, receiving) {
                (ColliderKind::Hitbox(eid_raw), ColliderKind::Body | ColliderKind::Hurtbox) => {
                    (AbilityExecutionId(eid_raw), attacker, target)
                }
                _ => continue,
            };

            // Skip projectile hitboxes — world-space sensors are standalone
            // colliders (no rigid body parent), so Rapier does not reliably fire
            // Started events when they are moved via set_translation().
            // resolve_projectile_hits handles them with manual Parry tests.
            //
            // Skip lag-compensated body-attached hitboxes — they are handled
            // exclusively by resolve_compensated_hits against historical
            // positions. Detached world sensors (GroundTarget/CasterOffset)
            // keep an authoritative world pose and must still resolve through
            // their actual Rapier sensor at that pose.
            if self.state.combat.hitboxes.get(exec_id)
                .is_some_and(|hb| hb.projectile.is_some() || (hb.rewind_ticks > 0 && !hb.world_sensor))
            {
                continue;
            }

            // Skip self-hits.
            if attacker == target {
                continue;
            }

            // Layer isolation: hitboxes only affect entities on the same layer.
            if !self.same_layer(attacker, target) {
                continue;
            }

            // Track overlapping for periodic damage (HazardZone).
            self.state.combat.hitboxes.add_overlapping(exec_id, target);

            // Dedup: skip if this hitbox already hit this target.
            if !self.state.combat.hitboxes.record_hit(exec_id, target) {
                continue;
            }

            // Look up ability data for damage values — get ability_id from the hitbox record.
            // The hitbox must still exist after record_hit succeeds.
            let ability_id = match self.state.combat.hitboxes.get(exec_id) {
                Some(hb) => hb.ability_id,
                None => continue,
            };
            // Translate target EntityId → dense index. Skip non-active targets
            // (e.g. Spawning) for consistency with resolve_compensated_hits.
            let target_idx = match self.state.entities.lookup(target) {
                Some(idx) if self.state.entities.is_active(idx) => idx,
                _ => continue,
            };

            self.apply_hit_damage(attacker, target, target_idx, ability_id, false, Some(exec_id));
        }

        // Second pass: process Stopped sensor events to clear already_hit for
        // reentry-capable hitboxes and remove from overlapping set.
        for contact in self.state.physics.contacts
            .iter()
            .filter(|c| !c.started && c.is_sensor)
        {
            let (acting, _receiving, _attacker, target) = normalize_contact_pair(
                contact.kind1, contact.entity1,
                contact.kind2, contact.entity2,
            );
            if let ColliderKind::Hitbox(eid_raw) = acting {
                let eid = AbilityExecutionId(eid_raw);
                self.state.combat.hitboxes.clear_hit(eid, target);
                self.state.combat.hitboxes.remove_overlapping(eid, target);
            }
        }

        self.sync_periodic_hitbox_overlaps();
    }

    /// Synchronize continuous hitboxes from the live physics overlap state.
    ///
    /// Rapier's Started/Stopped events are edge-triggered, which means they are
    /// not sufficient for hazards and auras when a sensor is spawned around an
    /// already-overlapping target or when kinematic bodies remain stationary.
    /// For periodic hitboxes, query the sensor's current intersections every
    /// tick so initial entry, exit, and in-zone periodic damage all use the
    /// authoritative overlap state instead of transition bookkeeping alone.
    fn sync_periodic_hitbox_overlaps(&mut self) {
        let periodic_hitboxes = self.state.combat.hitboxes.armed_periodic_ids();
        for exec_id in periodic_hitboxes {
            let (attacker, ability_id, sensor_handle, previous_targets) =
                match self.state.combat.hitboxes.get(exec_id) {
                    Some(hb) => match hb.sensor_handle {
                        Some(sensor_handle) => {
                            (hb.owner, hb.ability_id, sensor_handle, hb.overlapping.clone())
                        }
                        None => continue,
                    },
                    None => continue,
                };

            let current_targets: HashSet<EntityId> = self
                .physics
                .sensor_intersections(sensor_handle)
                .into_iter()
                .collect();

            let exited: Vec<EntityId> = previous_targets
                .difference(&current_targets)
                .copied()
                .collect();
            let entered: Vec<EntityId> = current_targets
                .difference(&previous_targets)
                .copied()
                .collect();

            for target in exited {
                self.state.combat.hitboxes.clear_hit(exec_id, target);
                self.state.combat.hitboxes.remove_overlapping(exec_id, target);
            }

            for target in entered {
                if target == attacker {
                    continue;
                }
                // Layer isolation: skip targets on a different layer.
                if !self.same_layer(attacker, target) {
                    continue;
                }

                self.state.combat.hitboxes.add_overlapping(exec_id, target);

                let target_idx = match self.state.entities.lookup(target) {
                    Some(idx) if self.state.entities.is_active(idx) => idx,
                    _ => continue,
                };

                if !self.state.combat.hitboxes.record_hit(exec_id, target) {
                    continue;
                }

                self.apply_hit_damage(attacker, target, target_idx, ability_id, false, Some(exec_id));
            }
        }
    }

    /// Periodic re-damage for lingering area effects (HazardZone).
    fn resolve_periodic_damage(&mut self) {
        let due = self.state.combat.hitboxes.collect_periodic_due(self.current_tick);
        for (exec_id, attacker, ability_id, targets) in due {
            for target in targets {
                if target == attacker {
                    continue;
                }
                if !self.same_layer(attacker, target) {
                    continue;
                }
                let target_idx = match self.state.entities.lookup(target) {
                    Some(idx) if self.state.entities.is_active(idx) => idx,
                    _ => continue,
                };
                // Clear dedup so record_hit succeeds on re-damage.
                self.state.combat.hitboxes.get_mut(exec_id).map(|hb| {
                    hb.already_hit.remove(&target);
                });
                if !self.state.combat.hitboxes.record_hit(exec_id, target) {
                    continue;
                }
                self.apply_hit_damage(attacker, target, target_idx, ability_id, false, Some(exec_id));
            }
            self.state.combat.hitboxes.mark_periodic_tick(exec_id, self.current_tick);
        }
    }

    /// Projectile hit detection — standalone Parry intersection tests.
    fn resolve_projectile_hits(&mut self) {
        let projectiles: Vec<_> = self
            .state
            .combat
            .hitboxes
            .armed_projectile_ids()
            .into_iter()
            .filter_map(|eid| {
                let hb = self.state.combat.hitboxes.get(eid)?;
                let proj = hb.projectile.as_ref()?;
                Some((eid, hb.owner, hb.ability_id, hb.shape, proj.prev_position, proj.position))
            })
            .collect();

        if projectiles.is_empty() {
            return;
        }

        let mut confirmed_hits: Vec<(EntityId, EntityId, EntityIndex, u32, AbilityExecutionId)> = Vec::new();
        let mut hit_projectiles: Vec<AbilityExecutionId> = Vec::new();

        for (exec_id, attacker, ability_id, shape, prev_pos, curr_pos) in projectiles {
            let hitbox_shape = lag_compensation::hitbox_sensor_shape(shape);
            let pierce = self.state.combat.hitboxes.get(exec_id)
                .map(|hb| hb.pierce)
                .unwrap_or(false);

            for slot in 0..self.state.entities.len() {
                if self.state.entities.states[slot]
                    != game_core::entity::lifecycle::EntityState::Active
                {
                    continue;
                }
                let target_idx = self.state.entities.index_at(slot);
                let target_id = self.state.entities.id_of(target_idx);

                if target_id == attacker {
                    continue;
                }
                if !self.same_layer(attacker, target_id) {
                    continue;
                }
                if self.state.combat.hitboxes.has_hit(exec_id, target_id) {
                    continue;
                }

                let target_pos = match self.physics.get_transform(target_id) {
                    Some(t) => t.position,
                    None => continue,
                };

                if !lag_compensation::swept_shapes_intersect(hitbox_shape, prev_pos, curr_pos, target_pos) {
                    continue;
                }
                if !self.state.combat.hitboxes.record_hit(exec_id, target_id) {
                    continue;
                }

                confirmed_hits.push((attacker, target_id, target_idx, ability_id, exec_id));
                if !pierce {
                    // Projectile consumed on first hit — stop scanning further targets.
                    hit_projectiles.push(exec_id);
                    break;
                }
            }
        }

        for (attacker, target_id, target_idx, ability_id, exec_id) in confirmed_hits {
            self.apply_hit_damage(attacker, target_id, target_idx, ability_id, false, Some(exec_id));
        }

        // Remove projectiles that hit a target (single-target behaviour).
        for exec_id in hit_projectiles {
            if let Some(removed) = self.state.combat.hitboxes.remove(exec_id) {
                if let Some(handle) = removed.sensor_handle {
                    self.physics.remove_sensor(handle);
                }
                self.emit_event(removed.owner, EventPayload::SkillObjectRemoved {
                    execution_id: exec_id.0,
                });
                self.emit_event(removed.owner, EventPayload::HitboxRemoved {
                    ability_id: removed.ability_id,
                });
            }
        }
    }

    /// Lag-compensated hit detection — second pass after `resolve_hits`.
    fn resolve_compensated_hits(&mut self) {
        // Collect compensated hitboxes: armed, rewind_ticks > 0.
        let compensated: Vec<_> = self
            .state
            .combat
            .hitboxes
            .iter_armed_compensated()
            .map(|hb| (hb.execution_id, hb.owner, hb.ability_id, hb.shape, hb.offset, hb.rewind_ticks, hb.max_rewind_ticks))
            .collect();

        if compensated.is_empty() {
            return;
        }

        let global_max = self.global_max_rewind_ticks;
        let mut confirmed_hits: Vec<(EntityId, EntityId, EntityIndex, u32, AbilityExecutionId, u32)> = Vec::new();

        for (exec_id, attacker, ability_id, shape, offset, rewind_ticks, max_rewind_override) in compensated {
            // Apply per-ability cap, then global cap.
            let effective_rewind = rewind_ticks
                .min(max_rewind_override.unwrap_or(global_max))
                .min(global_max);

            if effective_rewind == 0 {
                continue;
            }

            let (attacker_pos, facing) = match self.state.combat.executions.get(exec_id) {
                Some(ctx) => {
                    if let Some(t) = self.physics.get_transform(attacker) {
                        let yaw = 2.0 * t.rotation.y.atan2(t.rotation.w);
                        let current_facing = Vec3f { x: yaw.sin(), y: 0.0, z: yaw.cos() };
                        (t.position, current_facing)
                    } else {
                        (ctx.origin, ctx.facing)
                    }
                }
                None => continue,
            };

            let hitbox_pos =
                lag_compensation::hitbox_world_position(attacker_pos, facing, offset);
            let hitbox_shape = lag_compensation::hitbox_sensor_shape(shape);

            // Determine the historical tick to sample.
            let rewind_tick = TickId(self.current_tick.0.saturating_sub(effective_rewind as u64));
            let snapshot = match self.transform_history.get_snapshot(rewind_tick) {
                Some(s) => s,
                None => continue, // Not enough history yet.
            };

            let candidate_radius = lag_compensation::hitbox_candidate_radius(hitbox_shape);
            let candidates = snapshot.nearby_positions(hitbox_pos, candidate_radius);

            // Test each entity in the snapshot for overlap with the hitbox.
            for (target_id, historical_pos) in candidates {
                // Skip self-hits.
                if target_id == attacker {
                    continue;
                }
                if !self.same_layer(attacker, target_id) {
                    continue;
                }

                // Skip entities already hit by the normal pass or a prior compensated test.
                if self.state.combat.hitboxes.has_hit(exec_id, target_id) {
                    continue;
                }

                // Only compensate for active entities with a dense index.
                let target_idx = match self.state.entities.lookup(target_id) {
                    Some(idx) if self.state.entities.is_active(idx) => idx,
                    _ => continue,
                };

                // Run the standalone shape intersection test.
                if !lag_compensation::shapes_intersect(hitbox_shape, hitbox_pos, historical_pos) {
                    continue;
                }

                // Record this as a hit (dedup).
                if !self.state.combat.hitboxes.record_hit(exec_id, target_id) {
                    continue;
                }

                confirmed_hits.push((attacker, target_id, target_idx, ability_id, exec_id, effective_rewind));
            }
        }

        // Apply damage for all confirmed compensated hits.
        for (attacker, target_id, target_idx, ability_id, exec_id, effective_rewind) in confirmed_hits {
            self.emit_event(target_id, EventPayload::CompensationApplied {
                source: attacker,
                ability_id,
                rewind_ticks: effective_rewind,
            });
            self.apply_hit_damage(attacker, target_id, target_idx, ability_id, true, Some(exec_id));
        }
    }
}
