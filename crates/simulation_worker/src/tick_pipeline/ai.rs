use super::*;
use game_core::ai::decision::{
    AiBlackboard, AiDecisionPolicy, AiStateChangeReason, DesiredAiAction, FsmAiPolicy,
};
use game_core::combat::status::DEFAULT_THREAT_SWITCH_ADVANTAGE;

#[derive(Clone, Debug)]
enum EncounterTargeting {
    Entity(EntityId),
    Position(Vec3f),
    Direction(Vec3f),
    SelfCast,
    CasterOffset,
    MultiLockOn(Vec<EntityId>),
}

const CHASE_DIRECT_ARRIVE_RADIUS: f32 = 1.0;
const CHASE_SLOT_RADIUS: f32 = 1.35;
const CHASE_SLOT_ARRIVE_RADIUS: f32 = 0.35;
const THREAT_TARGETABLE_EPSILON: f32 = 0.001;

fn advance_route_state_if_arrived(
    state: &mut game_core::ai::routes::RouteFollowState,
    route: &game_core::ai::routes::RouteDef,
    current_pos: Vec3f,
) {
    while !state.completed {
        let Some(target) = route.waypoints.get(state.waypoint_index).copied() else {
            state.completed = true;
            return;
        };
        let dx = target.x - current_pos.x;
        let dz = target.z - current_pos.z;
        if dx * dx + dz * dz > route.arrive_radius * route.arrive_radius {
            return;
        }
        let next_index = state.waypoint_index.saturating_add(1);
        if next_index < route.waypoints.len() {
            state.waypoint_index = next_index;
        } else if route.looped {
            state.waypoint_index = 0;
        } else {
            state.completed = true;
        }
    }
}

fn threat_value_is_targetable(threat: f32) -> bool {
    threat.is_finite() && threat > THREAT_TARGETABLE_EPSILON
}

impl TickPipeline {
    // ── Phase 7: AI decisions ───────────────────────────────────

    pub(super) fn build_ai_blackboard(&self, idx: EntityIndex) -> Option<AiBlackboard<'_>> {
        let self_id = self.state.entities.id_of(idx);
        let state = self.state.ai.npc_ai.get(idx).copied()?;
        let transform = self.physics.get_transform(self_id)?;

        let mut blackboard = AiBlackboard::new(self_id, state, transform.position);
        blackboard.layer = self.layer_of_idx(idx);
        blackboard.home_position = self.state.ai.home_positions.get(idx).copied();
        let current_target = self
            .npc_state_prev
            .get(&self_id)
            .and_then(|(_, target)| *target);
        if let Some(table) = self.state.combat.threat_tables.get(idx) {
            blackboard.top_threat = self.top_perceived_threat(idx, table);
            blackboard.selected_target = self.select_perceived_target_with_hysteresis(
                idx,
                table,
                current_target,
                DEFAULT_THREAT_SWITCH_ADVANTAGE,
            );
        }
        blackboard.current_target = current_target;
        blackboard.goal_kind = self
            .npc_goals
            .get(&self_id)
            .map(|(goal_kind, _)| goal_kind.as_str());
        blackboard.cc_disabled = self.is_cc_disabled(idx);
        blackboard.silenced = self.is_silenced(idx);
        blackboard.rooted = self.is_rooted(idx);
        blackboard.no_chase = self.state.ai.npc_no_chase.get(idx).copied() == Some(true);
        blackboard.leash_radius = self
            .state
            .ai
            .npc_leash_radius
            .get(idx)
            .copied()
            .unwrap_or(0.0);
        blackboard.aggro_radius = self
            .state
            .ai
            .npc_aggro_radius
            .get(idx)
            .copied()
            .unwrap_or(0.0);

        Some(blackboard)
    }

    fn entity_is_stealthed(&self, target_idx: EntityIndex) -> bool {
        self.state
            .status
            .get_buffs(target_idx)
            .iter()
            .any(|buff| buff.modifiers.stealth == Some(true))
    }

    fn ai_can_perceive(&self, observer_idx: EntityIndex, target_idx: EntityIndex) -> bool {
        if observer_idx == target_idx {
            return true;
        }
        if !self.state.entities.is_active(target_idx) {
            return false;
        }
        if !self.entity_is_stealthed(target_idx) {
            return true;
        }
        let observer_team = self.team_of_idx(observer_idx);
        let target_team = self.team_of_idx(target_idx);
        observer_team != 0 && observer_team == target_team
    }

    fn top_perceived_threat(
        &self,
        observer_idx: EntityIndex,
        table: &game_core::combat::status::ThreatTable,
    ) -> Option<EntityId> {
        table
            .entries
            .iter()
            .filter(|entry| threat_value_is_targetable(entry.threat))
            .filter(|entry| self.ai_target_is_valid(observer_idx, entry.source))
            .max_by(|a, b| a.threat.total_cmp(&b.threat))
            .map(|entry| entry.source)
    }

    fn select_perceived_target_with_hysteresis(
        &self,
        observer_idx: EntityIndex,
        table: &game_core::combat::status::ThreatTable,
        current_target: Option<EntityId>,
        switch_advantage: f32,
    ) -> Option<EntityId> {
        let top_target = self.top_perceived_threat(observer_idx, table)?;
        let top_threat = table.threat_of(top_target)?;
        let Some(current_target) = current_target else {
            return Some(top_target);
        };
        if current_target == top_target {
            return Some(current_target);
        }
        if !self.ai_target_is_valid(observer_idx, current_target) {
            return Some(top_target);
        }
        let Some(current_threat) = table.threat_of(current_target) else {
            return Some(top_target);
        };

        let advantage = switch_advantage.max(0.0);
        if top_threat > current_threat * (1.0 + advantage) {
            Some(top_target)
        } else {
            Some(current_target)
        }
    }

    fn apply_proximity_aggro(&mut self, idx: EntityIndex, players: &[EntityIndex]) {
        let aggro = self
            .state
            .ai
            .npc_aggro_radius
            .get(idx)
            .copied()
            .unwrap_or(0.0);
        if aggro <= 0.0 {
            return;
        }

        let npc_id = self.state.entities.id_of(idx);
        let npc_layer = self.layer_of_idx(idx);
        let Some(npc_t) = self.physics.get_transform(npc_id) else {
            return;
        };
        let npc_pos = npc_t.position;
        let aggro_sq = aggro * aggro;

        for &player_idx in players {
            let player_id = self.state.entities.id_of(player_idx);
            if self.layer_of_idx(player_idx) != npc_layer {
                continue;
            }
            if !self.ai_can_perceive(idx, player_idx) {
                continue;
            }
            let Some(player_t) = self.physics.get_transform(player_id) else {
                continue;
            };
            let dx = player_t.position.x - npc_pos.x;
            let dz = player_t.position.z - npc_pos.z;
            if dx * dx + dz * dz > aggro_sq {
                continue;
            }

            if !self.state.combat.threat_tables.contains(idx) {
                self.state
                    .combat
                    .threat_tables
                    .insert(idx, game_core::combat::status::ThreatTable::default());
            }
            let table = self.state.combat.threat_tables.get_mut(idx).unwrap();
            if table.entries.iter().all(|entry| entry.source != player_id) {
                table.entries.push(game_core::combat::status::ThreatEntry {
                    source: player_id,
                    threat: 1.0,
                });
            }
            audit!(
                self.state,
                Ai,
                AiDecisions,
                7,
                Some(npc_id),
                "aggro_proximity"
            );
            break;
        }
    }

    fn set_ai_state_for_reason(
        &mut self,
        idx: EntityIndex,
        state: game_schema::NpcAiState,
        reason: AiStateChangeReason,
    ) {
        if let Some(ai) = self.state.ai.npc_ai.get_mut(idx) {
            *ai = state;
        }
        let npc_id = self.state.entities.id_of(idx);
        match reason {
            AiStateChangeReason::ToCombat => {
                audit!(self.state, Ai, AiDecisions, 7, None, "to_combat");
            }
            AiStateChangeReason::CombatToIdle => {
                audit!(self.state, Ai, AiDecisions, 7, None, "combat_to_idle");
            }
            AiStateChangeReason::LeashEvade => {
                audit!(self.state, Ai, AiDecisions, 7, Some(npc_id), "leash_evade");
            }
            AiStateChangeReason::FleeToIdle => {
                audit!(self.state, Ai, AiDecisions, 7, None, "flee_to_idle");
            }
            AiStateChangeReason::GoalIdle => {
                audit!(self.state, Ai, AiDecisions, 7, Some(npc_id), "goal_idle");
            }
            AiStateChangeReason::EvadeNoHome => {}
        }
    }

    fn sanitize_combat_threat_table(&mut self, idx: EntityIndex) {
        let npc_layer = self.layer_of_idx(idx);
        if let Some(table) = self.state.combat.threat_tables.get_mut(idx) {
            let entities = &self.state.entities;
            let cache = &self.entity_layer_cache;
            table.entries.retain(|entry| {
                entities.lookup(entry.source).map_or(false, |source_idx| {
                    let slot = source_idx.as_usize();
                    let source_layer = if slot < cache.len() { cache[slot] } else { 0 };
                    let source_kind = entities.kinds[slot];
                    source_layer == npc_layer
                        && source_kind != game_core::entity::lifecycle::EntityKind::Prop
                })
            });
        }
    }

    fn ai_target_is_valid(&self, idx: EntityIndex, target: EntityId) -> bool {
        let npc_layer = self.layer_of_idx(idx);
        self.state
            .entities
            .lookup(target)
            .is_some_and(|target_idx| {
                if !self.state.entities.is_active(target_idx)
                    || !self.ai_can_perceive(idx, target_idx)
                {
                    return false;
                }
                let slot = target_idx.as_usize();
                let target_layer = if slot < self.entity_layer_cache.len() {
                    self.entity_layer_cache[slot]
                } else {
                    0
                };
                let target_kind = self.state.entities.kinds[slot];
                target_layer == npc_layer
                    && target_kind != game_core::entity::lifecycle::EntityKind::Prop
            })
    }

    fn try_npc_cast_best_ability(
        &mut self,
        npc_id: EntityId,
        idx: EntityIndex,
        target_id: EntityId,
    ) {
        let ability_ids = self
            .state
            .ai
            .npc_ability_ids
            .get(idx)
            .cloned()
            .unwrap_or_else(|| vec![1]);
        for ability_id in ability_ids {
            let Some(targeting) =
                self.resolve_ai_ability_targeting(npc_id, idx, target_id, ability_id)
            else {
                continue;
            };
            if !self.reserve_ability_cast(npc_id, ability_id) {
                continue;
            }
            if self.cast_ability(npc_id, ability_id, targeting, 0, 0) {
                audit!(
                    self.state,
                    Execution,
                    AiDecisions,
                    7,
                    Some(npc_id),
                    "npc_cast"
                );
                break;
            }
            self.release_ability_reservation(npc_id, ability_id);
        }
    }

    fn apply_evade_home(&mut self, npc_id: EntityId, idx: EntityIndex, home: Vec3f) {
        self.npc_move_toward_pos(npc_id, home, self.dt);
        audit!(
            self.state,
            Transform,
            AiDecisions,
            7,
            Some(npc_id),
            "evade_walk"
        );

        if let Some(npc_t) = self.physics.get_transform(npc_id) {
            let dx = npc_t.position.x - home.x;
            let dz = npc_t.position.z - home.z;
            const R: f32 = game_core::physics_constants::EVADE_ARRIVE_RADIUS;
            if dx * dx + dz * dz <= R * R {
                if let Some(table) = self.state.combat.threat_tables.get_mut(idx) {
                    table.entries.clear();
                }
                let max_hp = self.state.combat.health.max_hp[idx.as_usize()];
                let current_hp = self.state.combat.health.hp[idx.as_usize()];
                if current_hp < max_hp {
                    self.pending_heals
                        .push((npc_id, max_hp - current_hp, npc_id));
                }
                if let Some(ai) = self.state.ai.npc_ai.get_mut(idx) {
                    *ai = game_schema::NpcAiState::Idle;
                }
                audit!(self.state, Ai, AiDecisions, 7, Some(npc_id), "evade_home");
            }
        }

        if let Some(table) = self.state.combat.threat_tables.get_mut(idx) {
            table.entries.clear();
        }
    }

    fn collect_ai_action_plans(
        &self,
        ai_actors: &[EntityIndex],
        skip_decisions: &HashSet<EntityIndex>,
        actions: &mut Vec<DesiredAiAction>,
        action_ranges: &mut Vec<(EntityIndex, usize, usize)>,
        chase_slots: &mut Vec<(EntityId, EntityId)>,
    ) {
        actions.clear();
        action_ranges.clear();
        chase_slots.clear();
        for idx in ai_actors {
            if skip_decisions.contains(idx)
                || self.state.ai.npc_passive.get(*idx).copied() == Some(true)
            {
                continue;
            }
            let Some(blackboard) = self.build_ai_blackboard(*idx) else {
                continue;
            };
            let start = actions.len();
            if let Some(policy) = self.ai_behavior_trees.get(&blackboard.self_id) {
                policy.decide_into(&blackboard, actions);
            } else {
                FsmAiPolicy.decide_into(&blackboard, actions);
            }
            let end = actions.len();
            if let Some(target) = actions[start..end].iter().find_map(|action| match action {
                DesiredAiAction::MoveTowardEntity(target) => Some(*target),
                _ => None,
            }) && self.ai_target_is_valid(*idx, target)
            {
                chase_slots.push((target, blackboard.self_id));
            }
            action_ranges.push((*idx, start, end));
        }
        chase_slots.sort_by_key(|(target, actor)| (target.0, actor.0));
    }

    fn apply_desired_ai_actions(
        &mut self,
        idx: EntityIndex,
        actions: &[DesiredAiAction],
        chase_slots: &[(EntityId, EntityId)],
    ) {
        let npc_id = self.state.entities.id_of(idx);
        for action in actions {
            match action {
                DesiredAiAction::SetState { state, reason } => {
                    self.set_ai_state_for_reason(idx, *state, *reason);
                }
                DesiredAiAction::StopMovement => {}
                DesiredAiAction::ClearThreat => {
                    if let Some(table) = self.state.combat.threat_tables.get_mut(idx) {
                        table.entries.clear();
                    }
                }
                DesiredAiAction::MoveTowardEntity(target_id) => {
                    if self.ai_target_is_valid(idx, *target_id) {
                        self.npc_move_toward(npc_id, idx, *target_id, self.dt, chase_slots);
                        audit!(self.state, Transform, AiDecisions, 7, Some(npc_id), "chase");
                    }
                }
                DesiredAiAction::MoveAwayFromEntity(target_id) => {
                    let npc_layer = self.layer_of_idx(idx);
                    if self.layer_of(*target_id) == npc_layer {
                        self.npc_move_away(npc_id, idx, *target_id, self.dt);
                        audit!(self.state, Transform, AiDecisions, 7, Some(npc_id), "flee");
                    }
                }
                DesiredAiAction::MoveToPoint(point) => {
                    self.npc_move_toward_pos(npc_id, *point, self.dt);
                    audit!(
                        self.state,
                        Transform,
                        AiDecisions,
                        7,
                        Some(npc_id),
                        "patrol"
                    );
                }
                DesiredAiAction::FollowRoute { route_id } => {
                    self.follow_ai_route(npc_id, idx, route_id.as_str());
                }
                DesiredAiAction::EvadeHome { home_position } => {
                    self.ai_route_follow.remove(&npc_id);
                    self.apply_evade_home(npc_id, idx, *home_position);
                }
                DesiredAiAction::TryCastBestAbility { target } => {
                    if self.ai_target_is_valid(idx, *target) {
                        self.try_npc_cast_best_ability(npc_id, idx, *target);
                    }
                }
            }
        }
    }

    fn follow_ai_route(&mut self, npc_id: EntityId, idx: EntityIndex, route_id: &str) {
        if self.is_cc_disabled(idx) || self.is_rooted(idx) {
            return;
        }
        // Borrow the route from the registry instead of cloning it. The waypoint
        // vector and id are read-only here, so a shared borrow of `self.routes`
        // coexists with disjoint mutable access to `self.ai_route_follow`.
        let Some(route) = self.routes.get(route_id) else {
            self.ai_route_follow.remove(&npc_id);
            return;
        };
        if let Some(layer_scope) = route.layer_scope
            && layer_scope != self.layer_of_idx(idx)
        {
            self.ai_route_follow.remove(&npc_id);
            return;
        }
        let Some(current_pos) = self.physics.get_transform(npc_id).map(|t| t.position) else {
            return;
        };

        // Resolve the next destination while the route borrow is live, then copy
        // out the `Copy` values so the route borrow ends before the `&mut self`
        // movement call below.
        let resolved = {
            let state = self
                .ai_route_follow
                .entry(npc_id)
                .and_modify(|state| {
                    if state.route_id != route.route_id {
                        *state =
                            game_core::ai::routes::RouteFollowState::new(route.route_id.clone());
                    }
                })
                .or_insert_with(|| {
                    game_core::ai::routes::RouteFollowState::new(route.route_id.clone())
                });

            if state.completed {
                None
            } else {
                advance_route_state_if_arrived(state, route, current_pos);
                if state.completed {
                    None
                } else {
                    Some((route.waypoints[state.waypoint_index], route.arrive_radius))
                }
            }
        };
        let Some((destination, arrive_radius)) = resolved else {
            return;
        };

        self.npc_move_toward_pos_with_radius(npc_id, destination, self.dt, arrive_radius);
        audit!(
            self.state,
            Transform,
            AiDecisions,
            7,
            Some(npc_id),
            "route_follow"
        );
    }

    pub(super) fn phase_ai_decisions(&mut self) {
        use game_core::combat::status::AiOverride;
        use game_core::entity::lifecycle::{EntityKind, NpcAiState};

        let mut ai_actors = std::mem::take(&mut self.ai_actor_indices_scratch);
        ai_actors.clear();
        self.state
            .extend_active_indices_of_kind(EntityKind::Npc, &mut ai_actors);
        self.state
            .extend_active_indices_of_kind(EntityKind::Boss, &mut ai_actors);

        // Pre-compute player indices once for proximity aggro scanning.
        let mut players = std::mem::take(&mut self.ai_player_indices_scratch);
        players.clear();
        self.state
            .extend_active_indices_of_kind(EntityKind::Player, &mut players);
        let mut actions = std::mem::take(&mut self.ai_action_scratch);
        let mut skip_decisions = std::mem::take(&mut self.ai_skip_decision_scratch);
        skip_decisions.clear();
        let mut chase_slots = std::mem::take(&mut self.ai_chase_slot_scratch);
        chase_slots.clear();
        let mut action_ranges = std::mem::take(&mut self.ai_action_ranges_scratch);
        action_ranges.clear();

        for idx in &ai_actors {
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
                        skip_decisions.insert(*idx);

                        if !self.is_cc_disabled(*idx) {
                            let npc_id = self.state.entities.id_of(*idx);
                            if let Some(threat_source) = self
                                .state
                                .combat
                                .threat_tables
                                .get(*idx)
                                .and_then(|table| table.top_threat())
                            {
                                let npc_layer = self.layer_of_idx(*idx);
                                if self.layer_of(threat_source) == npc_layer {
                                    self.npc_move_away(npc_id, *idx, threat_source, self.dt);
                                    audit!(
                                        self.state,
                                        Transform,
                                        AiDecisions,
                                        7,
                                        Some(npc_id),
                                        "flee"
                                    );
                                }
                            }
                        }
                        continue;
                    }
                    AiOverride::ForceIdle => {
                        if let Some(ai) = self.state.ai.npc_ai.get_mut(*idx) {
                            *ai = NpcAiState::Idle;
                        }
                        audit!(self.state, Ai, AiDecisions, 7, None, "override_idle");
                        skip_decisions.insert(*idx);
                        continue;
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
            if !override_applied
                && matches!(ai_state, NpcAiState::Idle | NpcAiState::Patrol)
                && self
                    .state
                    .combat
                    .threat_tables
                    .get(*idx)
                    .and_then(|table| self.top_perceived_threat(*idx, table))
                    .is_none()
            {
                self.apply_proximity_aggro(*idx, &players);
            }

            self.sanitize_combat_threat_table(*idx);
        }

        self.collect_ai_action_plans(
            &ai_actors,
            &skip_decisions,
            &mut actions,
            &mut action_ranges,
            &mut chase_slots,
        );

        for (idx, start, end) in &action_ranges {
            self.apply_desired_ai_actions(*idx, &actions[*start..*end], &chase_slots);
        }
        actions.clear();
        action_ranges.clear();
        ai_actors.clear();
        players.clear();
        skip_decisions.clear();
        chase_slots.clear();
        self.ai_action_scratch = actions;
        self.ai_actor_indices_scratch = ai_actors;
        self.ai_player_indices_scratch = players;
        self.ai_skip_decision_scratch = skip_decisions;
        self.ai_chase_slot_scratch = chase_slots;
        self.ai_action_ranges_scratch = action_ranges;
    }

    // ── NPC movement helpers (Phase 7) ─────────────────────────

    fn chase_slot_destination(
        &self,
        npc_id: EntityId,
        target_id: EntityId,
        chase_slots: &[(EntityId, EntityId)],
    ) -> Option<Vec3f> {
        let start = chase_slots.partition_point(|(target, _)| target.0 < target_id.0);
        let end = chase_slots.partition_point(|(target, _)| target.0 <= target_id.0);
        let group = &chase_slots[start..end];
        if group.len() <= 1 {
            return None;
        }
        let slot_index = group.iter().position(|(_, actor)| *actor == npc_id)?;
        let target_pos = self.physics.get_transform(target_id)?.position;
        let angle = (slot_index as f32 / group.len() as f32) * std::f32::consts::TAU;
        Some(Vec3f {
            x: target_pos.x + angle.cos() * CHASE_SLOT_RADIUS,
            y: target_pos.y,
            z: target_pos.z + angle.sin() * CHASE_SLOT_RADIUS,
        })
    }

    /// Move `npc_id` one step toward `target_id`'s current physics position.
    /// Uses the NPC's authoritative base speed. No-ops if either transform is unavailable.
    fn npc_move_toward(
        &mut self,
        npc_id: EntityId,
        npc_idx: game_core::entity::entity_index::EntityIndex,
        target_id: EntityId,
        dt: f32,
        chase_slots: &[(EntityId, EntityId)],
    ) {
        let (npc_pos, target_pos) = match (
            self.physics.get_transform(npc_id),
            self.physics.get_transform(target_id),
        ) {
            (Some(a), Some(b)) => (a.position, b.position),
            _ => return,
        };
        let (destination, arrive_radius) =
            if let Some(slot) = self.chase_slot_destination(npc_id, target_id, chase_slots) {
                (slot, CHASE_SLOT_ARRIVE_RADIUS)
            } else {
                (target_pos, CHASE_DIRECT_ARRIVE_RADIUS)
            };
        let dx = destination.x - npc_pos.x;
        let dz = destination.z - npc_pos.z;
        if dx * dx + dz * dz <= arrive_radius * arrive_radius {
            return;
        }
        self.npc_step_toward(npc_id, npc_idx, npc_pos, destination, dt);
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
        self.npc_move_toward_pos_with_radius(npc_id, dest, dt, 0.5);
    }

    fn npc_move_toward_pos_with_radius(
        &mut self,
        npc_id: EntityId,
        dest: Vec3f,
        dt: f32,
        arrive_radius: f32,
    ) {
        let npc_pos = match self.physics.get_transform(npc_id) {
            Some(t) => t.position,
            None => return,
        };
        // Stop if already close enough — prevents micro-jitter at the home point.
        let dx = dest.x - npc_pos.x;
        let dz = dest.z - npc_pos.z;
        if dx * dx + dz * dz <= arrive_radius * arrive_radius {
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
        //
        // Players currently in reconnect grace
        // (`InstanceMembership.disconnect_at.is_some()`) are excluded — see
        // `docs/contracts/world_activity_policy_contract.md` "no offscreen
        // combat" rule. Their entity rows still exist (so reconnect can
        // restore them) but they should not count as "present" for
        // `PlayerCountAtLeast` / `WorldActivityEventActive` triggers or the
        // Director's per-region scaling.
        //
        // Bare `WorldPhase` triggers do not consult this count, so they can
        // still fire into an empty cell; the shipped `open_world_origin_boss`
        // rule has been migrated to `WorldActivityEventActive` which combines
        // a presence floor with a scoped event row. Authored rules that
        // spawn actors should prefer `WorldActivityEventActive` over bare
        // `WorldPhase` for the same reason.
        let mut region_player_counts: HashMap<(i32, i32, u32), u32> = HashMap::new();
        for (eid, cell) in &self.entity_regions {
            if self.disconnected_players.contains(eid) {
                continue;
            }
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
        self.director.evaluate(
            &region_player_counts,
            self.current_tick,
            &self.world_phases,
            &self.world_activity_events,
        )
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

    pub(super) fn apply_replace_ability_list(
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

    pub(super) fn apply_spawn_adds(
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
        // Surface guard: SpawnAdds may only spawn Npc-kind add archetypes whose
        // `usage` permits the encounter-add path. Anything else (ActorOnly
        // rows, Boss-kind archetypes) is dropped at the source so we never
        // commit a director spawn or a membership row that the worker would
        // later refuse to install in `register_encounter_add_with_archetype`.
        // content-check rejects these statically; this is the defensive
        // runtime mirror for live-edited or otherwise-bypassed content.
        if !profile.usage.allows_encounter_add() {
            log::warn!(
                "Encounter: boss {} SpawnAdds archetype '{}' has usage {:?}; encounter-add path requires Both or AddOnly — skipping",
                boss_entity_id.0,
                archetype,
                profile.usage,
            );
            return;
        }
        if profile.kind != game_schema::EntityKind::Npc {
            log::warn!(
                "Encounter: boss {} SpawnAdds archetype '{}' has kind {:?}; only Npc-kind adds are supported — skipping",
                boss_entity_id.0,
                archetype,
                profile.kind,
            );
            return;
        }
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
                npc_config: profile.director_npc_config(),
                team_id: profile.team_id,
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

    pub(super) fn apply_spawn_volume(
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

    pub(super) fn apply_cast_skill(
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

        let targetings = self.resolve_encounter_targetings(boss_entity_id, idx, skill_id, &target);
        if targetings.is_empty() {
            log::debug!(
                "Encounter: boss {} cast {} skipped (target {:?} unresolved)",
                boss_entity_id.0,
                skill_id,
                target,
            );
            return;
        };

        if !self.reserve_ability_cast(boss_entity_id, skill_id) {
            log::debug!(
                "Encounter: boss {} cast {} skipped (cooldown/reserved)",
                boss_entity_id.0,
                skill_id
            );
            return;
        }

        let mut any_cast = false;
        for targeting in targetings {
            let resolved = Self::to_resolved_targeting(targeting);
            let casted = self.cast_ability(boss_entity_id, skill_id, resolved, 0, 0);
            if !casted {
                log::debug!(
                    "Encounter: boss {} cast {} rejected after target resolution",
                    boss_entity_id.0,
                    skill_id
                );
            } else {
                any_cast = true;
            }
        }
        if !any_cast {
            self.release_ability_reservation(boss_entity_id, skill_id);
        }
    }

    /// Resolve a telegraph target and emit either entity lock-on warnings or
    /// area telegraphs for world-position AoEs.
    pub(super) fn apply_telegraph(
        &mut self,
        boss_entity_id: EntityId,
        skill_id: u32,
        target: game_core::encounter::Target,
        lead_ticks: u32,
    ) {
        let Some(idx) = self.state.entities.lookup(boss_entity_id) else {
            return;
        };
        let impact_tick = TickId(self.current_tick.0.saturating_add(lead_ticks as u64));
        if !self.ability_ready_at(boss_entity_id, skill_id, impact_tick) {
            log::debug!(
                "Encounter: boss {} telegraph skill {} skipped (not ready by impact tick {})",
                boss_entity_id.0,
                skill_id,
                impact_tick.0
            );
            return;
        }
        let targetings = self.resolve_encounter_targetings(boss_entity_id, idx, skill_id, &target);
        if targetings.is_empty() {
            log::debug!(
                "Encounter: boss {} telegraph skill {} skipped (target {:?} unresolved)",
                boss_entity_id.0,
                skill_id,
                target,
            );
            return;
        };
        for targeting in targetings {
            match targeting {
                EncounterTargeting::Entity(target_id) => {
                    self.emit_event(
                        target_id,
                        EventPayload::TelegraphWarning {
                            source: boss_entity_id,
                            target: target_id,
                            impact_tick: impact_tick.0,
                        },
                    );
                }
                EncounterTargeting::MultiLockOn(targets) => {
                    for target_id in targets {
                        self.emit_event(
                            target_id,
                            EventPayload::TelegraphWarning {
                                source: boss_entity_id,
                                target: target_id,
                                impact_tick: impact_tick.0,
                            },
                        );
                    }
                }
                other => {
                    if let Some((position, radius, shape)) =
                        self.area_telegraph_data(boss_entity_id, skill_id, &other)
                    {
                        self.emit_event(
                            boss_entity_id,
                            EventPayload::AreaTelegraph {
                                source: boss_entity_id,
                                ability_id: skill_id,
                                position,
                                radius,
                                shape,
                                impact_tick: impact_tick.0,
                            },
                        );
                    }
                }
            }
        }
    }

    pub(super) fn apply_encounter_cue(
        &mut self,
        boss_entity_id: EntityId,
        target: &game_core::encounter::Target,
        cue_id: &str,
        anchor: game_core::encounter::EncounterCueAnchor,
        shape: game_core::encounter::EncounterCueShape,
        lead_ticks: u32,
        duration_ticks: u32,
    ) {
        let targets = self.resolve_encounter_effect_targets(boss_entity_id, target);
        if targets.is_empty() {
            log::debug!(
                "Encounter: boss {} cue '{}' skipped (target {:?} unresolved)",
                boss_entity_id.0,
                cue_id,
                target,
            );
            return;
        }

        let starts_at_tick = self.current_tick.0.saturating_add(lead_ticks as u64);
        let expires_at_tick = starts_at_tick.saturating_add(duration_ticks as u64);
        let (shape_name, inner_radius, outer_radius, half_height) =
            Self::encounter_cue_shape_data(shape);

        for target_id in targets {
            let (anchor_entity, position) =
                self.encounter_cue_anchor_snapshot(boss_entity_id, target_id, &anchor);
            self.emit_event(
                target_id,
                EventPayload::EncounterCue {
                    source: boss_entity_id,
                    target: target_id,
                    cue_id: cue_id.to_string(),
                    anchor_entity,
                    position,
                    shape: shape_name.to_string(),
                    inner_radius,
                    outer_radius,
                    half_height,
                    starts_at_tick,
                    expires_at_tick,
                },
            );
        }
    }

    fn encounter_cue_anchor_snapshot(
        &self,
        boss_entity_id: EntityId,
        target_id: EntityId,
        anchor: &game_core::encounter::EncounterCueAnchor,
    ) -> (Option<EntityId>, Vec3f) {
        match anchor {
            game_core::encounter::EncounterCueAnchor::Boss => (
                Some(boss_entity_id),
                self.physics
                    .get_transform(boss_entity_id)
                    .map(|t| t.position)
                    .unwrap_or_else(|| Vec3f::new(0.0, 0.0, 0.0)),
            ),
            game_core::encounter::EncounterCueAnchor::Target => (
                Some(target_id),
                self.physics
                    .get_transform(target_id)
                    .map(|t| t.position)
                    .unwrap_or_else(|| Vec3f::new(0.0, 0.0, 0.0)),
            ),
            game_core::encounter::EncounterCueAnchor::FixedPoint { position } => {
                (None, Vec3f::new(position[0], position[1], position[2]))
            }
        }
    }

    fn encounter_cue_shape_data(
        shape: game_core::encounter::EncounterCueShape,
    ) -> (&'static str, f32, f32, f32) {
        match shape {
            game_core::encounter::EncounterCueShape::None => ("none", 0.0, 0.0, 0.0),
            game_core::encounter::EncounterCueShape::Sphere { radius } => {
                ("sphere", 0.0, radius, 0.0)
            }
            game_core::encounter::EncounterCueShape::Ring {
                inner_radius,
                outer_radius,
                half_height,
            } => ("ring", inner_radius, outer_radius, half_height),
        }
    }

    fn resolve_encounter_effect_targets(
        &self,
        boss_entity_id: EntityId,
        target: &game_core::encounter::Target,
    ) -> Vec<EntityId> {
        let Some(boss_idx) = self.state.entities.lookup(boss_entity_id) else {
            return Vec::new();
        };
        self.resolve_encounter_targets(boss_entity_id, boss_idx, target)
    }

    pub(super) fn apply_encounter_buff(
        &mut self,
        boss_entity_id: EntityId,
        target: &game_core::encounter::Target,
        buff_id: u32,
        mode: game_core::encounter::BuffApplyMode,
    ) {
        let Some(template) = self.buff_registry.get(buff_id).cloned() else {
            sim_warn!(
                self,
                "Encounter: boss {} ApplyBuff skipped; buff_id {} not found",
                boss_entity_id.0,
                buff_id
            );
            return;
        };
        let targets = self.resolve_encounter_effect_targets(boss_entity_id, target);
        for target_id in targets {
            let Some(idx) = self.state.entities.lookup(target_id) else {
                continue;
            };
            if let game_core::encounter::BuffApplyMode::ReplaceAny(buff_ids) = &mode {
                // ReplaceAny from the encounter runtime is the
                // legitimate owner of mechanic-locked buffs (e.g.
                // Manaya's Core mark swaps), so no MechanicLocked
                // exemption is applied here. Player-cast cleanse-style
                // skills that need to respect MechanicLocked must use
                // `apply_encounter_remove_buffs` / `RemoveBuffs`
                // semantics or filter via a different code path —
                // this entry point is encounter-owned by construction.
                let removed = self
                    .state
                    .status
                    .remove_buffs_by_ids_where(idx, buff_ids, |_| true);
                if !removed.is_empty() {
                    audit!(
                        self.state,
                        Buff,
                        EncounterRuntime,
                        7,
                        Some(target_id),
                        "encounter_replace_buff"
                    );
                    self.stats_dirty.insert(target_id);
                    for ab in removed {
                        self.emit_event(
                            target_id,
                            EventPayload::BuffExpired {
                                buff_id: ab.buff_id,
                            },
                        );
                    }
                }
            }
            let active = game_core::combat::status::ActiveBuff::from_template(
                &template,
                boss_entity_id,
                target_id,
                self.current_tick,
            );
            audit!(
                self.state,
                Buff,
                EncounterRuntime,
                7,
                Some(target_id),
                "encounter_apply_buff"
            );
            self.state.status.apply_or_stack_buff(idx, active);
            self.stats_dirty.insert(target_id);
            self.emit_event(
                target_id,
                EventPayload::BuffApplied {
                    buff_id,
                    source: boss_entity_id,
                    duration_ticks: template.duration_ticks.unwrap_or(0),
                },
            );
        }
    }

    pub(super) fn apply_encounter_remove_buffs(
        &mut self,
        boss_entity_id: EntityId,
        target: &game_core::encounter::Target,
        buff_ids: &[u32],
        force: bool,
    ) {
        if buff_ids.is_empty() {
            return;
        }
        let targets = self.resolve_encounter_effect_targets(boss_entity_id, target);
        for target_id in targets {
            let Some(idx) = self.state.entities.lookup(target_id) else {
                continue;
            };
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
                .remove_buffs_by_ids_where(idx, buff_ids, |ab| {
                    force || !locked.contains(&ab.buff_id)
                });
            if removed.is_empty() {
                continue;
            }
            audit!(
                self.state,
                Buff,
                EncounterRuntime,
                7,
                Some(target_id),
                "encounter_remove_buffs"
            );
            self.stats_dirty.insert(target_id);
            for ab in removed {
                self.emit_event(
                    target_id,
                    EventPayload::BuffExpired {
                        buff_id: ab.buff_id,
                    },
                );
            }
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
            game_core::encounter::Target::RuntimeEntity { entity } => {
                if !self.same_layer(boss_id, *entity) {
                    return Vec::new();
                }
                let Some(idx) = self.state.entities.lookup(*entity) else {
                    return Vec::new();
                };
                if self.state.entities.is_active(idx) {
                    vec![*entity]
                } else {
                    Vec::new()
                }
            }
            game_core::encounter::Target::TopThreat => {
                if let Some(top) = self
                    .state
                    .combat
                    .threat_tables
                    .get(boss_idx)
                    .and_then(|table| table.top_threat())
                {
                    vec![top]
                } else {
                    Vec::new()
                }
            }
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
                let store = if let Some(s) = self.volumes.get(&boss_id) {
                    s
                } else {
                    return Vec::new();
                };
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
            game_core::encounter::Target::BossOffset { .. }
            | game_core::encounter::Target::FixedPoint { .. } => Vec::new(),
        }
    }

    fn resolve_encounter_targetings(
        &self,
        caster_id: EntityId,
        caster_idx: EntityIndex,
        ability_id: u32,
        target: &game_core::encounter::Target,
    ) -> Vec<EncounterTargeting> {
        let Some(ability) = self.abilities.get(ability_id) else {
            return Vec::new();
        };
        let max_range = ability
            .max_range
            .unwrap_or(game_core::physics_constants::DEFAULT_ABILITY_MAX_RANGE);

        match ability.targeting_mode {
            TargetingMode::EntityTarget | TargetingMode::AimAssist => self
                .resolve_encounter_targets(caster_id, caster_idx, target)
                .into_iter()
                .filter(|entity| self.valid_entity_target(caster_id, *entity, max_range, true))
                .map(EncounterTargeting::Entity)
                .collect(),
            TargetingMode::LockOn { .. } => {
                let targets: Vec<EntityId> = self
                    .resolve_encounter_targets(caster_id, caster_idx, target)
                    .into_iter()
                    .filter(|entity| self.valid_entity_target(caster_id, *entity, max_range, true))
                    .collect();
                if targets.is_empty() {
                    Vec::new()
                } else {
                    vec![EncounterTargeting::MultiLockOn(targets)]
                }
            }
            TargetingMode::GroundTarget => self
                .resolve_encounter_positions(caster_id, caster_idx, target)
                .into_iter()
                .filter_map(|point| self.resolve_ground_target_point(caster_id, point, max_range))
                .map(EncounterTargeting::Position)
                .collect(),
            TargetingMode::CasterOffset => {
                if self.physics.get_transform(caster_id).is_some() {
                    vec![EncounterTargeting::CasterOffset]
                } else {
                    Vec::new()
                }
            }
            TargetingMode::SelfOnly => vec![EncounterTargeting::SelfCast],
            TargetingMode::DirectionTarget | TargetingMode::RaycastStrict => {
                let caster_pos = match self.physics.get_transform(caster_id) {
                    Some(t) => t.position,
                    None => return Vec::new(),
                };
                self.resolve_encounter_positions(caster_id, caster_idx, target)
                    .into_iter()
                    .filter(|point| {
                        let dx = point.x - caster_pos.x;
                        let dy = point.y - caster_pos.y;
                        let dz = point.z - caster_pos.z;
                        dx * dx + dy * dy + dz * dz <= max_range * max_range
                            && self.physics.line_of_sight_on_layer(
                                caster_pos,
                                *point,
                                self.layer_of(caster_id),
                            )
                    })
                    .filter_map(|point| Self::direction_to(caster_pos, point))
                    .map(EncounterTargeting::Direction)
                    .collect()
            }
        }
    }

    fn resolve_ai_ability_targeting(
        &self,
        npc_id: EntityId,
        _npc_idx: EntityIndex,
        target_id: EntityId,
        ability_id: u32,
    ) -> Option<ResolvedTargeting> {
        if !self.ability_ready_at(npc_id, ability_id, self.current_tick) {
            return None;
        }
        let ability = self.abilities.get(ability_id)?;
        let max_range = ability
            .max_range
            .unwrap_or(game_core::physics_constants::DEFAULT_ABILITY_MAX_RANGE);
        if !matches!(ability.targeting_mode, TargetingMode::SelfOnly)
            && !self.valid_entity_target(npc_id, target_id, max_range, true)
        {
            return None;
        }

        match ability.targeting_mode {
            TargetingMode::EntityTarget | TargetingMode::AimAssist => {
                Some(ResolvedTargeting::Entity { target: target_id })
            }
            TargetingMode::LockOn { .. } => Some(ResolvedTargeting::MultiLockOn {
                targets: vec![target_id],
            }),
            TargetingMode::GroundTarget => self
                .physics
                .get_transform(target_id)
                .and_then(|target| {
                    self.resolve_ground_target_point(npc_id, target.position, max_range)
                })
                .map(|point| ResolvedTargeting::Position { point }),
            TargetingMode::CasterOffset => self
                .physics
                .get_transform(npc_id)
                .map(|_| ResolvedTargeting::CasterOffset),
            TargetingMode::SelfOnly => Some(ResolvedTargeting::SelfCast),
            TargetingMode::DirectionTarget | TargetingMode::RaycastStrict => {
                let caster_pos = self.physics.get_transform(npc_id)?.position;
                let target_pos = self.physics.get_transform(target_id)?.position;
                Self::direction_to(caster_pos, target_pos)
                    .map(|dir| ResolvedTargeting::Direction { dir })
            }
        }
    }

    fn resolve_encounter_positions(
        &self,
        boss_id: EntityId,
        boss_idx: EntityIndex,
        target: &game_core::encounter::Target,
    ) -> Vec<Vec3f> {
        match target {
            game_core::encounter::Target::BossOffset { offset } => self
                .boss_offset_position(boss_id, Vec3f::new(offset[0], offset[1], offset[2]))
                .into_iter()
                .collect(),
            game_core::encounter::Target::FixedPoint { position } => {
                vec![Vec3f::new(position[0], position[1], position[2])]
            }
            _ => self
                .resolve_encounter_targets(boss_id, boss_idx, target)
                .into_iter()
                .filter_map(|entity| self.physics.get_transform(entity).map(|t| t.position))
                .collect(),
        }
    }

    fn valid_entity_target(
        &self,
        caster_id: EntityId,
        target_id: EntityId,
        max_range: f32,
        require_los: bool,
    ) -> bool {
        if target_id == caster_id || !self.same_layer(caster_id, target_id) {
            return false;
        }
        let Some(target_idx) = self.state.entities.lookup(target_id) else {
            return false;
        };
        if !self.state.entities.is_active(target_idx) {
            return false;
        }
        let (Some(caster_t), Some(target_t)) = (
            self.physics.get_transform(caster_id),
            self.physics.get_transform(target_id),
        ) else {
            return false;
        };
        let dx = target_t.position.x - caster_t.position.x;
        let dy = target_t.position.y - caster_t.position.y;
        let dz = target_t.position.z - caster_t.position.z;
        if dx * dx + dy * dy + dz * dz > max_range * max_range {
            return false;
        }
        !require_los
            || self.physics.line_of_sight_on_layer(
                caster_t.position,
                target_t.position,
                self.layer_of(caster_id),
            )
    }

    fn resolve_ground_target_point(
        &self,
        caster_id: EntityId,
        point: Vec3f,
        max_range: f32,
    ) -> Option<Vec3f> {
        let caster_pos = self.physics.get_transform(caster_id)?.position;
        let dx = point.x - caster_pos.x;
        let dz = point.z - caster_pos.z;
        if dx * dx + dz * dz > max_range * max_range {
            return None;
        }
        const SKY_LIFT: f32 = 200.0;
        const MAX_DROP: f32 = 400.0;
        let layer = self.layer_of(caster_id);
        let resolved = self.physics.raycast_surface(
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
        )?;
        if !self
            .physics
            .line_of_sight_on_layer(caster_pos, resolved, layer)
        {
            return None;
        }
        Some(resolved)
    }

    fn boss_offset_position(&self, boss_id: EntityId, offset: Vec3f) -> Option<Vec3f> {
        let transform = self.physics.get_transform(boss_id)?;
        let facing = Self::normalize_direction(Self::forward_from_rotation(transform.rotation))
            .unwrap_or(Vec3f {
                x: 0.0,
                y: 0.0,
                z: 1.0,
            });
        Some(Vec3f {
            x: transform.position.x + facing.x * offset.z + offset.x,
            y: transform.position.y + offset.y,
            z: transform.position.z + facing.z * offset.z,
        })
    }

    fn area_telegraph_data(
        &self,
        caster_id: EntityId,
        ability_id: u32,
        targeting: &EncounterTargeting,
    ) -> Option<(Vec3f, f32, String)> {
        let ability = self.abilities.get(ability_id)?;
        let sensor = skill_shape_to_sensor(ability.shape);
        let radius = match sensor {
            SensorShape::Sphere { radius } | SensorShape::Capsule { radius, .. } => radius,
        };
        let position = match targeting {
            EncounterTargeting::Position(point) => *point,
            EncounterTargeting::CasterOffset => {
                let offset = self.first_hitbox_offset(ability_id).unwrap_or(Vec3f::ZERO);
                self.boss_offset_position(caster_id, offset)?
            }
            EncounterTargeting::SelfCast => self.physics.get_transform(caster_id)?.position,
            EncounterTargeting::Direction(dir) => {
                let origin = self.physics.get_transform(caster_id)?.position;
                Vec3f {
                    x: origin.x + dir.x * radius,
                    y: origin.y,
                    z: origin.z + dir.z * radius,
                }
            }
            EncounterTargeting::Entity(_) | EncounterTargeting::MultiLockOn(_) => return None,
        };
        Some((position, radius, format!("{:?}", ability.shape)))
    }

    fn first_hitbox_offset(&self, ability_id: u32) -> Option<Vec3f> {
        self.abilities
            .get_timeline(ability_id)?
            .actions
            .iter()
            .find_map(|scheduled| match &scheduled.action {
                AbilityAction::SpawnHitbox { offset, .. }
                | AbilityAction::SpawnConfiguredHitbox { offset, .. } => Some(*offset),
                _ => None,
            })
    }

    fn to_resolved_targeting(targeting: EncounterTargeting) -> ResolvedTargeting {
        match targeting {
            EncounterTargeting::Entity(target) => ResolvedTargeting::Entity { target },
            EncounterTargeting::Position(point) => ResolvedTargeting::Position { point },
            EncounterTargeting::Direction(dir) => ResolvedTargeting::Direction { dir },
            EncounterTargeting::SelfCast => ResolvedTargeting::SelfCast,
            EncounterTargeting::CasterOffset => ResolvedTargeting::CasterOffset,
            EncounterTargeting::MultiLockOn(targets) => ResolvedTargeting::MultiLockOn { targets },
        }
    }

    pub(super) fn encounter_arena_activated(
        &self,
        boss_id: EntityId,
        boss_idx: EntityIndex,
    ) -> bool {
        let boss_layer = self.layer_of_idx(boss_idx);
        let radius = self
            .state
            .ai
            .npc_aggro_radius
            .get(boss_idx)
            .copied()
            .filter(|r| *r > 0.0)
            .unwrap_or(20.0);
        let Some(boss_pos) = self.physics.get_transform(boss_id).map(|t| t.position) else {
            return false;
        };
        let radius_sq = radius * radius;
        // Threat-table reactivation: an existing same-layer threat
        // source wakes the encounter. We deliberately do NOT apply
        // the aggro-radius distance gate here — once a boss has been
        // pulled and its threat table contains entries, ranged threat
        // beyond the aggro radius (e.g. a ranged DPS kiting at long
        // range) must still keep the encounter active. The same-layer
        // check is enough to prevent stale cross-layer threat entries
        // from forcing reactivation. The fallback player-scan branch
        // applies the distance gate for the cold-start case where no
        // threat table exists yet.
        if self
            .state
            .combat
            .threat_tables
            .get(boss_idx)
            .is_some_and(|table| {
                table
                    .entries
                    .iter()
                    .any(|entry| self.ai_target_is_valid(boss_idx, entry.source))
            })
        {
            return true;
        }
        self.state
            .active_indices_of_kind(EntityKind::Player)
            .into_iter()
            .any(|player_idx| {
                if self.layer_of_idx(player_idx) != boss_layer {
                    return false;
                }
                if !self.ai_can_perceive(boss_idx, player_idx) {
                    return false;
                }
                let player_id = self.state.entities.id_of(player_idx);
                self.physics.get_transform(player_id).is_some_and(|t| {
                    let dx = t.position.x - boss_pos.x;
                    let dz = t.position.z - boss_pos.z;
                    dx * dx + dz * dz <= radius_sq
                })
            })
    }

    pub(super) fn seed_encounter_activation_threat(
        &mut self,
        boss_id: EntityId,
        boss_idx: EntityIndex,
    ) {
        use game_core::combat::status::{ThreatEntry, ThreatTable};
        use game_core::entity::lifecycle::NpcAiState;

        let table_has_threat = self
            .state
            .combat
            .threat_tables
            .get(boss_idx)
            .is_some_and(|table| table.top_threat().is_some());
        if table_has_threat {
            return;
        }
        let boss_layer = self.layer_of_idx(boss_idx);
        let radius = self
            .state
            .ai
            .npc_aggro_radius
            .get(boss_idx)
            .copied()
            .filter(|r| *r > 0.0)
            .unwrap_or(20.0);
        let Some(boss_pos) = self.physics.get_transform(boss_id).map(|t| t.position) else {
            return;
        };
        let radius_sq = radius * radius;
        let mut nearest: Option<(EntityId, f32)> = None;
        for player_idx in self.state.active_indices_of_kind(EntityKind::Player) {
            if self.layer_of_idx(player_idx) != boss_layer {
                continue;
            }
            if !self.ai_can_perceive(boss_idx, player_idx) {
                continue;
            }
            let player_id = self.state.entities.id_of(player_idx);
            if let Some(t) = self.physics.get_transform(player_id) {
                let dx = t.position.x - boss_pos.x;
                let dz = t.position.z - boss_pos.z;
                let dist_sq = dx * dx + dz * dz;
                if dist_sq <= radius_sq && nearest.map_or(true, |(_, best)| dist_sq < best) {
                    nearest = Some((player_id, dist_sq));
                }
            }
        }
        if let Some((target, _)) = nearest {
            if !self.state.combat.threat_tables.contains(boss_idx) {
                self.state
                    .combat
                    .threat_tables
                    .insert(boss_idx, ThreatTable::default());
            }
            if let Some(table) = self.state.combat.threat_tables.get_mut(boss_idx) {
                table.entries.push(ThreatEntry {
                    source: target,
                    threat: 1.0,
                });
            }
            if let Some(ai) = self.state.ai.npc_ai.get_mut(boss_idx) {
                *ai = NpcAiState::Combat;
            }
        }
    }

    pub(super) fn apply_set_interactable_state(
        &mut self,
        boss_id: EntityId,
        selector: &game_core::encounter::InteractableSelector,
        state: game_core::encounter::InteractableStateValue,
    ) {
        let sim_state = Self::encounter_interactable_state_to_sim(state);
        for target in self.resolve_interactable_selector(boss_id, selector) {
            self.set_interactable_state_runtime(target, sim_state);
        }
    }

    pub(super) fn apply_toggle_interactable(
        &mut self,
        boss_id: EntityId,
        selector: &game_core::encounter::InteractableSelector,
    ) {
        for target in self.resolve_interactable_selector(boss_id, selector) {
            self.toggle_interactable_state_runtime(target);
        }
    }

    fn resolve_interactable_selector(
        &self,
        boss_id: EntityId,
        selector: &game_core::encounter::InteractableSelector,
    ) -> Vec<EntityId> {
        let boss_layer = self.layer_of(boss_id);
        let mut out: Vec<EntityId> = self
            .state
            .interactables
            .iter()
            .filter_map(|(entity, info)| {
                if self.layer_of(*entity) != boss_layer {
                    return None;
                }
                let matches = match selector {
                    game_core::encounter::InteractableSelector::ScriptId { script_id } => {
                        info.script_id.as_deref() == Some(script_id.as_str())
                    }
                    game_core::encounter::InteractableSelector::Tag { tag } => {
                        info.tags.iter().any(|t| t == tag)
                    }
                };
                matches.then_some(*entity)
            })
            .collect();
        out.sort_by_key(|id| id.0);
        out
    }

    fn encounter_interactable_state_to_sim(
        state: game_core::encounter::InteractableStateValue,
    ) -> game_core::sim_state::SimInteractState {
        match state {
            game_core::encounter::InteractableStateValue::Idle => {
                game_core::sim_state::SimInteractState::Idle
            }
            game_core::encounter::InteractableStateValue::Active => {
                game_core::sim_state::SimInteractState::Active
            }
            game_core::encounter::InteractableStateValue::Cooldown => {
                game_core::sim_state::SimInteractState::Cooldown
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
