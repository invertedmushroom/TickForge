use super::*;

impl TickPipeline {
    // ── Phase 3: Skill scheduling ───────────────────────────────

    pub(super) fn phase_skill_scheduling(&mut self) {
        // Drain all scheduled actions due this tick (queue is sorted by tick_id).
        let current = self.current_tick;
        let split_idx = self
            .scheduled_actions
            .partition_point(|a| a.tick_id <= current);
        let mut due_actions = std::mem::take(&mut self.scheduled_action_scratch);
        due_actions.clear();
        due_actions.extend(self.scheduled_actions.drain(..split_idx));

        for scheduled in due_actions.drain(..) {
            let entity = scheduled.entity;
            log::debug!(
                "dispatch sched={} tick={} entity={} source={:?} action={}",
                scheduled.id,
                scheduled.tick_id.0,
                entity.0,
                scheduled.source,
                match &scheduled.action_type {
                    ScheduledActionType::AbilityFrame { action, .. } => action.label(),
                    ScheduledActionType::ContactSpawnHitbox { .. } => "ContactSpawnHitbox",
                    ScheduledActionType::BuffExpire { .. } => "BuffExpire",
                },
            );
            match scheduled.action_type {
                ScheduledActionType::AbilityFrame {
                    execution_id,
                    ability_id,
                    ref action,
                } => {
                    self.execute_ability_action(entity, ability_id, execution_id, action);
                }
                ScheduledActionType::ContactSpawnHitbox(payload) => {
                    let ContactSpawnHitboxPayload {
                        parent_execution_id,
                        ability_id,
                        shape,
                        position,
                        offset,
                        effect,
                        rules,
                        duration_ticks,
                    } = *payload;
                    self.spawn_contact_hitbox(
                        entity,
                        parent_execution_id,
                        ability_id,
                        shape,
                        position,
                        offset,
                        effect,
                        rules,
                        duration_ticks,
                    );
                }
                ScheduledActionType::BuffExpire { buff_id } => {
                    self.emit_event(entity, EventPayload::BuffExpired { buff_id });
                }
            }
        }
        self.scheduled_action_scratch = due_actions;

        // Cull execution contexts for casts that no longer own any runtime effect.
        //
        // This covers abilities whose timeline has no RemoveHitbox (pure cooldown,
        // buff-apply) — without this pass their AbilityExecutionContext would leak
        // for the lifetime of the session.
        //
        // The `execution_is_alive` predicate defines what "still owning an effect"
        // means.  Extend that method when new effect types (projectiles, buffs) are
        // introduced — no changes here are needed.
        let mut scheduled_sources = std::mem::take(&mut self.scheduled_sources_scratch);
        scheduled_sources.clear();
        scheduled_sources.extend(self.scheduled_actions.iter().filter_map(|a| a.source));

        let mut dead = std::mem::take(&mut self.execution_cull_scratch);
        dead.clear();
        dead.extend(
            self.state
                .combat
                .executions
                .active_ids_iter()
                .filter(|&id| {
                    !Self::execution_is_alive(id, &scheduled_sources, &self.state.combat.hitboxes)
                }),
        );

        for id in dead.drain(..) {
            if let Some(_ctx) = self.state.combat.executions.get(id) {
                audit!(
                    self.state,
                    Execution,
                    AbilityTimeline,
                    3,
                    Some(_ctx.caster),
                    "cull"
                );
            }
            self.state.combat.executions.remove(id);
        }
        scheduled_sources.clear();
        self.scheduled_sources_scratch = scheduled_sources;
        self.execution_cull_scratch = dead;
    }

    fn spawn_logical_hitbox(
        &mut self,
        entity: EntityId,
        ability_id: u32,
        execution_id: AbilityExecutionId,
        shape: SkillShape,
        offset: Vec3f,
        effect_override: Option<HitEffectSpec>,
        rules_override: Option<HitboxRules>,
    ) {
        let Some(ability) = self.abilities.get(ability_id) else {
            return;
        };
        // Look up rewind_ticks from the execution context for this cast.
        let rewind_ticks = self
            .state
            .combat
            .executions
            .get(execution_id)
            .map(|ctx| ctx.rewind_ticks)
            .unwrap_or(0);
        // Preserve the no-override path: store `None` so combat.rs's hit
        // resolution fast-path activates and reads scalar fields directly off
        // `ability` (no per-hit Option<HitEffectSpec> clone, no per-spawn
        // default_hit_effect() Vec allocations).
        let rules = rules_override.unwrap_or_else(|| ability.default_hitbox_rules());

        // Declare the hitbox logically — no Rapier sensor yet.
        //
        // The sensor is deferred to `ApplyDamageFrame` so that the Rapier
        // physics step on the damage-frame tick is the first to see the
        // collider, generating CollisionEvent::started on the correct tick.
        self.state.combat.hitboxes.spawn_with_payload(
            execution_id,
            entity,
            ability_id,
            self.current_tick,
            shape,
            offset,
            rewind_ticks,
            effect_override,
            rules,
        );
        audit!(
            self.state,
            Hitbox,
            AbilityTimeline,
            3,
            Some(entity),
            "spawn"
        );
        self.emit_event(entity, EventPayload::HitboxSpawned { ability_id });
    }

    fn spawn_contact_hitbox(
        &mut self,
        entity: EntityId,
        parent_execution_id: AbilityExecutionId,
        ability_id: u32,
        shape: SkillShape,
        position: Vec3f,
        offset: Vec3f,
        effect: HitEffectSpec,
        rules: HitboxRules,
        duration_ticks: u32,
    ) {
        let sensor_shape = skill_shape_to_sensor(shape);
        let spawn_pos = Vec3f {
            x: position.x + offset.x,
            y: position.y + offset.y,
            z: position.z + offset.z,
        };
        let child_execution_id = self.state.combat.executions.next_id();
        let parent_facing = self
            .state
            .combat
            .executions
            .get(parent_execution_id)
            .map(|ctx| ctx.facing)
            .unwrap_or(Vec3f {
                x: 0.0,
                y: 0.0,
                z: 1.0,
            });

        self.state
            .combat
            .executions
            .insert(AbilityExecutionContext {
                execution_id: child_execution_id,
                ability_id,
                caster: entity,
                started_at: self.current_tick,
                targeting: ResolvedTargeting::Position { point: spawn_pos },
                origin: spawn_pos,
                facing: parent_facing,
                params: AbilityParams::default(),
                rewind_ticks: 0,
            });

        let handle = self.physics.spawn_world_sensor(
            spawn_pos,
            sensor_shape,
            ColliderKind::Hitbox(child_execution_id.0),
            entity,
        );
        self.state.combat.hitboxes.spawn_with_payload(
            child_execution_id,
            entity,
            ability_id,
            self.current_tick,
            shape,
            offset,
            0,
            Some(effect),
            rules,
        );
        if self
            .state
            .combat
            .hitboxes
            .arm_at_tick(child_execution_id, handle, self.current_tick)
        {
            if let Some(hb) = self.state.combat.hitboxes.get_mut(child_execution_id) {
                hb.world_sensor = true;
            }
            self.emit_event(entity, EventPayload::HitboxSpawned { ability_id });
            // Dedicated, client-visible event so VFX can render a brief flash
            // at the contact-spawn position. `HitboxSpawned` above is only used
            // by internal pipeline listeners; it is dropped before commit.
            let visual_radius = match sensor_shape {
                game_core::physics_backend::SensorShape::Sphere { radius } => radius,
                game_core::physics_backend::SensorShape::Capsule { radius, .. } => radius,
            };
            self.emit_event(
                entity,
                EventPayload::ContactHitboxSpawned {
                    execution_id: child_execution_id.0,
                    parent_execution_id: parent_execution_id.0,
                    ability_id,
                    position: spawn_pos,
                    radius: visual_radius,
                    duration_ticks,
                },
            );
            if duration_ticks > 0 {
                let tick_id = TickId(self.current_tick.0 + duration_ticks as u64);
                let scheduled = ScheduledAction {
                    id: {
                        let id = self.next_scheduled_id;
                        self.next_scheduled_id += 1;
                        id
                    },
                    tick_id,
                    entity,
                    source: Some(child_execution_id),
                    action_type: ScheduledActionType::AbilityFrame {
                        execution_id: child_execution_id,
                        ability_id,
                        action: AbilityAction::RemoveHitbox,
                    },
                };
                let pos = self
                    .scheduled_actions
                    .partition_point(|a| a.tick_id <= tick_id);
                self.scheduled_actions.insert(pos, scheduled);
            }
            audit!(
                self.state,
                Hitbox,
                AbilityTimeline,
                3,
                Some(entity),
                "spawn_contact"
            );
        } else {
            self.physics.remove_sensor(handle);
            self.state.combat.executions.remove(child_execution_id);
            // Symmetry with the success path: spawn_with_payload above wrote
            // an `ActiveHitbox` into the store. If arming failed we must
            // remove it too, otherwise the hitbox row leaks (sensor_handle
            // = None, armed = false, never matched by force_remove_entities
            // because we just removed the execution context).
            self.state.combat.hitboxes.remove(child_execution_id);
        }
    }

    pub(super) fn execute_ability_action(
        &mut self,
        entity: EntityId,
        ability_id: u32,
        execution_id: AbilityExecutionId,
        action: &AbilityAction,
    ) {
        match action {
            AbilityAction::SpawnHitbox { shape, offset } => {
                self.spawn_logical_hitbox(
                    entity,
                    ability_id,
                    execution_id,
                    *shape,
                    *offset,
                    None,
                    None,
                );
            }
            AbilityAction::SpawnConfiguredHitbox {
                shape,
                offset,
                effect,
                rules,
            } => {
                self.spawn_logical_hitbox(
                    entity,
                    ability_id,
                    execution_id,
                    *shape,
                    *offset,
                    effect.as_deref().cloned(),
                    *rules,
                );
            }
            AbilityAction::ApplyDamageFrame => {
                // Fast path: MultiLockOn targeting bypasses the hitbox/sensor system.
                // Damage is applied directly after re-validating each tagged target.
                let multi_data = self
                    .state
                    .combat
                    .executions
                    .get(execution_id)
                    .and_then(|ctx| {
                        if let game_core::combat::skill::ResolvedTargeting::MultiLockOn {
                            targets,
                        } = &ctx.targeting
                        {
                            Some((targets.clone(), ctx.origin))
                        } else {
                            None
                        }
                    });
                if let Some((targets, caster_pos)) = multi_data {
                    let max_range = self
                        .ability_prop(ability_id, |ad| ad.max_range, None)
                        .unwrap_or(game_core::physics_constants::DEFAULT_ABILITY_MAX_RANGE);
                    let range_sq = max_range * max_range;
                    let caster_layer = self.layer_of(entity);
                    // Validate each target: active, in 3-D range, same layer, and clear LoS.
                    let mut valid: Vec<(EntityId, EntityIndex)> = Vec::new();
                    let mut invalid: Vec<EntityId> = Vec::new();
                    for &target in &targets {
                        let is_active = self
                            .state
                            .entities
                            .lookup(target)
                            .is_some_and(|idx| self.state.entities.is_active(idx));
                        let pos = self.physics.get_transform(target).map(|t| t.position);
                        let ok = is_active
                            && pos.map_or(false, |p| {
                                let dx = p.x - caster_pos.x;
                                let dy = p.y - caster_pos.y;
                                let dz = p.z - caster_pos.z;
                                dx * dx + dy * dy + dz * dz <= range_sq
                                    && self.layer_of(target) == caster_layer
                                    && self.physics.line_of_sight_on_layer(
                                        caster_pos,
                                        p,
                                        caster_layer,
                                    )
                            });
                        if ok {
                            if let Some(idx) = self.state.entities.lookup(target) {
                                valid.push((target, idx));
                                continue;
                            }
                        }
                        invalid.push(target);
                    }
                    for (target, target_idx) in valid {
                        self.apply_hit_damage(
                            entity,
                            target,
                            target_idx,
                            ability_id,
                            false,
                            Some(execution_id),
                        );
                    }
                    for &target in &invalid {
                        self.emit_event(
                            entity,
                            EventPayload::LockOnCanceled {
                                source: entity,
                                target,
                            },
                        );
                    }
                    self.emit_event(
                        entity,
                        EventPayload::LockOnFired {
                            source: entity,
                            targets,
                        },
                    );
                    self.emit_event(entity, EventPayload::DamageFrame { ability_id });
                    return;
                }
                // Materialise the Rapier sensor for this hitbox.
                //
                // Phase 3 runs before Phase 4 (physics step), so inserting the collider
                // here means Rapier generates CollisionEvent::started on this same tick.
                // Phase 6 then resolves the contacts — damage fires exactly when the
                // timeline says the damage frame is open.
                //
                // For projectile shapes, spawn a world-space sensor at the caster's
                // origin and set up projectile travel state. Direction is resolved
                // from the targeting intent: Direction/SelfCast use caster facing,
                // Entity/LockOn aim toward the target, Position aims toward a world
                // point (range-capped).
                //
                // For ground-target abilities (Position targeting + non-Projectile
                // shape), spawn a stationary world-space sensor at the target position.
                let stored = self
                    .state
                    .combat
                    .hitboxes
                    .get(execution_id)
                    .map(|hb| (hb.shape, hb.offset));
                let ctx_data = self
                    .state
                    .combat
                    .executions
                    .get(execution_id)
                    .map(|ctx| (ctx.targeting.clone(), ctx.origin, ctx.facing));
                if let Some((shape, offset)) = stored {
                    let sensor_shape = skill_shape_to_sensor(shape);
                    let is_projectile = shape == SkillShape::Projectile;
                    // Ground-target: non-projectile shape + Position targeting = spawn
                    // sensor at the target world position (HazardZone, Sphere AoE, etc.)
                    let is_ground_target = !is_projectile
                        && matches!(
                            ctx_data.as_ref().map(|(t, _, _)| t),
                            Some(game_core::combat::skill::ResolvedTargeting::Position { .. })
                        );
                    let is_caster_offset = !is_projectile
                        && matches!(
                            ctx_data.as_ref().map(|(t, _, _)| t),
                            Some(game_core::combat::skill::ResolvedTargeting::CasterOffset)
                        );
                    let use_world_sensor = is_projectile || is_ground_target || is_caster_offset;

                    if use_world_sensor {
                        let Some((targeting, origin, default_facing)) = ctx_data else {
                            sim_warn!(
                                self,
                                "ApplyDamageFrame: execution context missing for exec_id={:?}, skipping world sensor",
                                execution_id
                            );
                            self.emit_event(entity, EventPayload::DamageFrame { ability_id });
                            return;
                        };

                        if is_ground_target {
                            // Stationary world sensor at the ground-target position.
                            let point = match &targeting {
                                game_core::combat::skill::ResolvedTargeting::Position { point } => {
                                    *point
                                }
                                _ => unreachable!(
                                    "is_ground_target already validated Position targeting"
                                ),
                            };
                            let spawn_pos = game_protocol::types::Vec3f {
                                x: point.x,
                                y: point.y + offset.y,
                                z: point.z,
                            };
                            let handle = self.physics.spawn_world_sensor(
                                spawn_pos,
                                sensor_shape,
                                ColliderKind::Hitbox(execution_id.0),
                                entity,
                            );
                            if self.state.combat.hitboxes.arm_at_tick(
                                execution_id,
                                handle,
                                self.current_tick,
                            ) {
                                if let Some(hb) = self.state.combat.hitboxes.get_mut(execution_id) {
                                    hb.world_sensor = true;
                                }
                                let radius = match sensor_shape {
                                    SensorShape::Sphere { radius } => radius,
                                    SensorShape::Capsule { radius, .. } => radius,
                                };
                                self.emit_event(
                                    entity,
                                    EventPayload::HazardSpawned {
                                        execution_id: execution_id.0,
                                        ability_id,
                                        position: spawn_pos,
                                        radius,
                                    },
                                );
                                audit!(
                                    self.state,
                                    Hitbox,
                                    AbilityTimeline,
                                    3,
                                    Some(entity),
                                    "arm_ground"
                                );
                            } else {
                                self.physics.remove_sensor(handle);
                            }
                        } else if is_caster_offset {
                            // Stationary world sensor at the caster's cast origin,
                            // oriented by their cast-time facing direction.
                            let spawn_pos = game_protocol::types::Vec3f {
                                x: origin.x + default_facing.x * offset.z + offset.x,
                                y: origin.y + offset.y,
                                z: origin.z + default_facing.z * offset.z,
                            };
                            let handle = self.physics.spawn_world_sensor(
                                spawn_pos,
                                sensor_shape,
                                ColliderKind::Hitbox(execution_id.0),
                                entity,
                            );
                            if self.state.combat.hitboxes.arm_at_tick(
                                execution_id,
                                handle,
                                self.current_tick,
                            ) {
                                if let Some(hb) = self.state.combat.hitboxes.get_mut(execution_id) {
                                    hb.world_sensor = true;
                                }
                                let radius = match sensor_shape {
                                    SensorShape::Sphere { radius } => radius,
                                    SensorShape::Capsule { radius, .. } => radius,
                                };
                                self.emit_event(
                                    entity,
                                    EventPayload::HazardSpawned {
                                        execution_id: execution_id.0,
                                        ability_id,
                                        position: spawn_pos,
                                        radius,
                                    },
                                );
                                audit!(
                                    self.state,
                                    Hitbox,
                                    AbilityTimeline,
                                    3,
                                    Some(entity),
                                    "arm_caster_offset"
                                );
                            } else {
                                self.physics.remove_sensor(handle);
                            }
                        } else {
                            // Resolve travel direction from targeting intent.
                            let facing = match &targeting {
                                game_core::combat::skill::ResolvedTargeting::Direction { dir } => {
                                    Self::normalize_direction(*dir).unwrap_or(default_facing)
                                }
                                game_core::combat::skill::ResolvedTargeting::Entity { target } => {
                                    // Aim toward the target entity's current position.
                                    self.physics
                                        .get_transform(*target)
                                        .and_then(|t| {
                                            let dx = t.position.x - origin.x;
                                            let dy = t.position.y - origin.y;
                                            let dz = t.position.z - origin.z;
                                            let len_sq = dx * dx + dy * dy + dz * dz;
                                            if len_sq > 1e-6 {
                                                let inv = 1.0 / len_sq.sqrt();
                                                Some(game_protocol::types::Vec3f::new(
                                                    dx * inv,
                                                    dy * inv,
                                                    dz * inv,
                                                ))
                                            } else {
                                                None
                                            }
                                        })
                                        .unwrap_or(default_facing)
                                }
                                game_core::combat::skill::ResolvedTargeting::Position { point } => {
                                    // Aim toward the targeted world position.
                                    let dx = point.x - origin.x;
                                    let dy = point.y - origin.y;
                                    let dz = point.z - origin.z;
                                    let len_sq = dx * dx + dy * dy + dz * dz;
                                    if len_sq > 1e-6 {
                                        let inv = 1.0 / len_sq.sqrt();
                                        game_protocol::types::Vec3f::new(
                                            dx * inv,
                                            dy * inv,
                                            dz * inv,
                                        )
                                    } else {
                                        default_facing
                                    }
                                }
                                _ => default_facing,
                            };
                            // Spawn position: origin + offset (offset.z along facing direction)
                            let spawn_pos = game_protocol::types::Vec3f {
                                x: origin.x + facing.x * offset.z + offset.x,
                                y: origin.y + offset.y,
                                z: origin.z + facing.z * offset.z,
                            };
                            // Read per-ability projectile speed and max range, falling back to defaults.
                            let projectile_speed = self
                                .ability_prop(ability_id, |ad| ad.projectile_speed, None)
                                .unwrap_or(game_core::physics_constants::DEFAULT_PROJECTILE_SPEED);
                            let ability_max_range = self
                                .ability_prop(ability_id, |ad| ad.max_range, None)
                                .unwrap_or(game_core::physics_constants::DEFAULT_ABILITY_MAX_RANGE);
                            // For Position targeting, cap the projectile range to the
                            // distance to the target point so it doesn't overshoot.
                            let max_range = match &targeting {
                                game_core::combat::skill::ResolvedTargeting::Position { point } => {
                                    let dx = point.x - spawn_pos.x;
                                    let dy = point.y - spawn_pos.y;
                                    let dz = point.z - spawn_pos.z;
                                    (dx * dx + dy * dy + dz * dz).sqrt().min(ability_max_range)
                                }
                                _ => ability_max_range,
                            };
                            let handle = self.physics.spawn_world_sensor(
                                spawn_pos,
                                sensor_shape,
                                ColliderKind::Hitbox(execution_id.0),
                                entity,
                            );
                            if self.state.combat.hitboxes.arm_at_tick(
                                execution_id,
                                handle,
                                self.current_tick,
                            ) {
                                // Set up projectile travel state.
                                if let Some(hb) = self.state.combat.hitboxes.get_mut(execution_id) {
                                    hb.projectile =
                                        Some(game_core::combat::hitbox::ProjectileState {
                                            position: spawn_pos,
                                            prev_position: spawn_pos,
                                            direction: facing,
                                            speed: projectile_speed,
                                            max_range_sq: max_range * max_range,
                                            origin: spawn_pos,
                                        });
                                }
                                // Notify clients so they can spawn a predicted visual.
                                self.emit_event(
                                    entity,
                                    EventPayload::ProjectileLaunched {
                                        execution_id: execution_id.0,
                                        ability_id,
                                        origin: spawn_pos,
                                        direction: facing,
                                        speed: projectile_speed,
                                        max_range,
                                    },
                                );
                                audit!(
                                    self.state,
                                    Hitbox,
                                    AbilityTimeline,
                                    3,
                                    Some(entity),
                                    "arm_world"
                                );
                            } else {
                                self.physics.remove_sensor(handle);
                            }
                        }
                    } else {
                        // Entity-parented sensor (melee, PBAoE, etc.)
                        if let Some(handle) = self.physics.spawn_sensor(
                            entity,
                            sensor_shape,
                            offset,
                            ColliderKind::Hitbox(execution_id.0),
                        ) {
                            if self.state.combat.hitboxes.arm_at_tick(
                                execution_id,
                                handle,
                                self.current_tick,
                            ) {
                                audit!(self.state, Hitbox, AbilityTimeline, 3, Some(entity), "arm");
                            } else {
                                self.physics.remove_sensor(handle);
                            }
                        }
                    }
                }
                self.emit_event(entity, EventPayload::DamageFrame { ability_id });
            }
            AbilityAction::RemoveHitbox => {
                if let Some(removed) = self.state.combat.hitboxes.remove(execution_id) {
                    audit!(
                        self.state,
                        Hitbox,
                        AbilityTimeline,
                        3,
                        Some(entity),
                        "remove"
                    );
                    if let Some(handle) = removed.sensor_handle {
                        self.physics.remove_sensor(handle);
                    }
                    // If this was a detached world object (projectile or hazard),
                    // notify clients to kill the visual.
                    if removed.projectile.is_some() || removed.world_sensor {
                        self.emit_event(
                            entity,
                            EventPayload::SkillObjectRemoved {
                                execution_id: execution_id.0,
                            },
                        );
                    }
                }
                // Execution context cleanup is deferred to the uniform culling pass
                // at the end of Phase 3.  execution_is_alive() will return false
                // now that the hitbox has been removed, so the context will be
                // collected on the same tick.
                self.emit_event(entity, EventPayload::HitboxRemoved { ability_id });
            }
            AbilityAction::CooldownStart { duration_ticks } => {
                // Insert directly into the cooldown map — no scheduled action needed.
                // Phase 8 drains entries where ready_at ≤ current_tick and emits CooldownReady.
                // Retroactive cooldown reduction: mutate the ready_at value for the entry directly.
                // If an existing entry for this (entity, ability) already reached readiness
                // on this same tick, emit the pending CooldownReady before overwriting it,
                // otherwise the ready event can be lost when the new ready_at is in future.
                if let Some(&old_ready) = self.cooldowns.get(&(entity, ability_id))
                    && old_ready <= self.current_tick
                {
                    // Remove the old entry and emit its CooldownReady now.
                    self.cooldowns.remove(&(entity, ability_id));
                    // This retroactive emission is initiated from AbilityTimeline (Phase 3).
                    audit!(
                        self.state,
                        Cooldown,
                        AbilityTimeline,
                        3,
                        Some(entity),
                        "expire_retro"
                    );
                    self.emit_event(entity, EventPayload::CooldownReady { ability_id });
                }
                // Read cached cooldown reduction from StatBlock (recalculated in Phase 1.5).
                let cd_reduce: f32 = self
                    .state
                    .entities
                    .lookup(entity)
                    .map(|idx| self.state.stats.get(idx).cooldown_reduce_pct)
                    .unwrap_or(0.0);
                // Same formula as the Phase 2 `CastStart` emit site —
                // shared helper guarantees the wire-visible
                // `effective_cooldown_ticks` cannot drift from the
                // value actually written into the cooldown map.
                let effective_ticks =
                    game_core::stats::apply_cooldown_reduction(*duration_ticks, cd_reduce);
                let ready_at = TickId(self.current_tick.0 + effective_ticks.max(1) as u64);
                self.cooldowns.insert((entity, ability_id), ready_at);
                audit!(
                    self.state,
                    Cooldown,
                    AbilityTimeline,
                    3,
                    Some(entity),
                    "start"
                );
            }
            AbilityAction::OpenFollowUpWindow {
                duration_ticks,
                next_ability_id,
            } => {
                // Write a combo / follow-up window for (entity, ability).
                // Phase 2 of subsequent ticks checks this: if the player presses
                // the same ability_id again within the window, the cast is
                // redirected to next_ability_id.
                let expiry = TickId(self.current_tick.0 + *duration_ticks as u64);
                self.state
                    .combat
                    .active_windows
                    .insert((entity, ability_id), (*next_ability_id, expiry));
                audit!(
                    self.state,
                    Window,
                    AbilityTimeline,
                    3,
                    Some(entity),
                    "open_window"
                );
            }
            AbilityAction::StanceBegin {
                dodge_active,
                rooted,
            } => {
                // Set iframe/root flags on the caster (timeline-driven).
                // Persists until a corresponding StanceEnd action fires.
                if let Some(t) = self.tactical_mut(entity) {
                    if *dodge_active {
                        t.dodge_stacks = t.dodge_stacks.saturating_add(1);
                    }
                    if *rooted {
                        t.movement_conditions
                            .insert(game_core::combat::tactical::MovementConditions::ROOTED);
                    }
                }
                if *rooted {
                    // If the entity is mid-air, inject a fall arc so gravity keeps acting.
                    self.apply_gravity_if_airborne(entity);
                }
                audit!(
                    self.state,
                    Tactical,
                    AbilityTimeline,
                    3,
                    Some(entity),
                    "stance_begin"
                );
            }
            AbilityAction::StanceEnd => {
                // Clear iframe/root flags on the caster (timeline-driven).
                if let Some(t) = self.tactical_mut(entity) {
                    t.dodge_stacks = t.dodge_stacks.saturating_sub(1);
                    t.movement_conditions
                        .remove(game_core::combat::tactical::MovementConditions::ROOTED);
                    // Only end deliberate arc-movement arcs. Gravity-fallthrough arcs
                    // (gravity_only=true) must keep running so the entity falls to the
                    // ground rather than hanging in mid-air after the stance ends.
                    if t.arc_state.map_or(false, |a| !a.gravity_only) {
                        t.arc_state = None;
                    }
                }
                audit!(
                    self.state,
                    Tactical,
                    AbilityTimeline,
                    3,
                    Some(entity),
                    "stance_end"
                );
            }
            AbilityAction::RootForTicks { ticks } => {
                // Implement RootForTicks by setting `rooted_until_tick` so that
                // the expiry is checked deterministically in `is_rooted()`.
                // This composes with other root sources without clobbering them.
                let until_tick = TickId(self.current_tick.0 + *ticks as u64);
                if let Some(t) = self.tactical_mut(entity) {
                    t.rooted_until_tick = Some(until_tick);
                }
                // If the entity is mid-air, inject a fall arc so gravity keeps acting.
                self.apply_gravity_if_airborne(entity);
                audit!(
                    self.state,
                    Tactical,
                    AbilityTimeline,
                    3,
                    Some(entity),
                    "root_for_ticks"
                );
            }
            AbilityAction::SetMovement { conditions } => {
                let needs_gravity_check =
                    conditions.contains(game_core::combat::tactical::MovementConditions::ROOTED);
                if let Some(t) = self.tactical_mut(entity) {
                    // Only update ability-owned bits (ROOTED | INPUT_LOCK).
                    // CC bits (STUNNED | KNOCKED_DOWN | FLOATING | SLEEPING | SILENCED | FEARED)
                    // are owned by Phase 6 combat and must not be clobbered by timeline actions.
                    use game_core::combat::tactical::MovementConditions;
                    t.movement_conditions.remove(MovementConditions::ROOTED);
                    t.movement_conditions.remove(MovementConditions::INPUT_LOCK);
                    if conditions.contains(MovementConditions::ROOTED) {
                        t.movement_conditions.insert(MovementConditions::ROOTED);
                    }
                    if conditions.contains(MovementConditions::INPUT_LOCK) {
                        t.movement_conditions.insert(MovementConditions::INPUT_LOCK);
                    }
                    if conditions.is_empty() {
                        t.rooted_until_tick = None;
                    }
                }
                if needs_gravity_check {
                    // If the entity is mid-air, inject a fall arc so gravity keeps acting.
                    self.apply_gravity_if_airborne(entity);
                }
                audit!(
                    self.state,
                    Tactical,
                    AbilityTimeline,
                    3,
                    Some(entity),
                    "set_movement"
                );
            }
            AbilityAction::ArcMovement {
                speed,
                lift,
                gravity,
            } => {
                // Launch the caster in a kinematic arc.
                // Initial velocity = facing * speed + up * lift.
                // Phase 2 will integrate gravity each tick and feed into move_character.
                let facing = self
                    .state
                    .combat
                    .executions
                    .get(execution_id)
                    .map(|ctx| ctx.facing)
                    .unwrap_or(Vec3f {
                        x: 0.0,
                        y: 0.0,
                        z: 1.0,
                    });
                if let Some(t) = self.tactical_mut(entity) {
                    t.arc_state = Some(game_core::combat::tactical::ArcState {
                        velocity: Vec3f {
                            x: facing.x * speed,
                            y: *lift,
                            z: facing.z * speed,
                        },
                        gravity: *gravity,
                        gravity_only: false,
                    });
                }
                audit!(
                    self.state,
                    Tactical,
                    AbilityTimeline,
                    3,
                    Some(entity),
                    "arc_begin"
                );
            }
            AbilityAction::ApplyBuff { buff_id } => {
                // Apply a buff to the caster (self-buff) with stacking.
                if let Some(template) = self.buff_registry.get(*buff_id) {
                    if let Some(idx) = self.state.entities.lookup(entity) {
                        let active = game_core::combat::status::ActiveBuff::from_template(
                            template,
                            entity,
                            entity,
                            self.current_tick,
                        );
                        self.state.status.apply_or_stack_buff(idx, active);
                        self.stats_dirty.insert(entity);
                        audit!(
                            self.state,
                            Buff,
                            AbilityTimeline,
                            3,
                            Some(entity),
                            "apply_buff"
                        );
                        let duration = template.duration_ticks.unwrap_or(0);
                        self.emit_event(
                            entity,
                            EventPayload::BuffApplied {
                                buff_id: *buff_id,
                                source: entity,
                                duration_ticks: duration,
                            },
                        );
                    }
                } else {
                    sim_warn!(self, "ApplyBuff: buff_id {} not found in registry", buff_id);
                }
            }
            AbilityAction::Telegraph { impact_delay } => {
                // Emit a TelegraphWarning to resolved target(s) so they can react.
                // Handles single Entity/LockOn and multi-target MultiLockOn.
                if let Some(ctx) = self.state.combat.executions.get(execution_id).cloned() {
                    let impact_tick = self.current_tick.0 + *impact_delay as u64;
                    match &ctx.targeting {
                        game_core::combat::skill::ResolvedTargeting::Entity { target } => {
                            self.emit_event(
                                *target,
                                EventPayload::TelegraphWarning {
                                    source: entity,
                                    target: *target,
                                    impact_tick,
                                },
                            );
                        }
                        game_core::combat::skill::ResolvedTargeting::MultiLockOn { targets } => {
                            for &t in targets {
                                self.emit_event(
                                    t,
                                    EventPayload::TelegraphWarning {
                                        source: entity,
                                        target: t,
                                        impact_tick,
                                    },
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            AbilityAction::Cleanse { count } => {
                // B6: Remove up to `count` Condition debuffs from self.
                // If any removed debuff had a cc_effect, also clear the matching CC timer.
                if let Some(idx) = self.state.entities.lookup(entity) {
                    let locked: HashSet<u32> = self
                        .state
                        .status
                        .get_buffs(idx)
                        .iter()
                        .filter(|ab| self.buff_is_mechanic_locked(ab.buff_id))
                        .map(|ab| ab.buff_id)
                        .collect();
                    let removed = self
                        .state
                        .status
                        .cleanse_conditions_where(idx, *count, |ab| !locked.contains(&ab.buff_id));
                    for ab in &removed {
                        self.emit_event(
                            entity,
                            EventPayload::BuffExpired {
                                buff_id: ab.buff_id,
                            },
                        );
                        if let Some(cc) = ab.modifiers.cc_effect {
                            self.clear_cc_by_effect(entity, idx, cc);
                            self.emit_event(
                                entity,
                                EventPayload::CCCleared {
                                    cc_effect: cc,
                                    source: entity,
                                },
                            );
                        }
                    }
                    if !removed.is_empty() {
                        self.stats_dirty.insert(entity);
                        self.emit_event(
                            entity,
                            EventPayload::Cleansed {
                                count: removed.len() as u32,
                                source: entity,
                            },
                        );
                    }
                }
            }
            AbilityAction::ClearCC { cc_effect } => {
                // B6: Clear a specific CC effect from self.
                if let Some(idx) = self.state.entities.lookup(entity) {
                    // Also remove the matching CC condition debuff.
                    let locked_cc_present = self.state.status.get_buffs(idx).iter().any(|ab| {
                        ab.modifiers.cc_effect == Some(*cc_effect)
                            && self.buff_is_mechanic_locked(ab.buff_id)
                    });
                    let locked: HashSet<u32> = self
                        .state
                        .status
                        .get_buffs(idx)
                        .iter()
                        .filter(|ab| self.buff_is_mechanic_locked(ab.buff_id))
                        .map(|ab| ab.buff_id)
                        .collect();
                    if let Some(ab) =
                        self.state
                            .status
                            .remove_cc_debuff_where(idx, *cc_effect, |ab| {
                                !locked.contains(&ab.buff_id)
                            })
                    {
                        self.clear_cc_by_effect(entity, idx, *cc_effect);
                        self.emit_event(
                            entity,
                            EventPayload::BuffExpired {
                                buff_id: ab.buff_id,
                            },
                        );
                        self.stats_dirty.insert(entity);
                    } else if !locked_cc_present {
                        self.clear_cc_by_effect(entity, idx, *cc_effect);
                    }
                    self.emit_event(
                        entity,
                        EventPayload::CCCleared {
                            cc_effect: *cc_effect,
                            source: entity,
                        },
                    );
                }
            }
            AbilityAction::Stunbreak => {
                // Break free of ALL active CC effects, then apply short stability.
                if let Some(idx) = self.state.entities.lookup(entity) {
                    use game_schema::CCEffect;
                    let all_cc = [
                        CCEffect::Stun,
                        CCEffect::Knockdown,
                        CCEffect::Sleep,
                        CCEffect::Silence,
                        CCEffect::Fear,
                    ];
                    for cc in &all_cc {
                        let locked_cc_present = self.state.status.get_buffs(idx).iter().any(|ab| {
                            ab.modifiers.cc_effect == Some(*cc)
                                && self.buff_is_mechanic_locked(ab.buff_id)
                        });
                        let locked: HashSet<u32> = self
                            .state
                            .status
                            .get_buffs(idx)
                            .iter()
                            .filter(|ab| self.buff_is_mechanic_locked(ab.buff_id))
                            .map(|ab| ab.buff_id)
                            .collect();
                        if let Some(ab) = self
                            .state
                            .status
                            .remove_cc_debuff_where(idx, *cc, |ab| !locked.contains(&ab.buff_id))
                        {
                            self.clear_cc_by_effect(entity, idx, *cc);
                            self.emit_event(
                                entity,
                                EventPayload::BuffExpired {
                                    buff_id: ab.buff_id,
                                },
                            );
                        } else if !locked_cc_present {
                            self.clear_cc_by_effect(entity, idx, *cc);
                        }
                    }
                    self.stats_dirty.insert(entity);
                    self.emit_event(entity, EventPayload::Stunbreak);

                    // Grant stunbreak stability (buff 401).
                    const STUNBREAK_STABILITY_BUFF_ID: u32 = 401;
                    if let Some(template) = self.buff_registry.get(STUNBREAK_STABILITY_BUFF_ID) {
                        let active = game_core::combat::status::ActiveBuff::from_template(
                            template,
                            entity,
                            entity,
                            self.current_tick,
                        );
                        self.state.status.apply_or_stack_buff(idx, active);
                        let duration = template.duration_ticks.unwrap_or(0);
                        self.emit_event(
                            entity,
                            EventPayload::BuffApplied {
                                buff_id: STUNBREAK_STABILITY_BUFF_ID,
                                source: entity,
                                duration_ticks: duration,
                            },
                        );
                    }
                }
            }
            AbilityAction::TeleportBehindTarget { distance } => {
                // Teleport the caster to a position directly behind the target entity.
                // "Behind" = target_position - target_forward * distance (horizontal only).
                // The destination is wall-clamped so the caster cannot phase through geometry.
                let data = self
                    .state
                    .combat
                    .executions
                    .get(execution_id)
                    .map(|ctx| (ctx.origin, ctx.targeting.clone()));
                let Some((caster_pos, targeting)) = data else {
                    return;
                };
                let target = match &targeting {
                    game_core::combat::skill::ResolvedTargeting::Entity { target } => *target,
                    _ => return, // TeleportBehindTarget requires Entity targeting
                };
                let Some(target_t) = self.physics.get_transform(target) else {
                    return;
                };
                // Derive the target's forward direction from its quaternion (yaw-only).
                let yaw = 2.0_f32 * target_t.rotation.y.atan2(target_t.rotation.w);
                let target_forward = Vec3f {
                    x: yaw.sin(),
                    y: 0.0,
                    z: yaw.cos(),
                };
                let raw_dest = Vec3f {
                    x: target_t.position.x - target_forward.x * distance,
                    y: target_t.position.y,
                    z: target_t.position.z - target_forward.z * distance,
                };
                let caster_layer = self.layer_of(entity);
                let destination =
                    self.physics
                        .cast_to_wall_on_layer(caster_pos, raw_dest, caster_layer);
                if self.physics.teleport_entity(entity, destination) {
                    self.emit_event(
                        entity,
                        EventPayload::Teleported {
                            entity,
                            from: caster_pos,
                            to: destination,
                        },
                    );
                }
            }
            AbilityAction::TeleportForward { distance } => {
                // Teleport the caster forward along their facing direction.
                // The destination is wall-clamped so the caster stops flush at geometry.
                let data = self
                    .state
                    .combat
                    .executions
                    .get(execution_id)
                    .map(|ctx| (ctx.origin, ctx.facing));
                let Some((caster_pos, facing)) = data else {
                    return;
                };
                let raw_dest = Vec3f {
                    x: caster_pos.x + facing.x * distance,
                    y: caster_pos.y,
                    z: caster_pos.z + facing.z * distance,
                };
                let caster_layer = self.layer_of(entity);
                let destination =
                    self.physics
                        .cast_to_wall_on_layer(caster_pos, raw_dest, caster_layer);
                if self.physics.teleport_entity(entity, destination) {
                    self.emit_event(
                        entity,
                        EventPayload::Teleported {
                            entity,
                            from: caster_pos,
                            to: destination,
                        },
                    );
                }
            }
        }
    }
}
