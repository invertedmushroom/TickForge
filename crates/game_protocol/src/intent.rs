use serde::{Deserialize, Serialize};

use crate::entity_id::EntityId;
use crate::tick::TickId;

// Shared intent types from game_schema.
pub use game_schema::{AbilityTarget, BlockData, IntentAction, MoveDir, UseAbilityData};

/// Client input bound to a specific simulation tick.
///
/// Per spec: inputs must be explicitly bound to a tick to prevent drift.
/// Reducers compute: target_tick = current_tick + 1.
/// The simulation worker only processes intents where target_tick == current_tick.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerIntent {
    /// The entity performing this intent.
    pub entity_id: EntityId,
    /// Client-assigned sequence number for reconciliation and replay protection.
    pub sequence_id: u64,
    /// The tick this intent should be processed on.
    pub target_tick: TickId,
    /// The tick the client had last rendered when the intent was created.
    /// Used for lag compensation rewind. 0 means no rewind (legacy/local client).
    pub client_observed_tick: u64,
    /// What the player wants to do.
    pub action: IntentAction,
}
