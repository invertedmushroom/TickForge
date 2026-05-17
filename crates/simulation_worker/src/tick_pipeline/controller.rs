use super::*;

impl TickPipeline {
    // ── Phase 2: Controller update ──────────────────────────────

    pub(super) fn phase_controller_update(&mut self, intents: &[&PlayerIntent]) {
        // Sub-step: drive arc movement for entities mid-vault/leap.
        // Runs before intent processing so arcs take priority over player input
        // (the entity is also rooted, so apply_movement would early-exit anyway).
        self.drive_arc_movement();

        // Sub-step: advance charge timers, detect tier crossings, auto-release.
        // Runs before intent processing so auto-releases fire before new intents.
        self.drive_charging();

        // Sub-step: drive fear movement for feared entities.
        // Feared entities move away from the fear source at 50% speed.
        // Runs before intent processing; is_cc_disabled blocks their normal intents.
        self.drive_fear_movement();

        self.expire_timed_lever_puzzles();

        // Track which (entity, ability) pairs have been cast in this Phase 2 pass.
        // Without this, two UseAbility intents for the same ability targeting the same
        // tick both pass is_on_cooldown() — the cooldown map isn't updated until Phase 3.
        // This closes the same-tick double-cast exploit before the cooldown state exists.
        let mut cast_this_tick: HashSet<(EntityId, u32)> = HashSet::new();

        for intent in intents {
            let entity_id = intent.entity_id;

            // Only process intents for active entities.
            let is_active = self
                .state
                .entities
                .lookup(entity_id)
                .is_some_and(|idx| self.state.entities.is_active(idx));
            if !is_active {
                continue;
            }

            match &intent.action {
                IntentAction::Move(dir) => self.apply_movement(entity_id, dir),
                IntentAction::Stop => self.apply_stop(entity_id),
                IntentAction::FaceTo(dir) => self.handle_face_to(entity_id, dir),
                // TagTarget is selection input for an open lock-on session. Allow it even
                // while CC'd or silenced — the player pre-selected targets before the cast.
                IntentAction::TagTarget(target_id_raw) => {
                    self.handle_tag_target(entity_id, *target_id_raw)
                }
                // CC-disabled entities (stunned, knocked down, floating, sleeping, feared)
                // cannot act, except abilities flagged with `usable_while_cc` (e.g. Stunbreak).
                _ => {
                    let idx = self.state.entities.lookup(entity_id).unwrap();
                    if self.is_cc_disabled(idx) {
                        // Allow UseAbility if the ability is marked usable_while_cc.
                        if let IntentAction::UseAbility(data) = &intent.action {
                            if self.ability_prop(
                                data.ability_id,
                                |ad| ad.cast_requirements().usable_while_cc,
                                false,
                            ) {
                                self.handle_use_ability(intent, &mut cast_this_tick);
                            }
                        }
                        continue;
                    }
                    // Silenced entities can move/jump/block but cannot cast abilities.
                    if self.is_silenced(idx) {
                        match &intent.action {
                            IntentAction::UseAbility(_) | IntentAction::ReleaseAbility(_) => {
                                continue;
                            }
                            _ => {}
                        }
                    }
                    match &intent.action {
                        IntentAction::UseAbility(_data) => {
                            self.handle_use_ability(intent, &mut cast_this_tick)
                        }
                        IntentAction::ReleaseAbility(ability_id) => {
                            self.handle_release_ability(entity_id, *ability_id, &mut cast_this_tick)
                        }
                        IntentAction::Block(data) => self.handle_block(entity_id, data),
                        IntentAction::Interact(target_id_raw) => {
                            self.handle_interact(entity_id, *target_id_raw)
                        }
                        IntentAction::Jump => self.handle_jump(entity_id),
                        IntentAction::WeaponSwap => self.handle_weapon_swap(entity_id),
                        _ => {} // Move/Stop/FaceTo handled above
                    }
                }
            }
        }

        // Sub-step: apply soft character-body repulsion.
        // Runs after all movement is applied so overlapping characters get nudged
        // apart symmetrically. Region-aware — uses each entity's current region
        // to determine which kind-pairs should repulse.
        self.apply_character_repulsion();
    }

    fn apply_movement(&mut self, entity_id: EntityId, dir: &MoveDir) {
        // Rooted entities cannot move — block stance, CC, or channeled skill.
        if let Some(idx) = self.state.entities.lookup(entity_id) {
            if self.is_rooted(idx) {
                return;
            }
        }
        // Normalise direction and scale by the cached movement speed from StatBlock.
        // Grounded movement: ignore vertical component to prevent client "flight".
        let x = dir.dir_x;
        let z = dir.dir_z;
        let len_sq = x * x + z * z;
        // Guard against NaN since `NaN < 1e-6` evaluates to `false` and
        // would let invalid directions through, producing NaN velocities.
        if !len_sq.is_finite() || len_sq < 1e-6 {
            return;
        }
        // Read cached movement speed from StatBlock (recalculated in Phase 1.5).
        let speed = if let Some(idx) = self.state.entities.lookup(entity_id) {
            self.state.stats.get(idx).movement_speed
        } else {
            return;
        };
        let inv_len = 1.0 / len_sq.sqrt();
        let nx = x * inv_len;
        let nz = z * inv_len;

        // Rotate body to face the movement direction (WASD-driven facing).
        let yaw = nx.atan2(nz);
        let half_yaw = yaw * 0.5;
        let rotation = Quatf {
            x: 0.0,
            y: half_yaw.sin(),
            z: 0.0,
            w: half_yaw.cos(),
        };
        self.physics.set_kinematic_rotation(entity_id, rotation);

        // While airborne in a jump arc (gravity_only=true), redirect the arc's XZ velocity
        // for the next tick instead of calling move_character ourselves. `drive_arc_movement`
        // already applied this tick's full displacement (XZ+Y) at the start of Phase 2.
        // Updating XZ here means the player's direction change takes effect on tick N+1.
        // Full ground speed applies in the air — GW2-style free air directional control.
        if let Some(idx) = self.state.entities.lookup(entity_id) {
            if self.state.combat.tactical[idx.as_usize()]
                .arc_state
                .map_or(false, |a| a.gravity_only)
            {
                let t = &mut self.state.combat.tactical[idx.as_usize()];
                if let Some(arc) = &mut t.arc_state {
                    arc.velocity.x = nx * speed;
                    arc.velocity.z = nz * speed;
                }
                audit!(
                    self.state,
                    Tactical,
                    Controller,
                    2,
                    Some(entity_id),
                    "air_dir"
                );
                return;
            }
        }

        let vx = nx * speed;
        let vz = nz * speed;

        // Constant downward pull keeps the KCC pressed against the ground
        // surface so `result.grounded` is reliably `true` when on a walkable
        // surface.  Without this, a purely horizontal desired vector can miss
        // the ground contact (the capsule skin is only ~0.01 of its height).
        // When walking off an edge, this gives a small initial drop (~0.2 u)
        // before the gravity arc below takes over for proper acceleration.
        let ground_pull = game_core::physics_constants::GROUND_PULL;

        // Move via character controller — resolves contacts against static
        // geometry (walls, obstacles) and slides along surfaces.
        let desired = Vec3f {
            x: vx * self.dt,
            y: -ground_pull * self.dt,
            z: vz * self.dt,
        };
        if let Some(result) = self.physics.move_character(entity_id, desired) {
            if let Some(idx) = self.state.entities.lookup(entity_id) {
                self.state.combat.tactical[idx.as_usize()].is_grounded = result.grounded;
                // Walk-off-edge: if KCC reports airborne and no arc exists,
                // inject a gravity-only arc so drive_arc_movement applies
                // proper falling physics (acceleration) from the next tick.
                if !result.grounded {
                    let t = &mut self.state.combat.tactical[idx.as_usize()];
                    if t.arc_state.is_none() {
                        t.arc_state = Some(game_core::combat::tactical::ArcState {
                            velocity: Vec3f {
                                x: vx,
                                y: 0.0,
                                z: vz,
                            },
                            gravity: game_core::physics_constants::FALL_GRAVITY,
                            gravity_only: true,
                        });
                    }
                }
            }
            audit!(
                self.state,
                Transform,
                Controller,
                2,
                Some(entity_id),
                "move"
            );
        }
    }

    fn apply_stop(&mut self, _entity_id: EntityId) {
        // For kinematic_position_based bodies, not issuing a set_kinematic_position
        // this tick is sufficient — the body remains at its current position.
    }

    /// Advance all entities currently in a kinematic arc (vault / leap).
    ///
    /// For each entity with `TacticalState::arc_state`:
    /// 1. Apply gravity to the velocity's Y component.
    /// 2. Feed `velocity * dt` into `move_character`.
    /// 3. If `MoveResult::grounded` (and Y velocity is non-positive, so we're
    ///    past the launch apex), end the arc and unroot the entity.
    fn drive_arc_movement(&mut self) {
        let dt = self.dt;

        // Collect (entity_id, slot_index) for entities with active arcs.
        // Two-pass avoids borrowing `self` mutably while iterating tactical.
        let arc_entities: Vec<(EntityId, usize)> = self
            .state
            .combat
            .tactical
            .iter()
            .enumerate()
            .filter(|(_, t)| t.arc_state.is_some())
            .filter_map(|(i, _)| {
                let id = self.state.entities.lookup_by_slot(i)?;
                Some((id, i))
            })
            .collect();

        for (entity_id, slot) in arc_entities {
            let arc = match &mut self.state.combat.tactical[slot].arc_state {
                Some(a) => a,
                None => continue,
            };
            // Integrate gravity (downward pull).
            arc.velocity.y -= arc.gravity * dt;

            let desired = Vec3f {
                x: arc.velocity.x * dt,
                y: arc.velocity.y * dt,
                z: arc.velocity.z * dt,
            };

            if let Some(result) = self.physics.move_character(entity_id, desired) {
                let vel_y = arc.velocity.y; // last use of `arc` — borrow expires after this
                self.state.combat.tactical[slot].is_grounded = result.grounded;
                // End the arc when the character touches ground and is falling.
                if result.grounded && vel_y <= 0.0 {
                    let impact_speed = vel_y.abs();
                    if impact_speed >= game_core::physics_constants::FALL_DAMAGE_THRESHOLD {
                        let excess =
                            impact_speed - game_core::physics_constants::FALL_DAMAGE_THRESHOLD;
                        let damage = excess * game_core::physics_constants::FALL_DAMAGE_FACTOR;
                        if let Some(idx) = self.state.entities.lookup(entity_id) {
                            let actual_damage =
                                self.state.combat.health.apply_damage(idx, damage, None);
                            if actual_damage > 0.0 {
                                audit!(
                                    self.state,
                                    Health,
                                    Controller,
                                    2,
                                    Some(entity_id),
                                    "fall_damage"
                                );
                                self.emit_event(
                                    entity_id,
                                    EventPayload::FallDamage {
                                        damage: actual_damage,
                                        impact_speed,
                                    },
                                );
                            }
                        }
                    }

                    // Clear CC conditions that end on landing.
                    let t = &mut self.state.combat.tactical[slot];
                    // Remove FLOATING (set by all displacement arcs: launch, pull, knockback).
                    t.movement_conditions
                        .remove(game_core::combat::tactical::MovementConditions::FLOATING);

                    // Apply arc recovery CC if configured (e.g. launch → knockdown,
                    // knockback → stun, pull → stun). Data-driven per ability.
                    // Duration is already DR-reduced + cc_duration_reduced at authoring time.
                    let recovery = if t.arc_recovery_ticks > 0 {
                        if let Some(effect) = t.arc_recovery_effect.take() {
                            let until = TickId(self.current_tick.0 + t.arc_recovery_ticks as u64);
                            let (condition, debuff_id) = match effect {
                                game_schema::CCEffect::Stun => (
                                    game_core::combat::tactical::MovementConditions::STUNNED,
                                    500,
                                ),
                                game_schema::CCEffect::Knockdown => (
                                    game_core::combat::tactical::MovementConditions::KNOCKED_DOWN,
                                    501,
                                ),
                                // Other CC types aren't typical arc recovery but
                                // fall through to knockdown as a safe default.
                                _ => (
                                    game_core::combat::tactical::MovementConditions::KNOCKED_DOWN,
                                    501,
                                ),
                            };
                            t.movement_conditions.insert(condition);
                            t.arc_recovery_ticks = 0;
                            Some((until, debuff_id))
                        } else {
                            t.arc_recovery_ticks = 0;
                            None
                        }
                    } else {
                        None
                    };

                    // Read arc_attacker before clearing arc metadata.
                    let attacker_id = self.state.combat.tactical[slot]
                        .arc_attacker
                        .unwrap_or(entity_id);

                    let t = &mut self.state.combat.tactical[slot];
                    t.arc_state = None;
                    t.arc_attacker = None;
                    audit!(
                        self.state,
                        Tactical,
                        Controller,
                        2,
                        Some(entity_id),
                        "arc_landed"
                    );

                    // Insert CC debuff for arc-recovery. Uses the original attacker
                    // (tracked through arc_attacker) as the debuff source.
                    if let Some((until, debuff_id)) = recovery {
                        if let Some(idx) = self.state.entities.lookup(entity_id) {
                            self.insert_cc_debuff(entity_id, idx, attacker_id, debuff_id, until);
                        }
                    }
                }
                audit!(
                    self.state,
                    Transform,
                    Controller,
                    2,
                    Some(entity_id),
                    "arc_move"
                );
            }
        }

        // Safety net: inject a gravity-only arc for any entity that reports
        // !is_grounded but has no active arc_state.  This can happen when the
        // support beneath an entity disappears (e.g., a prop is destroyed or —
        // before the KCC filter — an NPC capsule was removed after a CC-disabled
        // entity landed on it).  Without this the kinematic body would float
        // indefinitely since no system applies downward velocity.
        for (i, t) in self.state.combat.tactical.iter_mut().enumerate() {
            if !t.is_grounded && t.arc_state.is_none() {
                // Ensure the slot still maps to an active entity.
                if self.state.entities.lookup_by_slot(i).is_some() {
                    t.arc_state = Some(game_core::combat::tactical::ArcState {
                        velocity: Vec3f {
                            x: 0.0,
                            y: 0.0,
                            z: 0.0,
                        },
                        gravity: game_core::physics_constants::FALL_GRAVITY,
                        gravity_only: true,
                    });
                }
            }
        }
    }

    /// Drive forced movement for feared entities.
    ///
    /// Feared entities move away from `fear_source` at 50% of their base movement
    /// speed. This runs as a sub-step of Phase 2, before intent processing, so the
    /// entity's normal movement intents are rejected by `is_cc_disabled()`.
    fn drive_fear_movement(&mut self) {
        use game_core::combat::tactical::MovementConditions;
        let dt = self.dt;

        // Collect feared entities. Two-pass to avoid borrow conflicts.
        let feared_entities: Vec<(EntityId, usize)> = self
            .state
            .combat
            .tactical
            .iter()
            .enumerate()
            .filter(|(_, t)| t.movement_conditions.contains(MovementConditions::FEARED))
            .filter_map(|(i, _)| {
                let id = self.state.entities.lookup_by_slot(i)?;
                Some((id, i))
            })
            .collect();

        for (entity_id, slot) in feared_entities {
            let t = &self.state.combat.tactical[slot];
            let fear_source = match t.fear_source {
                Some(src) => src,
                None => continue,
            };

            // Get positions for direction calculation.
            let (target_pos, source_pos) = match (
                self.physics.get_transform(entity_id),
                self.physics.get_transform(fear_source),
            ) {
                (Some(tp), Some(sp)) => (tp.position, sp.position),
                _ => continue,
            };

            // Direction: away from fear source.
            let dx = target_pos.x - source_pos.x;
            let dz = target_pos.z - source_pos.z;
            let len = (dx * dx + dz * dz).sqrt();
            let (dir_x, dir_z) = if len > 1e-6 {
                (dx / len, dz / len)
            } else {
                (0.0, 1.0) // fallback: flee along +Z
            };

            // Move at 50% of the entity's base movement speed.
            let idx = match self.state.entities.lookup(entity_id) {
                Some(i) => i,
                None => continue,
            };
            let speed = self.state.stats.get(idx).movement_speed * 0.5;

            let ground_pull = game_core::physics_constants::GROUND_PULL;
            let desired = Vec3f {
                x: dir_x * speed * dt,
                y: -ground_pull * dt,
                z: dir_z * speed * dt,
            };

            if let Some(result) = self.physics.move_character(entity_id, desired) {
                self.state.combat.tactical[slot].is_grounded = result.grounded;
                audit!(
                    self.state,
                    Transform,
                    Controller,
                    2,
                    Some(entity_id),
                    "fear_move"
                );
            }
        }
    }

    /// Soft character-body repulsion: nudge overlapping character capsules apart.
    ///
    /// Runs once per tick after all movement, arcs, and fears have been applied.
    /// Single symmetric pass — each entity accumulates a push vector from all
    /// overlapping neighbours, then all pushes are applied together. Region-aware:
    /// uses each entity's current region to look up `RepulsionRules`, which
    /// determine which kind-pairs (player↔player, player↔NPC, etc.) repulse.
    ///
    /// Constants are tuned for capsule radius 0.3: two capsules overlap when
    /// their XZ distance < 0.6 units. Push is horizontal only — no vertical
    /// displacement — and capped per neighbour to feel like a nudge.
    fn apply_character_repulsion(&mut self) {
        const CHAR_RADIUS: f32 = game_core::physics_constants::CAPSULE_RADIUS;
        const PUSH_THRESHOLD: f32 = CHAR_RADIUS * 2.0;
        const PUSH_THRESHOLD_SQ: f32 = PUSH_THRESHOLD * PUSH_THRESHOLD;
        const MAX_PUSH_PER_TICK: f32 = 0.1;

        // Gather all character entities with their positions and kinds.
        // Uses entity_regions for rules lookup; entities without a region entry
        // default to OpenWorld rules (which only repulse player↔NPC).
        struct CharInfo {
            entity_id: EntityId,
            kind: EntityKind,
            x: f32,
            z: f32,
            layer: u32,
            rules: game_core::region::RepulsionRules,
        }

        let chars: Vec<CharInfo> = (0..self.state.entities.len())
            .filter_map(|i| {
                let idx = self.state.entities.index_at(i);
                if !self.state.entities.is_active(idx) {
                    return None;
                }
                let kind = self.state.entities.kinds[i];
                match kind {
                    EntityKind::Player | EntityKind::Npc | EntityKind::Boss => {}
                    _ => return None,
                }
                let id = self.state.entities.id_of(idx);
                let t = self.physics.get_transform(id)?;
                let rules = self.repulsion_rules_for(id);
                let layer = self.layer_of(id);
                Some(CharInfo {
                    entity_id: id,
                    kind,
                    x: t.position.x,
                    z: t.position.z,
                    layer,
                    rules,
                })
            })
            .collect();

        if chars.len() < 2 {
            return;
        }

        // Compute pushes with a temporary spatial grid so we only compare nearby
        // characters. A full pairwise pass becomes too expensive once NPC counts
        // reach the low thousands.
        fn cell_coord(value: f32, cell_size: f32) -> i32 {
            (value / cell_size).floor() as i32
        }

        fn accumulate_repulsion_pair(
            chars: &[CharInfo],
            pushes: &mut [(f32, f32)],
            i: usize,
            j: usize,
            push_threshold_sq: f32,
            push_threshold: f32,
            max_push_per_tick: f32,
        ) {
            let dx = chars[i].x - chars[j].x;
            let dz = chars[i].z - chars[j].z;
            let dist_sq = dx * dx + dz * dz;
            if dist_sq >= push_threshold_sq || dist_sq < 1e-8 {
                return;
            }

            // Layer isolation: no repulsion between entities on different layers.
            if chars[i].layer != chars[j].layer {
                return;
            }

            let dist = dist_sq.sqrt();
            let overlap = push_threshold - dist;
            let strength = (overlap * 0.5).min(max_push_per_tick);
            let nx = dx / dist;
            let nz = dz / dist;

            if chars[i].rules.should_repulse(chars[i].kind, chars[j].kind) {
                pushes[i].0 += nx * strength;
                pushes[i].1 += nz * strength;
            }
            if chars[j].rules.should_repulse(chars[j].kind, chars[i].kind) {
                pushes[j].0 -= nx * strength;
                pushes[j].1 -= nz * strength;
            }
        }

        let mut spatial_grid: std::collections::BTreeMap<(i32, i32), Vec<usize>> =
            std::collections::BTreeMap::new();
        for (index, ch) in chars.iter().enumerate() {
            let cell = (
                cell_coord(ch.x, PUSH_THRESHOLD),
                cell_coord(ch.z, PUSH_THRESHOLD),
            );
            spatial_grid.entry(cell).or_default().push(index);
        }

        // For each populated cell, compare pairs inside the cell plus the four
        // forward neighboring cells needed to cover all adjacent pairs exactly once.
        const NEIGHBOR_OFFSETS: [(i32, i32); 4] = [(1, -1), (1, 0), (1, 1), (0, 1)];

        let mut pushes: Vec<(f32, f32)> = vec![(0.0, 0.0); chars.len()];

        for (&cell, indices) in &spatial_grid {
            for a in 0..indices.len() {
                for b in (a + 1)..indices.len() {
                    accumulate_repulsion_pair(
                        &chars,
                        &mut pushes,
                        indices[a],
                        indices[b],
                        PUSH_THRESHOLD_SQ,
                        PUSH_THRESHOLD,
                        MAX_PUSH_PER_TICK,
                    );
                }
            }

            for (dx, dz) in NEIGHBOR_OFFSETS {
                let neighbor_cell = (cell.0 + dx, cell.1 + dz);
                let Some(neighbor_indices) = spatial_grid.get(&neighbor_cell) else {
                    continue;
                };

                for &i in indices {
                    for &j in neighbor_indices {
                        accumulate_repulsion_pair(
                            &chars,
                            &mut pushes,
                            i,
                            j,
                            PUSH_THRESHOLD_SQ,
                            PUSH_THRESHOLD,
                            MAX_PUSH_PER_TICK,
                        );
                    }
                }
            }
        }

        // Apply accumulated pushes via move_character so the KCC resolves
        // contacts against environment geometry (walls, floor). Using
        // set_kinematic_position here would bypass collision — pushing entities
        // into walls where they get stuck.
        for (ci, (px, pz)) in pushes.iter().enumerate() {
            if *px == 0.0 && *pz == 0.0 {
                continue;
            }
            let desired = Vec3f {
                x: *px,
                y: 0.0,
                z: *pz,
            };
            self.physics.move_character(chars[ci].entity_id, desired);
        }
    }

    /// Advance charge timers for all entities currently charging a hold-release ability.
    ///
    /// For each charging entity:
    /// 1. Compute elapsed ticks since `started_at`.
    /// 2. If a new tier threshold was crossed, emit `ChargeTierReached`.
    /// 3. If elapsed >= last tier's `min_ticks`, auto-release the ability at max tier.
    /// 4. Re-assert `rooted` so movement stays blocked while charging.
    fn drive_charging(&mut self) {
        // Cancel charges for CC-disabled entities before advancing timers.
        // A stunned/knocked-down/floating entity cannot continue charging.
        // Collect separately to avoid borrow conflicts with the main loop.
        let cc_cancelled: Vec<EntityId> = self
            .state
            .combat
            .charging
            .keys()
            .copied()
            .filter(|&eid| {
                self.state
                    .entities
                    .lookup(eid)
                    .is_some_and(|idx| self.is_cc_disabled(idx))
            })
            .collect();
        for entity_id in cc_cancelled {
            self.state.combat.charging.remove(&entity_id);
            if let Some(idx) = self.state.entities.lookup(entity_id) {
                self.state.combat.tactical[idx.as_usize()]
                    .movement_conditions
                    .remove(game_core::combat::tactical::MovementConditions::INPUT_LOCK);
            }
        }

        // Collect auto-releases to process after iteration (avoids borrow issues).
        let mut auto_releases: Vec<(EntityId, ChargingState, u8)> = Vec::new();
        // Collect tier-up events to emit.
        let mut tier_events: Vec<(EntityId, u32, u8)> = Vec::new();

        for (&entity_id, charging) in self.state.combat.charging.iter_mut() {
            let elapsed = self.current_tick.0.saturating_sub(charging.started_at.0) as u32;
            let tiers = match self
                .abilities
                .get(charging.ability_id)
                .and_then(|ad| ad.charge_tiers.as_ref())
            {
                Some(t) => t,
                None => continue,
            };

            // Determine current tier from elapsed ticks.
            let current_tier = tiers
                .iter()
                .rposition(|t| elapsed >= t.min_ticks)
                .unwrap_or(0) as u8;

            // Emit tier-up events for newly crossed thresholds.
            if current_tier > charging.notified_tier {
                for t in (charging.notified_tier + 1)..=current_tier {
                    tier_events.push((entity_id, charging.ability_id, t));
                }
                charging.notified_tier = current_tier;
            }

            // Check auto-release: last tier reached.
            let max_tier = (tiers.len() - 1) as u8;
            if current_tier >= max_tier && elapsed >= tiers.last().unwrap().min_ticks {
                auto_releases.push((entity_id, charging.clone(), max_tier));
            }

            // Re-assert INPUT_LOCK flag each tick while charging if enabled.
            let charge_roots = self
                .abilities
                .get(charging.ability_id)
                .map(|ad| ad.charge_roots_while_charging)
                .unwrap_or(true);
            if let Some(idx) = self.state.entities.lookup(entity_id) {
                let t = &mut self.state.combat.tactical[idx.as_usize()];
                if charge_roots {
                    t.movement_conditions
                        .insert(game_core::combat::tactical::MovementConditions::INPUT_LOCK);
                } else {
                    t.movement_conditions
                        .remove(game_core::combat::tactical::MovementConditions::INPUT_LOCK);
                }
            }
        }

        // Emit tier events.
        for (entity_id, ability_id, tier) in tier_events {
            self.emit_event(
                entity_id,
                EventPayload::ChargeTierReached { ability_id, tier },
            );
        }

        // Process auto-releases.
        for (entity_id, charging, tier) in auto_releases {
            self.state.combat.charging.remove(&entity_id);
            // Clear charge-driven INPUT_LOCK flag.
            if let Some(idx) = self.state.entities.lookup(entity_id) {
                let t = &mut self.state.combat.tactical[idx.as_usize()];
                t.movement_conditions
                    .remove(game_core::combat::tactical::MovementConditions::INPUT_LOCK);
            }
            if self.cast_ability(
                entity_id,
                charging.ability_id,
                charging.targeting,
                charging.rewind_ticks,
                tier,
            ) {
                audit!(
                    self.state,
                    Execution,
                    Controller,
                    2,
                    Some(entity_id),
                    "charge_auto_release"
                );
            }
        }
    }

    fn handle_face_to(&mut self, entity_id: EntityId, dir: &game_schema::MoveDir) {
        self.set_entity_body_facing(
            entity_id,
            Vec3f {
                x: dir.dir_x,
                y: dir.dir_y,
                z: dir.dir_z,
            },
            "face_to",
        );
    }

    fn handle_block(&mut self, entity_id: EntityId, data: &game_schema::BlockData) {
        self.set_entity_body_facing(
            entity_id,
            Vec3f {
                x: data.look_dir.dir_x,
                y: data.look_dir.dir_y,
                z: data.look_dir.dir_z,
            },
            "block_face",
        );
        if let Some(idx) = self.state.entities.lookup(entity_id) {
            let is_new_block = self.state.combat.tactical[idx.as_usize()]
                .block_start_tick
                .is_none();
            let t = &mut self.state.combat.tactical[idx.as_usize()];
            t.blocking = true;
            t.block_grace = false;
            if is_new_block {
                t.block_start_tick = Some(self.current_tick);
                self.emit_event(entity_id, EventPayload::BlockStart);
            }
            audit!(
                self.state,
                Tactical,
                Controller,
                2,
                Some(entity_id),
                "block"
            );
        }
    }

    fn handle_interact(&mut self, entity_id: EntityId, target_id_raw: u64) {
        use game_core::sim_state::{SimInteractKind, SimInteractState};
        let target = EntityId(target_id_raw);
        // Layer isolation: can only interact with entities on the same layer.
        if !self.same_layer(entity_id, target) {
            return;
        }
        let info = self.state.interactables.get(&target).cloned();
        if let (Some(actor_t), Some(target_t)) = (
            self.physics.get_transform(entity_id),
            self.physics.get_transform(target),
        ) {
            let dx = target_t.position.x - actor_t.position.x;
            let dz = target_t.position.z - actor_t.position.z;
            let dist_sq = dx * dx + dz * dz;

            let max_range = info
                .as_ref()
                .map(|i| i.interact_range)
                .unwrap_or(game_core::physics_constants::INTERACT_RADIUS);
            if dist_sq > max_range * max_range {
                return;
            }

            if let Some(info) = &info {
                let Some(actor_idx) = self.state.entities.lookup(entity_id) else {
                    return;
                };
                if let Some(required_buff) = info.required_buff {
                    let has_buff = self
                        .state
                        .status
                        .get_buffs(actor_idx)
                        .iter()
                        .any(|buff| buff.buff_id == required_buff);
                    if !has_buff {
                        return;
                    }
                }
                if let Some(required_item) = info.required_item
                    && !self.entity_has_item(entity_id, required_item)
                {
                    return;
                }
            }

            self.set_entity_body_facing(
                entity_id,
                Vec3f {
                    x: dx,
                    y: 0.0,
                    z: dz,
                },
                "interact_face",
            );
            self.emit_event(entity_id, EventPayload::InteractTriggered { target });

            // Branch by interactable kind for state mutations.
            let info = match info {
                Some(info) => info,
                None => return, // Not an interactable — event-only interaction
            };

            match info.kind {
                SimInteractKind::Switch => {
                    if info.puzzle_group.is_some() && info.puzzle_window_ticks > 0 {
                        self.activate_timed_lever(target, &info);
                    } else {
                        // Toggle: Idle ↔ Active
                        let new_state = match info.state {
                            SimInteractState::Idle => SimInteractState::Active,
                            SimInteractState::Active => SimInteractState::Idle,
                            SimInteractState::Cooldown => return, // Cannot interact during cooldown
                        };
                        self.set_interactable_state_runtime(target, new_state);
                    }
                }
                SimInteractKind::Chest => {
                    if info.state != SimInteractState::Idle {
                        return;
                    }
                    // Mark chest as Active (opened). Future: emit loot event.
                    self.set_interactable_state_runtime(target, SimInteractState::Active);
                }
                SimInteractKind::Gate | SimInteractKind::Grab => {
                    // Gates are not directly interactable (controlled by switches).
                    // Grab: future implementation.
                }
            }
        }
    }

    fn expire_timed_lever_puzzles(&mut self) {
        use game_core::sim_state::SimInteractState;

        let expired: Vec<String> = self
            .lever_puzzles
            .iter()
            .filter(|(_, runtime)| runtime.expires_at <= self.current_tick)
            .map(|(group, _)| group.clone())
            .collect();
        for group in expired {
            if let Some(runtime) = self.lever_puzzles.remove(&group) {
                for lever in runtime.activated {
                    self.set_interactable_own_state_runtime(lever, SimInteractState::Idle);
                }
            }
        }
    }

    fn activate_timed_lever(
        &mut self,
        lever: EntityId,
        info: &game_core::sim_state::InteractableInfo,
    ) {
        use game_core::sim_state::SimInteractState;

        if info.state != SimInteractState::Idle {
            return;
        }
        let Some(group) = info.puzzle_group.clone() else {
            return;
        };
        let required_count = if info.puzzle_required_count > 0 {
            info.puzzle_required_count as usize
        } else {
            self.state
                .interactables
                .values()
                .filter(|other| other.puzzle_group.as_deref() == Some(group.as_str()))
                .count()
        };
        if required_count == 0 {
            return;
        }

        self.set_interactable_own_state_runtime(lever, SimInteractState::Active);
        let expires_at = TickId(
            self.current_tick
                .0
                .saturating_add(info.puzzle_window_ticks as u64),
        );
        let complete = {
            let runtime =
                self.lever_puzzles
                    .entry(group.clone())
                    .or_insert_with(|| LeverPuzzleRuntime {
                        activated: BTreeSet::new(),
                        expires_at,
                    });
            runtime.activated.insert(lever);
            runtime.activated.len() >= required_count
        };

        if complete {
            let activated = self
                .lever_puzzles
                .remove(&group)
                .map(|runtime| runtime.activated)
                .unwrap_or_default();
            for lever_id in activated {
                let linked_gate = self
                    .state
                    .interactables
                    .get(&lever_id)
                    .and_then(|lever_info| lever_info.linked_entity);
                if let Some(gate_id) = linked_gate {
                    self.set_interactable_state_runtime(gate_id, SimInteractState::Active);
                }
            }
        }
    }

    fn handle_jump(&mut self, entity_id: EntityId) {
        let Some(idx) = self.state.entities.lookup(entity_id) else {
            return;
        };
        let t_read = &self.state.combat.tactical[idx.as_usize()];
        if !t_read.is_grounded || t_read.arc_state.is_some() {
            return;
        }
        if self.is_rooted(idx) {
            return;
        }
        let t = &mut self.state.combat.tactical[idx.as_usize()];
        t.arc_state = Some(game_core::combat::tactical::ArcState {
            velocity: Vec3f {
                x: 0.0,
                y: game_core::physics_constants::JUMP_SPEED,
                z: 0.0,
            },
            gravity: game_core::physics_constants::JUMP_GRAVITY,
            gravity_only: true,
        });
        t.is_grounded = false;
        self.emit_event(entity_id, EventPayload::Jumped);
        audit!(self.state, Tactical, Controller, 2, Some(entity_id), "jump");
    }

    fn handle_weapon_swap(&mut self, entity_id: EntityId) {
        let Some(idx) = self.state.entities.lookup(entity_id) else {
            return;
        };
        // Reject if mid-cast (any active execution for this entity).
        let has_active_cast = self
            .state
            .combat
            .executions
            .active_ids()
            .iter()
            .any(|&eid| {
                self.state
                    .combat
                    .executions
                    .get(eid)
                    .map_or(false, |ctx| ctx.caster == entity_id)
            });
        if has_active_cast {
            return;
        }
        // Reject if currently charging.
        if self.state.combat.charging.contains_key(&entity_id) {
            return;
        }
        // Reject if on weapon swap cooldown.
        if self
            .weapon_swap_cooldowns
            .get(&entity_id)
            .is_some_and(|&ready_at| self.current_tick < ready_at)
        {
            return;
        }
        // Only entities with a loadout can swap.
        let Some(loadout) = self.state.combat.loadouts.get_mut(idx) else {
            return;
        };
        let new_set = loadout.swap();
        // Clear combo windows — weapon swap resets combos.
        self.state
            .combat
            .active_windows
            .retain(|&(eid, _), _| eid != entity_id);
        // Set swap cooldown.
        let cd = game_core::physics_constants::WEAPON_SWAP_COOLDOWN_TICKS;
        self.weapon_swap_cooldowns
            .insert(entity_id, TickId(self.current_tick.0 + cd as u64));
        self.emit_event(entity_id, EventPayload::WeaponSwapped { new_set });
        audit!(
            self.state,
            Tactical,
            Controller,
            2,
            Some(entity_id),
            "weapon_swap"
        );
    }

    fn handle_use_ability(
        &mut self,
        intent: &PlayerIntent,
        cast_this_tick: &mut HashSet<(EntityId, u32)>,
    ) {
        let entity_id = intent.entity_id;
        let data = match &intent.action {
            IntentAction::UseAbility(d) => d,
            _ => return,
        };
        let ability_id = data.ability_id;
        let cast_key = (entity_id, ability_id);
        if cast_this_tick.contains(&cast_key) {
            return;
        }
        if self.state.combat.charging.contains_key(&entity_id) {
            return;
        }

        // Weapon-set gate: if the entity has a loadout, reject abilities not
        // in the active weapon set. Entities without a loadout are unrestricted.
        if let Some(idx) = self.state.entities.lookup(entity_id) {
            if let Some(loadout) = self.state.combat.loadouts.get(idx) {
                if !loadout.is_ability_available(ability_id) {
                    return;
                }
            }
        }

        // Resolve targeting mode from ability data.
        let targeting_mode = self.ability_prop(
            ability_id,
            |ad| ad.targeting_mode,
            TargetingMode::DirectionTarget,
        );
        let max_range = self
            .ability_prop(ability_id, |ad| ad.max_range, None)
            .unwrap_or(game_core::physics_constants::DEFAULT_ABILITY_MAX_RANGE);

        // Validate and resolve targeting based on the ability's TargetingMode.
        let targeting = match targeting_mode {
            TargetingMode::DirectionTarget => {
                let body_facing = self
                    .physics
                    .get_transform(entity_id)
                    .map(|t| Self::forward_from_rotation(t.rotation))
                    .and_then(Self::normalize_direction)
                    .unwrap_or(Vec3f {
                        x: 0.0,
                        y: 0.0,
                        z: 1.0,
                    });
                match &data.target {
                    game_schema::AbilityTarget::Direction(d) => {
                        let dir = Vec3f {
                            x: d.x,
                            y: d.y,
                            z: d.z,
                        };
                        let Some(dir) = Self::normalize_direction(dir) else {
                            return;
                        };
                        ResolvedTargeting::Direction { dir }
                    }
                    game_schema::AbilityTarget::None => {
                        ResolvedTargeting::Direction { dir: body_facing }
                    }
                    _ => return,
                }
            }
            TargetingMode::EntityTarget => {
                let target = match &data.target {
                    game_schema::AbilityTarget::Entity(id) => EntityId(*id),
                    _ => return,
                };
                if target == entity_id {
                    return;
                }
                if !self.same_layer(entity_id, target) {
                    return;
                }
                let Some(target_idx) = self.state.entities.lookup(target) else {
                    return;
                };
                if !self.state.entities.is_active(target_idx) {
                    return;
                }
                let (Some(caster_t), Some(target_t)) = (
                    self.physics.get_transform(entity_id),
                    self.physics.get_transform(target),
                ) else {
                    return;
                };
                let dx = target_t.position.x - caster_t.position.x;
                let dy = target_t.position.y - caster_t.position.y;
                let dz = target_t.position.z - caster_t.position.z;
                if dx * dx + dy * dy + dz * dz > max_range * max_range {
                    return;
                }
                if !self.physics.line_of_sight_on_layer(
                    caster_t.position,
                    target_t.position,
                    self.layer_of(entity_id),
                ) {
                    return;
                }
                ResolvedTargeting::Entity { target }
            }
            TargetingMode::GroundTarget => {
                // Client must send Position. Validate within max_range of caster
                // and that a clear line-of-sight exists to the target point.
                let point = match &data.target {
                    game_schema::AbilityTarget::Position(p) => *p,
                    _ => return, // reject non-position targeting for ground-target abilities
                };
                let caster_pos = self
                    .physics
                    .get_transform(entity_id)
                    .map(|t| t.position)
                    .unwrap_or(Vec3f::ZERO);
                // Horizontal (XZ) range is intentional for GroundTarget: the
                // ability's reach is "how far across the ground I can place
                // an AoE", independent of the caster's Y (standing on a
                // ledge should not shrink placement range). Vertical
                // placement abuse (targeting a cliff or platform far above)
                // is blocked by the 3D LoS check below, not by range.
                // Entity/direction modes continue to use full 3D distance.
                let dx = point.x - caster_pos.x;
                let dz = point.z - caster_pos.z;
                if dx * dx + dz * dz > max_range * max_range {
                    return; // out of horizontal range
                }
                // Resolve authoritative ground Y by raycasting straight down
                // from high above the client's (x, z). The client's Y is
                // advisory only — on uneven terrain it would be stale or
                // spoofed. If the ray misses (hole in terrain, off the
                // heightfield, or — with strict layer filtering — no
                // environment for this caster's layer at that XZ), reject
                // the cast: trusting the client-supplied Y here would let
                // a spoofed client place AoEs at arbitrary heights, and
                // the subsequent LoS check only catches blockers, not
                // empty sky above the caster.
                const SKY_LIFT: f32 = 200.0;
                const MAX_DROP: f32 = 400.0;
                let layer = self.layer_of(entity_id);
                let Some(resolved_point) = self.physics.raycast_surface(
                    Vec3f {
                        x: point.x,
                        y: point.y + SKY_LIFT,
                        z: point.z,
                    },
                    Vec3f {
                        x: 0.0,
                        y: -1.0,
                        z: 0.0,
                    },
                    MAX_DROP,
                    layer,
                ) else {
                    return; // no terrain at requested XZ on this layer
                };
                if !self
                    .physics
                    .line_of_sight_on_layer(caster_pos, resolved_point, layer)
                {
                    return; // blocked by environment
                }
                ResolvedTargeting::Position {
                    point: resolved_point,
                }
            }
            TargetingMode::RaycastStrict => {
                // Server raycasts from caster along client aim direction.
                // Only Direction is accepted — entity/position targets are rejected so the
                // client cannot fake a hit by supplying a tab-target entity ID.
                let caster_pos = self
                    .physics
                    .get_transform(entity_id)
                    .map(|t| t.position)
                    .unwrap_or(Vec3f::ZERO);

                let d = match &data.target {
                    game_schema::AbilityTarget::Direction(d) => d,
                    _ => return, // RaycastStrict requires Direction; reject Entity/Position/None
                };
                let dir = Vec3f {
                    x: d.x,
                    y: d.y,
                    z: d.z,
                };
                let len_sq = dir.x * dir.x + dir.y * dir.y + dir.z * dir.z;
                if !len_sq.is_finite() || len_sq < 1e-6 {
                    return; // degenerate direction
                }
                if let Some(hit) = self
                    .physics
                    .raycast(caster_pos, dir, max_range, Some(entity_id))
                {
                    if matches!(hit.kind, ColliderKind::Hurtbox) {
                        ResolvedTargeting::Entity { target: hit.entity }
                    } else {
                        return; // hit environment or non-hurtbox — whiff
                    }
                } else {
                    return; // ray missed — whiff
                }
            }
            TargetingMode::AimAssist => {
                // Soft-lock: raycast first, then cone fallback if ray misses.
                // target_hint narrows the cone for more precise lock-on.
                let caster_pos = self
                    .physics
                    .get_transform(entity_id)
                    .map(|t| t.position)
                    .unwrap_or(Vec3f::ZERO);

                match &data.target {
                    game_schema::AbilityTarget::Entity(id) => {
                        // Tab-lock path: validate target is within max_range.
                        let target = EntityId(*id);
                        if !self.same_layer(entity_id, target) {
                            return;
                        }
                        if let Some(t) = self.physics.get_transform(target) {
                            // 3D range check — keeps parity with EntityTarget
                            // and GroundTarget branches; avoids silently
                            // accepting stacked-Y exploits on uneven terrain.
                            let dx = t.position.x - caster_pos.x;
                            let dy = t.position.y - caster_pos.y;
                            let dz = t.position.z - caster_pos.z;
                            if dx * dx + dy * dy + dz * dz > max_range * max_range {
                                return;
                            }
                            ResolvedTargeting::Entity { target }
                        } else {
                            return;
                        }
                    }
                    game_schema::AbilityTarget::Direction(d) => {
                        let dir = Vec3f {
                            x: d.x,
                            y: d.y,
                            z: d.z,
                        };
                        let len_sq = dir.x * dir.x + dir.y * dir.y + dir.z * dir.z;
                        if !len_sq.is_finite() || len_sq < 1e-6 {
                            return; // degenerate direction
                        }
                        let inv_len = 1.0 / len_sq.sqrt();
                        let aim_dir = Vec3f {
                            x: dir.x * inv_len,
                            y: dir.y * inv_len,
                            z: dir.z * inv_len,
                        };

                        // 1. Try precise raycast first.
                        if let Some(hit) =
                            self.physics
                                .raycast(caster_pos, dir, max_range, Some(entity_id))
                        {
                            if matches!(hit.kind, ColliderKind::Hurtbox) {
                                // Direct hit — prefer target_hint if it matches,
                                // otherwise use the raycast result.
                                ResolvedTargeting::Entity { target: hit.entity }
                            } else {
                                // Hit environment — fall through to cone check.
                                self.aim_assist_cone(
                                    entity_id,
                                    caster_pos,
                                    aim_dir,
                                    max_range,
                                    data.target_hint,
                                )
                            }
                        } else {
                            // Ray missed — fall through to cone check.
                            self.aim_assist_cone(
                                entity_id,
                                caster_pos,
                                aim_dir,
                                max_range,
                                data.target_hint,
                            )
                        }
                    }
                    game_schema::AbilityTarget::None => ResolvedTargeting::SelfCast,
                    _ => return, // AimAssist expects Direction, Entity, or None
                }
            }
            TargetingMode::LockOn { max_targets } => {
                // Two-phase activation (TERA-style lock-on):
                //   Phase 1 — no existing session: open a new tagging session and return.
                //             The client then sends TagTarget intents to accumulate targets.
                //             No cooldown is consumed yet.
                //   Phase 2 — session already open for this ability: fire with accumulated
                //             targets and consume the cooldown even if the list is empty.
                //
                // ReleaseAbility cancels an open session without consuming cooldown.
                if self.is_on_cooldown(entity_id, ability_id) {
                    return;
                }

                let rewind_ticks = lag_compensation::compute_rewind_ticks(
                    self.current_tick,
                    intent.client_observed_tick,
                    self.global_max_rewind_ticks,
                );

                if let Some(session) = self.active_lock_on_sessions.remove(&entity_id) {
                    if session.ability_id == ability_id {
                        // Phase 2: fire with whatever targets were tagged.
                        let targets = session.tagged.clone();
                        let resolved = ResolvedTargeting::MultiLockOn { targets };
                        if self.cast_ability(entity_id, ability_id, resolved, rewind_ticks, 0) {
                            cast_this_tick.insert(cast_key);
                        }
                    } else {
                        // A different ability's session is open — cancel it, then open
                        // a fresh session for the new ability.
                        for target in &session.tagged {
                            self.emit_event(
                                entity_id,
                                EventPayload::LockOnCanceled {
                                    source: entity_id,
                                    target: *target,
                                },
                            );
                        }
                        let timeout = self
                            .ability_prop(ability_id, |ad| ad.lock_on_timeout_ticks, None)
                            .unwrap_or(LOCK_ON_SESSION_TIMEOUT_TICKS);
                        let timeout_at = TickId(self.current_tick.0 + timeout as u64);
                        self.active_lock_on_sessions.insert(
                            entity_id,
                            LockOnSession {
                                ability_id,
                                tagged: Vec::new(),
                                max_targets,
                                timeout_at,
                            },
                        );
                        self.emit_event(
                            entity_id,
                            EventPayload::LockOnSessionStarted {
                                source: entity_id,
                                ability_id,
                            },
                        );
                    }
                } else {
                    // Phase 1: open a new session.
                    let timeout = self
                        .ability_prop(ability_id, |ad| ad.lock_on_timeout_ticks, None)
                        .unwrap_or(LOCK_ON_SESSION_TIMEOUT_TICKS);
                    let timeout_at = TickId(self.current_tick.0 + timeout as u64);
                    self.active_lock_on_sessions.insert(
                        entity_id,
                        LockOnSession {
                            ability_id,
                            tagged: Vec::new(),
                            max_targets,
                            timeout_at,
                        },
                    );
                    self.emit_event(
                        entity_id,
                        EventPayload::LockOnSessionStarted {
                            source: entity_id,
                            ability_id,
                        },
                    );
                }
                return; // LockOn manages its own cast lifecycle — skip the normal cast path.
            }
            TargetingMode::SelfOnly => {
                // Reject any external target — force self-cast.
                ResolvedTargeting::SelfCast
            }
            TargetingMode::CasterOffset => {
                if !matches!(data.target, game_schema::AbilityTarget::None) {
                    return;
                }
                ResolvedTargeting::CasterOffset
            }
        };

        let rewind_ticks = lag_compensation::compute_rewind_ticks(
            self.current_tick,
            intent.client_observed_tick,
            self.global_max_rewind_ticks,
        );

        let is_chargeable = self
            .abilities
            .get(ability_id)
            .and_then(|ad| ad.charge_tiers.as_ref())
            .is_some_and(|tiers| tiers.len() >= 2);

        if is_chargeable {
            if self.is_on_cooldown(entity_id, ability_id) {
                return;
            }
            let max_ticks = self
                .abilities
                .get(ability_id)
                .and_then(|ad| ad.charge_tiers.as_ref())
                .and_then(|tiers| tiers.last())
                .map(|t| t.min_ticks)
                .unwrap_or(1);
            let charge_roots =
                self.ability_prop(ability_id, |ad| ad.charge_roots_while_charging, true);
            self.state.combat.charging.insert(
                entity_id,
                ChargingState {
                    ability_id,
                    started_at: self.current_tick,
                    targeting,
                    rewind_ticks,
                    notified_tier: 0,
                },
            );
            if charge_roots {
                if let Some(t) = self.tactical_mut(entity_id) {
                    t.movement_conditions
                        .insert(game_core::combat::tactical::MovementConditions::INPUT_LOCK);
                }
            }
            self.emit_event(
                entity_id,
                EventPayload::ChargeStart {
                    ability_id,
                    max_ticks,
                },
            );
            audit!(
                self.state,
                Execution,
                Controller,
                2,
                Some(entity_id),
                "charge_start"
            );
        } else {
            if self.cast_ability(entity_id, ability_id, targeting, rewind_ticks, 0) {
                audit!(
                    self.state,
                    Execution,
                    Controller,
                    2,
                    Some(entity_id),
                    "cast"
                );
                cast_this_tick.insert(cast_key);
            }
        }
    }

    /// Soft-lock cone fallback for AimAssist targeting mode.
    ///
    /// Iterates all entities in range, finds candidates within a cone around the
    /// aim direction, and picks the closest one. If `target_hint` matches a
    /// candidate, that entity is preferred (with a tighter cone).
    fn aim_assist_cone(
        &self,
        caster: EntityId,
        caster_pos: Vec3f,
        aim_dir: Vec3f,
        max_range: f32,
        target_hint: Option<u64>,
    ) -> ResolvedTargeting {
        let range_sq = max_range * max_range;
        let dot_threshold = if target_hint.is_some() {
            game_core::physics_constants::AIM_ASSIST_DOT_WITH_HINT
        } else {
            game_core::physics_constants::AIM_ASSIST_DOT_NO_HINT
        };

        let mut best: Option<(EntityId, f32)> = None; // (entity, dist_sq)
        let caster_layer = self.layer_of(caster);

        for (eid, transform) in self.physics.get_all_transforms() {
            if eid == caster {
                continue;
            }
            // Layer isolation: aim-assist only considers entities on the same layer.
            if self.layer_of(eid) != caster_layer {
                continue;
            }
            let dx = transform.position.x - caster_pos.x;
            let dy = transform.position.y - caster_pos.y;
            let dz = transform.position.z - caster_pos.z;
            let dist_sq = dx * dx + dy * dy + dz * dz;
            if dist_sq > range_sq || dist_sq < 1e-6 {
                continue;
            }

            // Check if entity is within the cone.
            let inv_dist = 1.0 / dist_sq.sqrt();
            let to_target = Vec3f {
                x: dx * inv_dist,
                y: dy * inv_dist,
                z: dz * inv_dist,
            };
            let dot = aim_dir.x * to_target.x + aim_dir.y * to_target.y + aim_dir.z * to_target.z;
            if dot < dot_threshold {
                continue;
            }

            // Prefer target_hint if it's a valid candidate.
            if target_hint == Some(eid.0) {
                return ResolvedTargeting::Entity { target: eid };
            }

            // Track closest candidate.
            if best.map_or(true, |(_, bd)| dist_sq < bd) {
                best = Some((eid, dist_sq));
            }
        }

        if let Some((target, _)) = best {
            ResolvedTargeting::Entity { target }
        } else {
            // No candidate found — resolve as direction (fire-and-forget).
            ResolvedTargeting::Direction { dir: aim_dir }
        }
    }

    fn handle_release_ability(
        &mut self,
        entity_id: EntityId,
        ability_id: u32,
        cast_this_tick: &mut HashSet<(EntityId, u32)>,
    ) {
        // Cancel any open lock-on session for this ability without consuming cooldown.
        if let Some(session) = self.active_lock_on_sessions.get(&entity_id) {
            if session.ability_id == ability_id {
                let tagged = session.tagged.clone();
                self.active_lock_on_sessions.remove(&entity_id);
                for target in &tagged {
                    self.emit_event(
                        entity_id,
                        EventPayload::LockOnCanceled {
                            source: entity_id,
                            target: *target,
                        },
                    );
                }
                return;
            }
        }
        if let Some(charging) = self.state.combat.charging.remove(&entity_id) {
            if charging.ability_id == ability_id {
                let elapsed = self.current_tick.0.saturating_sub(charging.started_at.0) as u32;
                let tier = self.resolve_charge_tier(ability_id, elapsed);
                if let Some(t) = self.tactical_mut(entity_id) {
                    t.movement_conditions
                        .remove(game_core::combat::tactical::MovementConditions::INPUT_LOCK);
                }
                if self.cast_ability(
                    entity_id,
                    ability_id,
                    charging.targeting,
                    charging.rewind_ticks,
                    tier,
                ) {
                    audit!(
                        self.state,
                        Execution,
                        Controller,
                        2,
                        Some(entity_id),
                        "charge_release"
                    );
                    cast_this_tick.insert((entity_id, ability_id));
                }
            } else {
                self.state.combat.charging.insert(entity_id, charging);
            }
        }
    }

    /// Validate and record a `TagTarget` intent for the caster's active lock-on session.
    ///
    /// Called for `IntentAction::TagTarget` during Phase 2. No-ops silently on any
    /// validation failure (no session, already tagged, out of range, no LoS, max reached).
    fn handle_tag_target(&mut self, caster: EntityId, target_id_raw: u64) {
        let target = EntityId(target_id_raw);
        if !self.same_layer(caster, target) {
            return;
        }
        // Target must be active (not despawning).
        if !self
            .state
            .entities
            .lookup(target)
            .is_some_and(|idx| self.state.entities.is_active(idx))
        {
            return;
        }
        // Extract session metadata without holding a borrow on the session.
        let (ability_id, is_full, already_tagged) =
            if let Some(session) = self.active_lock_on_sessions.get(&caster) {
                let is_full = session.tagged.len() >= session.max_targets as usize;
                let already_tagged = session.tagged.contains(&target);
                (session.ability_id, is_full, already_tagged)
            } else {
                return; // no active session
            };
        if already_tagged || is_full {
            return;
        }
        // Target must exist in physics.
        let Some(target_pos) = self.physics.get_transform(target).map(|t| t.position) else {
            return;
        };
        let Some(caster_pos) = self.physics.get_transform(caster).map(|t| t.position) else {
            return;
        };
        // 3-D range check.
        let max_range = self
            .ability_prop(ability_id, |ad| ad.max_range, None)
            .unwrap_or(game_core::physics_constants::DEFAULT_ABILITY_MAX_RANGE);
        let dx = target_pos.x - caster_pos.x;
        let dy = target_pos.y - caster_pos.y;
        let dz = target_pos.z - caster_pos.z;
        if dx * dx + dy * dy + dz * dz > max_range * max_range {
            return;
        }
        // Environment line-of-sight check.
        if !self
            .physics
            .line_of_sight_on_layer(caster_pos, target_pos, self.layer_of(caster))
        {
            return;
        }
        // All checks passed — record the tag.
        if let Some(session) = self.active_lock_on_sessions.get_mut(&caster) {
            session.tagged.push(target);
        }
        self.emit_event(
            caster,
            EventPayload::LockOnWarning {
                source: caster,
                target,
            },
        );
    }

    /// If the entity is airborne (`!is_grounded`) and has no active arc, inject a
    /// gravity-only `ArcState` so it falls naturally during a root or input-lock window.
    ///
    /// Called whenever `movement_conditions.insert(ROOTED)` or `rooted_until_tick` is set.
    /// `is_grounded` is maintained by `apply_movement` and `drive_arc_movement`.
    pub(super) fn apply_gravity_if_airborne(&mut self, entity: EntityId) {
        let Some(idx) = self.state.entities.lookup(entity) else {
            return;
        };
        let t = &self.state.combat.tactical[idx.as_usize()];
        // Already falling (deliberate arc) or confirmed on the ground — nothing to do.
        if t.is_grounded || t.arc_state.is_some() {
            return;
        }
        let t = &mut self.state.combat.tactical[idx.as_usize()];
        t.arc_state = Some(game_core::combat::tactical::ArcState {
            velocity: Vec3f {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            gravity: game_core::physics_constants::FALL_GRAVITY,
            gravity_only: true,
        });
    }

    /// Resolve the highest charge tier achieved for a given elapsed tick count.
    /// Returns the tier index (0-based). If no tiers are defined, returns 0.
    pub(super) fn resolve_charge_tier(&self, ability_id: u32, elapsed_ticks: u32) -> u8 {
        self.abilities
            .get(ability_id)
            .and_then(|ad| ad.charge_tiers.as_ref())
            .map(|tiers| {
                tiers
                    .iter()
                    .rposition(|t| elapsed_ticks >= t.min_ticks)
                    .unwrap_or(0) as u8
            })
            .unwrap_or(0)
    }
}
