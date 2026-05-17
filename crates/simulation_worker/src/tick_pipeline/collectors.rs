use super::*;

/// Squared positional tolerance (metres²) for transform delta filtering.
///
/// Entities whose position moved less than ~2 cm since the last committed
/// transform — and whose rotation is also within tolerance — are suppressed
/// from the commit payload. Physics remains authoritative; only the replicated
/// payload is delta-compressed. The bound is inherent: the client is always
/// within `sqrt(TRANSFORM_POS_EPS_SQ)` of authoritative position, because the
/// comparison is against the last *committed* value rather than the previous
/// tick. Accumulated sub-epsilon motion trips the emit once it exceeds the
/// bound.
const TRANSFORM_POS_EPS_SQ: f32 = 0.02 * 0.02;

/// Rotation tolerance expressed as `1.0 - |dot(prev, cur)|`.
///
/// `1e-5` corresponds to roughly 0.25° of angular difference — well below
/// visible jitter on turning NPCs.
const TRANSFORM_ROT_EPS: f32 = 1.0e-5;

/// Squared linear-velocity threshold (m²/s²) treated as "moving" when deciding
/// whether a zero-crossing should force an emit. `0.1²` matches the typical
/// KCC standstill floor.
const TRANSFORM_VEL_EPS_SQ: f32 = 0.1 * 0.1;

#[inline]
fn vec3_dist_sq(a: &Vec3f, b: &Vec3f) -> f32 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    let dz = a.z - b.z;
    dx * dx + dy * dy + dz * dz
}

#[inline]
fn vec3_len_sq(v: &Vec3f) -> f32 {
    v.x * v.x + v.y * v.y + v.z * v.z
}

/// Returns true when `cur` differs from `prev` by more than the configured
/// epsilons on position or rotation. A `None` baseline (first commit for this
/// entity) always counts as changed.
///
/// Linear velocity is included as a zero-crossing check only: if the previous
/// committed snapshot had non-zero velocity and the current one is at rest,
/// emit so the client stops extrapolating a stale direction. Without this,
/// an NPC that halts with less than `sqrt(TRANSFORM_POS_EPS_SQ)` position
/// delta on the stopping tick would leave the client's last-known velocity
/// at its pre-stop value, causing visible slide until the next sub-epsilon
/// accumulator trip. The symmetric rest→moving edge always trips the
/// position check within a few ticks and needs no special case here.
#[inline]
fn transform_exceeds_epsilon(prev: Option<&Transform>, cur: &Transform) -> bool {
    let Some(p) = prev else { return true };
    if vec3_dist_sq(&p.position, &cur.position) > TRANSFORM_POS_EPS_SQ {
        return true;
    }
    let dot = p.rotation.x * cur.rotation.x
        + p.rotation.y * cur.rotation.y
        + p.rotation.z * cur.rotation.z
        + p.rotation.w * cur.rotation.w;
    if 1.0 - dot.abs() > TRANSFORM_ROT_EPS {
        return true;
    }
    // Moving → rest edge: prev linvel significant, cur linvel negligible.
    let prev_vel_sq = vec3_len_sq(&p.linear_velocity);
    let cur_vel_sq = vec3_len_sq(&cur.linear_velocity);
    if prev_vel_sq > TRANSFORM_VEL_EPS_SQ && cur_vel_sq <= TRANSFORM_VEL_EPS_SQ {
        return true;
    }
    false
}

impl TickPipeline {
    // ── Transform delta collection ──────────────────────────────

    /// Collect only transforms that changed since the last emitted commit payload.
    ///
    /// The physics world remains the authoritative source of current transforms,
    /// so we still snapshot all transforms for lag compensation and region checks.
    /// Only the reducer payload is delta-compressed. Comparison uses an epsilon
    /// on position and rotation so sub-threshold solver and KCC jitter does not
    /// dominate the commit payload for clustered NPCs.
    pub(super) fn collect_transform_updates(
        &mut self,
        transforms: &[(EntityId, Transform)],
    ) -> Vec<(EntityId, Transform)> {
        let mut out = Vec::new();
        out.reserve(transforms.len());

        for &(eid, tf) in transforms {
            let prev = self.last_committed_transforms.get(&eid);
            if transform_exceeds_epsilon(prev, &tf) {
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
        let damaged: HashSet<EntityId> = self
            .pending_events
            .iter()
            .filter_map(|e| match &e.payload {
                EventPayload::Damage { .. }
                | EventPayload::FallDamage { .. }
                | EventPayload::Healed { .. } => Some(e.entity_id),
                _ => None,
            })
            .collect();
        damaged
            .iter()
            .filter_map(|&eid| {
                let idx = self.state.entities.lookup(eid)?;
                let i = idx.as_usize();
                Some((
                    eid,
                    self.state.combat.health.hp[i],
                    self.state.combat.health.max_hp[i],
                ))
            })
            .collect()
    }

    // ── Runtime domain snapshots ────────────────────────────────

    /// Collect buff updates for entities whose buff arrays changed this tick.
    ///
    /// Only dirty entities are emitted. Entities with zero remaining buffs are
    /// still included so the reducer's delete-all-then-insert reliably clears
    /// stale rows when every buff on an entity expires in the same tick.
    pub(super) fn collect_buff_updates(
        &mut self,
    ) -> Vec<(EntityId, Vec<game_core::combat::status::ActiveBuff>)> {
        let dirty = self.state.status.take_dirty();
        let mut out = Vec::with_capacity(dirty.len());
        for i in &dirty {
            if self.state.entities.states[*i] == game_core::entity::lifecycle::EntityState::Removed
            {
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
    pub(super) fn collect_npc_state_updates(
        &mut self,
    ) -> Vec<(EntityId, game_schema::NpcAiState, Option<EntityId>)> {
        let mut out = Vec::new();
        for (idx, &ai_state) in self.state.ai.npc_ai.iter() {
            if self.state.entities.states[idx.as_usize()]
                == game_core::entity::lifecycle::EntityState::Removed
            {
                continue;
            }
            let eid = self.state.entities.id_of(idx);
            let target = self
                .state
                .combat
                .threat_tables
                .get(idx)
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
            let layer = self.layer_of(eid);
            let new_cell = match self.entity_regions.get(&eid) {
                Some(current) if current.layer == layer => {
                    RegionCell::from_position_with_hysteresis(pos, current)
                }
                Some(current) => {
                    // Layer changed — carry it through so the region update
                    // propagates to entity_region even if XZ didn't move.
                    let mut cell = RegionCell::from_position_with_hysteresis(pos, current);
                    cell.layer = layer;
                    cell
                }
                None => RegionCell::from_position_on_layer(pos, layer),
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
