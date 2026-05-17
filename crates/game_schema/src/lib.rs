//! Shared type definitions used by both the SpacetimeDB WASM module
//! (`server_module`) and native game crates (`game_protocol`, `game_core`).
//!
//! This crate eliminates type duplication across the WASM boundary.
//!
//! - Native crates depend on `game_schema` (serde derives only).
//! - `server_module` depends on `game_schema` with `features = ["spacetimedb"]`
//!   to additionally derive `SpacetimeType`.

pub mod types;
pub mod entity;
pub mod damage;
pub mod intent;

pub use types::Vec3f;
pub use entity::{EntityKind, EntityState, NpcAiState};
pub use damage::DamageType;
pub use intent::{IntentAction, AbilityTarget, MoveDir, UseAbilityData};
