// ── Shared Types ────────────────────────────────────────────────────
//
// Types shared between server_module and native crates (game_protocol,
// game_core) live in the `game_schema` crate. server_module depends
// on game_schema with features = ["spacetimedb"] so that SpacetimeType
// is derived alongside serde traits.
//
// This eliminates the previous type duplication and prevents schema drift.
// ─────────────────────────────────────────────────────────────────────

pub mod tables;
pub mod reducers;
pub mod views;
