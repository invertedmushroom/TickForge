//! Pathing budget, cache, and replan-cooldown infrastructure.
//!
//! This module owns the scheduling layer for server pathing: the per-tick
//! search budget, result cache, and per-NPC replan cooldown. A search miss must
//! fall back to existing steering, never to an unbounded per-tick search, and
//! callers go through [`PathingState::request_path`] so the hot path stays
//! capped.
//!
//! Everything is worker-local (no DB schema change). The authored graph fills
//! [`PathResult`], and the follower consumes the waypoints; this module never
//! reads DB state or mutates dense stores.
//!
//! The scheduler owns the cache key / result / outcome types so callers cannot
//! bypass the cap. Unit tests exercise the behavior directly, so `dead_code` is
//! allowed module-wide.
#![allow(dead_code)]
use super::{EntityId, TickId, Vec3f};
use std::collections::HashMap;

/// Per-tick cap on *new* path searches across all NPCs. A request that would
/// exceed this falls back to steering for the tick and retries under the
/// per-NPC cooldown. Sized as a conservative hot-path ceiling; tune with the
/// authored graph.
pub(super) const PATH_SEARCHES_PER_TICK_CAP: u32 = 32;

/// Ticks an NPC must wait between issuing new searches (20 ≈ 1 s at 20 Hz).
/// Applied on every issued search — including ones that found no path — so a
/// blocked or unreachable NPC cannot re-search every tick.
pub(super) const PATH_REPLAN_COOLDOWN_TICKS: u32 = 20;

/// Identifier of a node in the authored navigation graph. In the cache layer it
/// is only an opaque component of the key; the graph assigns real ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct PathNodeId(pub u32);

/// Movement-capability class for a path request — part of the cache key so a
/// ground actor and (eventually) a flyer never share a cached route.
///
/// The initial authoring set has exactly one class; new classes are appended
/// only when a graph actually distinguishes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum PathCapability {
    /// Standard ground locomotion through the KCC.
    Ground,
}

/// Cache key: a route is uniquely identified by its layer, the graph version it
/// was computed against, its endpoints, and the actor capability. Bumping the
/// layer's `graph_version` makes prior keys unreachable and lets
/// [`PathCache::invalidate_stale`] drop them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct PathCacheKey {
    pub layer: u32,
    pub graph_version: u64,
    pub start: PathNodeId,
    pub goal: PathNodeId,
    pub capability: PathCapability,
}

/// A resolved path: a short, bounded waypoint list plus the tokens needed to
/// detect staleness. The follower advances one waypoint per tick through
/// `move_character`; the `graph_version` / `layer` let a consumer reject a path
/// that outlived its graph or crossed layers.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PathResult {
    pub path_id: u64,
    pub layer: u32,
    pub graph_version: u64,
    pub waypoints: Vec<Vec3f>,
}

/// Per-tick search budget. Reset at the top of every `run_tick` via
/// [`PathingState::begin_tick`]; each issued search reserves one slot.
#[derive(Debug)]
struct PathBudget {
    used: u32,
    cap: u32,
}

impl PathBudget {
    fn new(cap: u32) -> Self {
        Self { used: 0, cap }
    }

    fn reset(&mut self) {
        self.used = 0;
    }

    /// Reserve one search slot. Returns `false` (and reserves nothing) once the
    /// per-tick cap is reached.
    fn try_reserve(&mut self) -> bool {
        if self.used >= self.cap {
            return false;
        }
        self.used += 1;
        true
    }
}

/// Result cache keyed by [`PathCacheKey`]. Invalidation is **version-scoped**:
/// only entries for a layer whose stored `graph_version` no longer matches are
/// dropped, so a graph edit in one layer never clears unrelated routes.
#[derive(Debug, Default)]
struct PathCache {
    entries: HashMap<PathCacheKey, PathResult>,
}

impl PathCache {
    fn get(&self, key: &PathCacheKey) -> Option<&PathResult> {
        self.entries.get(key)
    }

    fn insert(&mut self, key: PathCacheKey, result: PathResult) {
        self.entries.insert(key, result);
    }

    /// Drop every cached entry for `layer` whose stored `graph_version` differs
    /// from `current_version`. Entries on other layers, and current-version
    /// entries on this layer, are untouched. Returns the number removed.
    fn invalidate_stale(&mut self, layer: u32, current_version: u64) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|k, _| k.layer != layer || k.graph_version == current_version);
        before - self.entries.len()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Why a path request did not produce a route this tick. The caller falls back
/// to existing steering in every case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeferReason {
    /// The per-tick search cap was already spent; retry next tick.
    BudgetExhausted,
    /// This NPC searched too recently; retry after its cooldown elapses.
    OnCooldown,
    /// A search ran but the goal was unreachable.
    NoPath,
}

/// Outcome of a [`PathingState::request_path`] call.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum PathOutcome {
    /// Served from cache — no search budget or cooldown consumed.
    Cached(PathResult),
    /// A fresh search ran this tick; budget and cooldown were consumed.
    Searched(PathResult),
    /// No route this tick; the caller must fall back to steering.
    Deferred(DeferReason),
}

/// Worker-owned pathing scheduler: the per-tick search budget, the result
/// cache, and the per-NPC replan cooldown, behind one `request_path` entry
/// point so no caller can bypass the cap.
pub(crate) struct PathingState {
    budget: PathBudget,
    cache: PathCache,
    /// Per-NPC earliest tick on which a new search may be issued.
    replan_cooldowns: HashMap<EntityId, TickId>,
    cooldown_ticks: u32,
}

impl PathingState {
    pub(super) fn new(budget_cap: u32, cooldown_ticks: u32) -> Self {
        Self {
            budget: PathBudget::new(budget_cap),
            cache: PathCache::default(),
            replan_cooldowns: HashMap::new(),
            cooldown_ticks,
        }
    }

    /// Reset the per-tick search budget. Called once at the top of `run_tick`,
    /// before any phase can request a path.
    pub(super) fn begin_tick(&mut self) {
        self.budget.reset();
    }

    /// Resolve a path request through cache → cooldown → budget → search.
    ///
    /// Order matters: a cache hit is free (it consumes neither budget nor
    /// cooldown), so it is checked first. A miss is gated by this NPC's replan
    /// cooldown and then by the global per-tick cap; only if both pass does the
    /// `search` closure run. The cooldown is stamped on *every* issued search —
    /// including one that finds no path — so an unreachable goal cannot thrash
    /// the hot path. `search` is where the bounded A* plugs in.
    pub(super) fn request_path(
        &mut self,
        entity: EntityId,
        key: PathCacheKey,
        now: TickId,
        search: impl FnOnce() -> Option<PathResult>,
    ) -> PathOutcome {
        if let Some(hit) = self.cache.get(&key) {
            return PathOutcome::Cached(hit.clone());
        }

        if let Some(&next_allowed) = self.replan_cooldowns.get(&entity) {
            if now.0 < next_allowed.0 {
                return PathOutcome::Deferred(DeferReason::OnCooldown);
            }
        }

        if !self.budget.try_reserve() {
            return PathOutcome::Deferred(DeferReason::BudgetExhausted);
        }

        self.replan_cooldowns
            .insert(entity, TickId(now.0 + self.cooldown_ticks as u64));

        match search() {
            Some(result) => {
                self.cache.insert(key, result.clone());
                PathOutcome::Searched(result)
            }
            None => PathOutcome::Deferred(DeferReason::NoPath),
        }
    }

    /// Drop cached routes for `layer` that predate `current_version`.
    /// Returns the number removed.
    pub(super) fn invalidate_layer(&mut self, layer: u32, current_version: u64) -> usize {
        self.cache.invalidate_stale(layer, current_version)
    }

    /// Forget per-NPC replan cooldowns for removed entities, keeping the map in
    /// step with `force_remove_entities` like every other per-entity store.
    pub(super) fn forget_entities(
        &mut self,
        removed: &std::collections::HashSet<EntityId>,
    ) {
        if self.replan_cooldowns.is_empty() {
            return;
        }
        self.replan_cooldowns.retain(|e, _| !removed.contains(e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(layer: u32, version: u64, start: u32, goal: u32) -> PathCacheKey {
        PathCacheKey {
            layer,
            graph_version: version,
            start: PathNodeId(start),
            goal: PathNodeId(goal),
            capability: PathCapability::Ground,
        }
    }

    fn result(path_id: u64, layer: u32, version: u64) -> PathResult {
        PathResult {
            path_id,
            layer,
            graph_version: version,
            waypoints: vec![Vec3f::ZERO],
        }
    }

    /// The per-tick cap is never exceeded: once spent, further *new* searches
    /// defer (steering fallback) and `begin_tick` refills the budget.
    #[test]
    fn budget_caps_new_searches_per_tick_and_refills() {
        let mut pathing = PathingState::new(2, PATH_REPLAN_COOLDOWN_TICKS);
        let now = TickId(100);

        // Two distinct NPCs / goals each run one search → budget spent.
        let a = pathing.request_path(EntityId(1), key(0, 1, 0, 1), now, || Some(result(10, 0, 1)));
        let b = pathing.request_path(EntityId(2), key(0, 1, 0, 2), now, || Some(result(11, 0, 1)));
        assert!(matches!(a, PathOutcome::Searched(_)));
        assert!(matches!(b, PathOutcome::Searched(_)));

        // Third distinct request this tick exceeds the cap → deferred, and the
        // search closure must NOT have run.
        let mut ran = false;
        let c = pathing.request_path(EntityId(3), key(0, 1, 0, 3), now, || {
            ran = true;
            Some(result(12, 0, 1))
        });
        assert_eq!(c, PathOutcome::Deferred(DeferReason::BudgetExhausted));
        assert!(!ran, "an over-budget request must not run the search");

        // Next tick refills the budget; NPC 3 (no prior cooldown) can search.
        pathing.begin_tick();
        let d = pathing.request_path(EntityId(3), key(0, 1, 0, 3), TickId(101), || {
            Some(result(12, 0, 1))
        });
        assert!(matches!(d, PathOutcome::Searched(_)));
    }

    /// An identical request within the same `(layer, graph_version)` is a cache
    /// hit — no second search, and it does not draw from the per-tick budget.
    #[test]
    fn identical_request_is_a_cache_hit_not_a_research() {
        let mut pathing = PathingState::new(1, PATH_REPLAN_COOLDOWN_TICKS);
        let now = TickId(0);
        let k = key(0, 1, 5, 9);

        let first = pathing.request_path(EntityId(1), k, now, || Some(result(42, 0, 1)));
        assert!(matches!(first, PathOutcome::Searched(_)));

        // Budget cap is 1 and already spent — yet a repeat resolves from cache,
        // and the closure must not run.
        let mut ran = false;
        let second = pathing.request_path(EntityId(2), k, now, || {
            ran = true;
            Some(result(99, 0, 1))
        });
        assert!(
            matches!(second, PathOutcome::Cached(ref p) if p.path_id == 42),
            "a repeat must serve the cached path, got {second:?}"
        );
        assert!(!ran, "a cache hit must not run the search");
    }

    /// Bumping a layer's `graph_version` invalidates *only* that layer's stale
    /// entries; current-version entries and other layers survive.
    #[test]
    fn graph_version_bump_invalidates_only_affected_entries() {
        let mut pathing = PathingState::new(8, PATH_REPLAN_COOLDOWN_TICKS);
        let now = TickId(0);

        // Layer 0 @ v1, layer 0 @ v2 (already migrated), layer 1 @ v1.
        pathing.request_path(EntityId(1), key(0, 1, 0, 1), now, || Some(result(1, 0, 1)));
        pathing.request_path(EntityId(2), key(0, 2, 0, 1), now, || Some(result(2, 0, 2)));
        pathing.request_path(EntityId(3), key(1, 1, 0, 1), now, || Some(result(3, 1, 1)));
        assert_eq!(pathing.cache.len(), 3);

        // Layer 0 advances to v2: the layer-0 @ v1 entry is stale and dropped;
        // the layer-0 @ v2 entry and the untouched layer-1 entry remain.
        let removed = pathing.invalidate_layer(0, 2);
        assert_eq!(removed, 1, "exactly the stale layer-0 entry is dropped");
        assert_eq!(pathing.cache.len(), 2);
        assert!(
            pathing.cache.get(&key(0, 2, 0, 1)).is_some(),
            "current-version layer-0 entry survives"
        );
        assert!(
            pathing.cache.get(&key(1, 1, 0, 1)).is_some(),
            "other-layer entry survives a layer-0 bump"
        );
        assert!(
            pathing.cache.get(&key(0, 1, 0, 1)).is_none(),
            "stale layer-0 entry is gone"
        );
    }

    /// A miss reissued before the NPC's cooldown elapses defers (steering
    /// fallback) without running a search; after the cooldown it may search
    /// again. Proven with a goal that has no cached entry so cooldown — not the
    /// cache — is the gate.
    #[test]
    fn replan_cooldown_blocks_immediate_research_on_miss() {
        let mut pathing = PathingState::new(8, 20);
        let npc = EntityId(7);

        // Tick 0: unreachable goal → search runs, finds nothing, stamps cooldown.
        let miss = pathing.request_path(npc, key(0, 1, 0, 1), TickId(0), || None);
        assert_eq!(miss, PathOutcome::Deferred(DeferReason::NoPath));

        // Tick 5 (< cooldown): same NPC, *different* goal (cache miss) → blocked
        // by cooldown, search must not run.
        pathing.begin_tick();
        let mut ran = false;
        let blocked = pathing.request_path(npc, key(0, 1, 0, 2), TickId(5), || {
            ran = true;
            Some(result(1, 0, 1))
        });
        assert_eq!(blocked, PathOutcome::Deferred(DeferReason::OnCooldown));
        assert!(!ran, "a request inside the cooldown must not search");

        // Tick 20 (== next_allowed): cooldown elapsed → search may run again.
        pathing.begin_tick();
        let allowed = pathing.request_path(npc, key(0, 1, 0, 2), TickId(20), || {
            Some(result(2, 0, 1))
        });
        assert!(matches!(allowed, PathOutcome::Searched(_)));
    }

    /// `forget_entities` keeps the cooldown map in step with entity teardown.
    #[test]
    fn forget_entities_clears_replan_cooldowns() {
        let mut pathing = PathingState::new(8, 20);
        pathing.request_path(EntityId(1), key(0, 1, 0, 1), TickId(0), || None);
        pathing.request_path(EntityId(2), key(0, 1, 0, 2), TickId(0), || None);
        assert_eq!(pathing.replan_cooldowns.len(), 2);

        let removed: std::collections::HashSet<EntityId> = [EntityId(1)].into_iter().collect();
        pathing.forget_entities(&removed);
        assert!(!pathing.replan_cooldowns.contains_key(&EntityId(1)));
        assert!(pathing.replan_cooldowns.contains_key(&EntityId(2)));
    }
}
