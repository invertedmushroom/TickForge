use std::collections::HashMap;

use game_protocol::entity_id::EntityId;
use game_protocol::event::DamageType;
use game_protocol::tick::TickId;
use serde::{Deserialize, Serialize};

// ── Buff categorization ─────────────────────────────────────────

/// Whether a buff is a beneficial effect (Boon) or a harmful one (Condition).
///
/// Cleanse removes Conditions; Dispel removes Boons from enemies.
/// Default is `Boon` for backwards compatibility with existing buff definitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuffKind {
    Boon,
    Condition,
}

impl Default for BuffKind {
    fn default() -> Self {
        BuffKind::Boon
    }
}

/// Who is allowed to remove a buff before natural expiry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuffRemovalPolicy {
    /// Normal buffs can be removed by cleanse, stunbreak, or explicit effects.
    #[default]
    Normal,
    /// Encounter/mechanic-owned buffs. Counterplay actions skip these; the
    /// owning mechanic must remove them explicitly.
    MechanicLocked,
}

// Re-export from game_schema so all existing `status::CCEffect` imports keep working.
pub use game_schema::CCEffect;

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
    /// Root: prevents all movement. Checked in Phase 2 (player) and Phase 7 (NPC).
    pub root: Option<bool>,
    /// Stability: absorbs one incoming CC application per buff stack.
    /// When CC would be applied (Phase 6), if the target has any buff with
    /// `stability: Some(true)`, one stack is consumed and the CC is negated.
    pub stability: Option<bool>,
    /// CC effect type this buff represents (Stun, Knockdown, etc.).
    /// Present on Condition debuffs that track a CC timer, enabling
    /// cleanse/dispel to find and remove the matching debuff.
    pub cc_effect: Option<CCEffect>,
    /// CC duration reduction percentage. Summed with equipment into
    /// `StatBlock::cc_duration_reduce`, clamped to [0.0, 0.75].
    pub cc_duration_reduce_pct: Option<f32>,
    /// Damage per tick for damage-over-time effects (poison, burn, bleed).
    /// When present, Phase 8b applies this damage at `dot_interval_ticks` intervals.
    pub dot_damage: Option<f32>,
    /// Tick interval between DoT damage applications. Defaults to 20 (~1s at 20 Hz).
    /// Only meaningful when `dot_damage` is `Some`.
    pub dot_interval_ticks: Option<u32>,
    /// Damage type for DoT ticks. Defaults to `Physical` if unset.
    pub dot_damage_type: Option<DamageType>,
    /// Stealth: hides entity from enemy teams in the nearby_transforms view.
    /// Allied entities (same team_id) still see the stealthed entity.
    pub stealth: Option<bool>,
}

/// Data-only buff definition for ability timelines and on-hit effects.
///
/// Contains everything needed to create an `ActiveBuff` except the runtime
/// source/target entities, which are filled in by the pipeline at application time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuffTemplate {
    pub buff_id: u32,
    #[serde(default)]
    pub name: String,
    /// Whether this buff is a Boon or Condition. Defaults to Boon.
    #[serde(default)]
    pub buff_kind: BuffKind,
    /// Duration in ticks. `None` = permanent until explicitly removed.
    pub duration_ticks: Option<u32>,
    pub max_stacks: u32,
    #[serde(default)]
    pub removal_policy: BuffRemovalPolicy,
    #[serde(default)]
    pub modifiers: BuffModifiers,
}

/// Registry of all known buff templates, keyed by buff_id.
///
/// Loaded from `data/buffs.ron` at startup. Abilities reference buffs by ID
/// (`on_hit_buffs: Vec<u32>`, `ApplyBuff { buff_id }`) and the pipeline looks
/// up the full template here at application time.
#[derive(Clone, Debug, Default)]
pub struct BuffRegistry {
    buffs: HashMap<u32, BuffTemplate>,
}

impl BuffRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, template: BuffTemplate) {
        self.buffs.insert(template.buff_id, template);
    }

    pub fn get(&self, buff_id: u32) -> Option<&BuffTemplate> {
        self.buffs.get(&buff_id)
    }
}

/// Active buff/debuff on an entity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveBuff {
    pub buff_id: u32,
    pub source: EntityId,
    pub target: EntityId,
    /// Whether this buff is a Boon or Condition. Copied from template.
    #[serde(default)]
    pub buff_kind: BuffKind,
    pub stacks: u32,
    pub max_stacks: u32,
    /// Tick when this buff expires. None = permanent until removed.
    pub expires_at: Option<TickId>,
    /// Structured per-phase modifier fields. Default = no modifiers (passive buff).
    #[serde(default)]
    pub modifiers: BuffModifiers,
    /// Tick when DoT damage was last applied. Initialised to the application tick
    /// so the first pulse fires after `dot_interval_ticks` elapse.
    #[serde(default)]
    pub last_dot_tick: Option<TickId>,
}

impl ActiveBuff {
    /// Create an `ActiveBuff` from a template, filling in runtime context.
    pub fn from_template(
        template: &BuffTemplate,
        source: EntityId,
        target: EntityId,
        current_tick: TickId,
    ) -> Self {
        Self {
            buff_id: template.buff_id,
            source,
            target,
            buff_kind: template.buff_kind,
            stacks: 1,
            max_stacks: template.max_stacks,
            expires_at: template
                .duration_ticks
                .map(|d| TickId(current_tick.0 + d as u64)),
            modifiers: template.modifiers,
            last_dot_tick: if template.modifiers.dot_damage.is_some() {
                Some(current_tick)
            } else {
                None
            },
        }
    }
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

pub const DEFAULT_THREAT_SWITCH_ADVANTAGE: f32 = 0.15;

impl ThreatTable {
    pub fn add_threat(&mut self, source: EntityId, amount: f32) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.source == source) {
            entry.threat += amount;
        } else {
            self.entries.push(ThreatEntry {
                source,
                threat: amount,
            });
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

    pub fn top_threat_with_value(&self) -> Option<(EntityId, f32)> {
        self.entries
            .iter()
            .filter(|entry| threat_value_is_targetable(entry.threat))
            .max_by(|a, b| a.threat.total_cmp(&b.threat))
            .map(|entry| (entry.source, entry.threat))
    }

    pub fn threat_of(&self, source: EntityId) -> Option<f32> {
        self.entries
            .iter()
            .find(|entry| entry.source == source)
            .map(|entry| entry.threat)
            .filter(|threat| threat_value_is_targetable(*threat))
    }

    /// Select a target with stickiness for the current target.
    ///
    /// `switch_advantage` is the fractional lead a challenger needs over the
    /// current target before the target changes. `0.15` means "switch only when
    /// the challenger has more than 15% extra threat".
    pub fn select_target_with_hysteresis(
        &self,
        current_target: Option<EntityId>,
        switch_advantage: f32,
    ) -> Option<EntityId> {
        let (top_target, top_threat) = self.top_threat_with_value()?;
        let Some(current_target) = current_target else {
            return Some(top_target);
        };
        if current_target == top_target {
            return Some(current_target);
        }
        let Some(current_threat) = self.threat_of(current_target) else {
            return Some(top_target);
        };

        let advantage = switch_advantage.max(0.0);
        if top_threat > current_threat * (1.0 + advantage) {
            Some(top_target)
        } else {
            Some(current_target)
        }
    }
}

fn threat_value_is_targetable(threat: f32) -> bool {
    threat.is_finite() && threat > 0.001
}

/// On-disk serialization format for `data/buffs.ron`.
#[derive(Clone, Debug, Deserialize)]
pub struct BuffFile {
    pub buffs: Vec<BuffTemplate>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shipped_buffs_ron() {
        let file: BuffFile =
            ron::from_str(include_str!("../../../../data/buffs.ron")).expect("buffs parse");
        assert!(
            file.buffs.iter().any(|buff| buff.buff_id == 802),
            "Manaya boss-lock buff should stay authored in data/buffs.ron"
        );
    }

    #[test]
    fn threat_hysteresis_keeps_current_target_when_challenger_is_close() {
        let current = EntityId(1);
        let challenger = EntityId(2);
        let mut table = ThreatTable::default();
        table.add_threat(current, 100.0);
        table.add_threat(challenger, 110.0);

        assert_eq!(
            table.select_target_with_hysteresis(Some(current), DEFAULT_THREAT_SWITCH_ADVANTAGE),
            Some(current)
        );
    }

    #[test]
    fn threat_hysteresis_switches_when_challenger_leads_enough() {
        let current = EntityId(1);
        let challenger = EntityId(2);
        let mut table = ThreatTable::default();
        table.add_threat(current, 100.0);
        table.add_threat(challenger, 116.0);

        assert_eq!(
            table.select_target_with_hysteresis(Some(current), DEFAULT_THREAT_SWITCH_ADVANTAGE),
            Some(challenger)
        );
    }

    #[test]
    fn threat_hysteresis_ignores_invalid_current_target() {
        let current = EntityId(1);
        let challenger = EntityId(2);
        let mut table = ThreatTable::default();
        table.add_threat(challenger, 10.0);

        assert_eq!(
            table.select_target_with_hysteresis(Some(current), DEFAULT_THREAT_SWITCH_ADVANTAGE),
            Some(challenger)
        );
    }

    #[test]
    fn threat_hysteresis_ignores_non_finite_and_non_positive_values() {
        let invalid_nan = EntityId(1);
        let invalid_zero = EntityId(2);
        let valid = EntityId(3);
        let mut table = ThreatTable::default();
        table.add_threat(invalid_nan, f32::NAN);
        table.add_threat(invalid_zero, 0.0);
        table.add_threat(valid, 1.0);

        assert_eq!(
            table.select_target_with_hysteresis(Some(invalid_nan), DEFAULT_THREAT_SWITCH_ADVANTAGE),
            Some(valid)
        );
    }
}
