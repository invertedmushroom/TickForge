//! Shared type definitions used by both the SpacetimeDB WASM module
//! (`server_module`) and native game crates (`game_protocol`, `game_core`).
//!
//! This crate eliminates type duplication across the WASM boundary.
//!
//! - Native crates depend on `game_schema` (serde derives only).
//! - `server_module` depends on `game_schema` with `features = ["spacetimedb"]`
//!   to additionally derive `SpacetimeType`.

pub mod cc;
pub mod damage;
pub mod dungeon;
pub mod entity;
pub mod equipment;
pub mod intent;
pub mod types;

pub use cc::CCEffect;
pub use damage::DamageType;
pub use dungeon::LayerCollisionPolicy;
pub use entity::{EntityKind, EntityState, NpcAiState};
pub use equipment::EquipmentSlot;
pub use intent::{AbilityTarget, BlockData, IntentAction, MoveDir, UseAbilityData};
pub use types::Vec3f;
