use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use serde::{Deserialize, Serialize};

/// AI behavior override carried by a buff.
///
/// Phase 7 checks this before running standard AI decision logic. A buff
/// with an `ai_override` field set takes precedence over the entity's normal
/// `NpcAiState` transitions so the override is data, not special-cased logic.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiOverride {
    /// Force the NPC into flee behavior regardless of threat table.
    ForceFlee,
    /// Freeze the NPC in place (suppresses all AI actions).
    ForceIdle,
    /// Force the NPC to focus on a specific target (fear, taunt, etc.).
    ForceFocus { target: EntityId },
}

/// Structured per-phase modifier fields on an active buff.
///
/// Each phase reads its relevant field at the appropriate point in the pipeline:
/// - `damage_out_pct` / `damage_in_pct`: Phase 6 combat resolution (applied to damage value).
/// - `speed_pct`: Phase 2 controller (multiplied into movement speed before position update).
/// - `ai_override`: Phase 7 AI decisions (checked before standard NpcAiState transitions).
/// - `cooldown_reduce_pct`: Phase 3 CooldownStart (shortens cooldown duration, clamped to 99%).
///
/// All fields are `Option` — `None` means the buff does not affect that domain.
/// Positive `damage_out_pct` amplifies (e.g. 0.1 = +10%), negative reduces.
/// `damage_in_pct` follows the same sign convention from the *receiver’s* perspective.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuffModifiers {
    /// Outgoing damage multiplier delta. Applied in Phase 6 to damage the caster deals.
    pub damage_out_pct: Option<f32>,
    /// Incoming damage multiplier delta. Applied in Phase 6 to damage the target receives.
    pub damage_in_pct: Option<f32>,
    /// Cooldown reduction percentage. Applied in Phase 3 when `CooldownStart` computes `ready_at`.\n    /// Positive values shorten cooldown (e.g. 0.2 = 20% reduction). Clamped to [0, 0.99).
    pub cooldown_reduce_pct: Option<f32>,
    /// Movement speed multiplier delta. Applied in Phase 2 to the entity’s base speed.
    pub speed_pct: Option<f32>,
    /// AI behavior override. Phase 7 checks this before running standard AI decisions.
    pub ai_override: Option<AiOverride>,
}

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
    /// Structured per-phase modifier fields. Default = no modifiers (passive buff).
    #[serde(default)]
    pub modifiers: BuffModifiers,
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
