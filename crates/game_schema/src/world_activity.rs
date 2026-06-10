//! World Activity Event schema (messaging spine).
//!
//! Lives in `game_schema` so the simulation worker's director (in
//! `game_core`) can match against the same state enum the server
//! reducer writes. The full row type lives in `server_module::tables`
//! because tables are server-only; this module defines just the shared
//! state enum and helper key types.
//!
//! See `docs/contracts/world_activity_policy_contract.md` and step 4 of
//! the 2026-06-09 messaging-spine review.

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
/// `upsert_world_activity_event`) and the worker (lookup key in the
/// projected `world_activity_events` map). Open-world events use
/// `scope_layer = 0` with the real `(rx, rz)`; instance events use
/// `scope_layer = instance.layer` with `(rx, rz) = (0, 0)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorldActivityEventKey {
    pub scope_layer: u32,
    pub scope_region_x: i32,
    pub scope_region_z: i32,
    pub tag: String,
}
