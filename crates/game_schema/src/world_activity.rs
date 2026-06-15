//! World Activity Event schema (messaging spine).
//!
//! Lives in `game_schema` so the simulation worker's director (in
//! `game_core`) can match against the same state enum the server
//! reducer writes. The full row type lives in `server_module::tables`
//! because tables are server-only; this module defines just the shared
//! state enum and helper key types.
//!
//! See `docs/contracts/world_activity_policy_contract.md`.

use serde::{Deserialize, Serialize};

/// Lifecycle state of a `world_activity_event` row.
///
/// Mirrored from the server table by the simulation worker's
/// `world_activity_event.on_insert` / `on_update` subscription.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub enum WorldActivityEventState {
    /// Authored but not yet activated (e.g. waiting on its trigger
    /// condition). Reserved for future timer/escalation events.
    Pending,
    /// Currently live. Director triggers gated on this state may fire
    /// when player-presence requirements are also met.
    Active,
    /// Goal resolved successfully. Director triggers do not fire on
    /// Completed rows.
    Completed,
    /// Event lifetime ended (timeout, cleanup, instance expiry).
    /// Director triggers do not fire on Expired rows.
    Expired,
}

/// Scope+tag key used by both the server (logical-uniqueness check in
/// `upsert_world_activity_event` / `upsert_activity_scope`) and the worker
/// (lookup key in the projected `activity_scopes` map). Open-world events use
/// `scope_layer = 0` with the real `(rx, rz)`; instance events use
/// `scope_layer = instance.layer` with `(rx, rz) = (0, 0)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorldActivityEventKey {
    pub scope_layer: u32,
    pub scope_region_x: i32,
    pub scope_region_z: i32,
    pub tag: String,
}

/// Simulation-mode axis of an `activity_scope` row.
///
/// This is the *second* axis the activity-scope contract requires, kept
/// as a distinct column from the durable activity `state`
/// (`WorldActivityEventState`): `state` answers "what has this scope
/// achieved" while `mode` answers "is the worker ticking it right now".
///
/// New progression rows start `Awake`; trusted worker reducers request
/// wake/drain transitions, `world_clock` advances drained scopes to
/// `Sleeping`, and instance expiry deletes stale scope rows directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "spacetimedb", derive(spacetimedb::SpacetimeType))]
pub enum ActivityScopeMode {
    /// Tier 1 (physics/AI/combat/pathing) runs for this scope.
    Awake,
    /// Bounded settle window before sleep: the scope keeps ticking long enough
    /// for in-flight volatile work to settle, while director triggers requiring
    /// `Awake` stop firing new actor spawns.
    Draining,
    /// Dormant: Tier 1 work is skipped; Tier 2 durable timers still
    /// advance.
    Sleeping,
    /// Reserved terminal teardown mode; current instance expiry deletes stale
    /// scope rows directly.
    Cleanup,
}

/// Worker-side / director-side projection of an `activity_scope` row.
///
/// Assembled by the simulation worker from the subscribed
/// `activity_scope` table and read by
/// `DirectorTrigger::WorldActivityEventActive`. It carries the two
/// independent presence floors that must be AND-ed: the scope's authored
/// `required_players` (event liveness) is enforced alongside the spawn rule's
/// `min_players` escalation tier, never instead of it.
///
/// This is a runtime struct, not a DB row type, so it derives serde
/// only (no `SpacetimeType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ActivityScopeProjection {
    /// Durable activity axis, mirrored from the row.
    pub state: WorldActivityEventState,
    /// Simulation-mode axis, mirrored from the row.
    pub mode: ActivityScopeMode,
    /// Authored scope-liveness floor. AND-ed with the rule's `min_players`.
    pub required_players: u32,
}
