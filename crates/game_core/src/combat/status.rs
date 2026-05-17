use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use serde::{Deserialize, Serialize};

/// Active buff/debuff on an entity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveBuff {
    pub buff_id: u32,
    pub source: EntityId,
    pub target: EntityId,
    pub stacks: u32,
    pub max_stacks: u32,
    /// Tick when this buff expires. None = permanent until removed.
    pub expires_at: Option<TickId>,
}

/// Aggro entry for a single source on a single NPC.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreatEntry {
    pub source: EntityId,
    pub threat: f32,
}

/// Aggro table for an NPC — threat-based targeting.
///
/// threat += damage
/// threat += taunts
/// threat -= decay_over_time
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ThreatTable {
    pub entries: Vec<ThreatEntry>,
}

impl ThreatTable {
    pub fn add_threat(&mut self, source: EntityId, amount: f32) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.source == source) {
            entry.threat += amount;
        } else {
            self.entries.push(ThreatEntry { source, threat: amount });
        }
    }

    /// Decay all threat values by a multiplicative factor (e.g. 0.98 per tick).
    /// Entries that fall below the noise floor are removed to keep the table compact.
    pub fn decay(&mut self, factor: f32) {
        for entry in &mut self.entries {
            entry.threat *= factor;
        }
        self.entries.retain(|e| e.threat > 0.001);
    }

    pub fn top_threat(&self) -> Option<EntityId> {
        self.entries
            .iter()
            .max_by(|a, b| a.threat.total_cmp(&b.threat))
            .map(|e| e.source)
    }
}
