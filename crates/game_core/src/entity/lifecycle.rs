use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use serde::{Deserialize, Serialize};

// EntityKind, EntityState, and NpcAiState are canonical shared types from game_schema.
pub use game_schema::{EntityKind, EntityState, NpcAiState};

/// Minimal entity metadata tracked by the simulation.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct EntityRecord {
    pub id: EntityId,
    pub kind: EntityKind,
    pub state: EntityState,
    pub spawned_at: TickId,
}

impl EntityRecord {
    pub fn new(id: EntityId, kind: EntityKind, tick: TickId) -> Self {
        Self {
            id,
            kind,
            state: EntityState::Spawning,
            spawned_at: tick,
        }
    }

    pub fn is_active(&self) -> bool {
        self.state == EntityState::Active
    }
}
