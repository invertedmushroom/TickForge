use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use serde::{Deserialize, Serialize};

/// Boss encounter framework — three-layer model per spec:
/// phases (enum), triggers, and actions.
///
/// Each boss is a list of EncounterRule { trigger, condition, action }.
/// Encounter controller owns phase sequencing, one-time flags, and
/// active mechanics list.
///
/// Boss phase identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BossPhase {
    Phase1,
    Phase2,
    Phase3,
    Enrage,
    Custom(String),
}

impl BossPhase {
    /// Convert to a numeric phase for DB storage.
    pub fn to_phase_number(&self) -> u32 {
        match self {
            BossPhase::Phase1 => 1,
            BossPhase::Phase2 => 2,
            BossPhase::Phase3 => 3,
            BossPhase::Enrage => 99,
            BossPhase::Custom(_) => 100,
        }
    }
}

/// Trigger conditions for encounter rules.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EncounterTrigger {
    /// Fires once when boss HP drops below a percentage.
    OnHpBelowOnce { percent: f32 },
    /// Fires once after a timer (in ticks from phase start).
    OnTimerOnce { ticks: u32 },
    /// Fires on entering a specific phase.
    OnStateEnter { phase: BossPhase },
    /// Fires on a named event.
    OnEvent { event_name: String },
}

/// Actions taken when a trigger fires.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EncounterAction {
    CastSkill { skill_id: u32 },
    StartMechanic { mechanic_id: u32 },
    ChangePhase { phase: BossPhase },
    SpawnNpc { kind_id: u32, count: u32 },
}

/// A single encounter rule: trigger → action.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EncounterRule {
    pub trigger: EncounterTrigger,
    pub action: EncounterAction,
    /// Whether this rule has already fired (for once-only triggers).
    pub fired: bool,
}

/// Mechanic trait — each mechanic handles its own lifecycle.
///
/// Mechanics use shared primitives (spawn_zone, assign_players,
/// apply_damage, is_entity_in_zone, schedule_at_tick) instead
/// of inventing ad hoc behavior.
pub trait Mechanic: Send {
    fn start(&mut self, tick: TickId);
    fn tick(&mut self, tick: TickId);
    fn on_event(&mut self, event_name: &str);
    fn is_finished(&self) -> bool;
}

/// Runtime state for an active boss encounter.
pub struct EncounterState {
    pub boss_entity: EntityId,
    pub phase: BossPhase,
    pub rules: Vec<EncounterRule>,
    pub active_mechanics: Vec<Box<dyn Mechanic>>,
    pub phase_start_tick: TickId,
}

/// Output actions produced by encounter evaluation.
#[derive(Clone, Debug)]
pub enum EncounterOutput {
    /// Boss phase changed — commit to boss_phase table.
    ChangeBossPhase {
        boss_entity_id: EntityId,
        new_phase: u32,
        entered_at_tick: u64,
    },
    /// Increment a zone counter (e.g., on boss kill or phase transition).
    IncrementZoneCounter {
        layer: u32,
        region_x: i32,
        region_z: i32,
        counter_name: String,
        delta: f64,
    },
}

impl EncounterState {
    /// Create a new encounter state from a boss entity and rules.
    pub fn new(boss_entity: EntityId, rules: Vec<EncounterRule>, start_tick: TickId) -> Self {
        Self {
            boss_entity,
            phase: BossPhase::Phase1,
            rules,
            active_mechanics: Vec::new(),
            phase_start_tick: start_tick,
        }
    }

    /// Evaluate encounter rules against current boss state.
    ///
    /// `boss_hp_pct` is the boss's current HP as a fraction of max (0.0–1.0).
    /// Returns a list of output actions to be processed by the pipeline.
    pub fn evaluate(
        &mut self,
        boss_hp_pct: f32,
        current_tick: TickId,
    ) -> Vec<EncounterOutput> {
        let mut outputs = Vec::new();
        let ticks_in_phase = current_tick.0.saturating_sub(self.phase_start_tick.0) as u32;

        for rule in &mut self.rules {
            if rule.fired {
                continue;
            }

            let triggered = match &rule.trigger {
                EncounterTrigger::OnHpBelowOnce { percent } => boss_hp_pct < *percent,
                EncounterTrigger::OnTimerOnce { ticks } => ticks_in_phase >= *ticks,
                EncounterTrigger::OnStateEnter { phase } => *phase == self.phase,
                EncounterTrigger::OnEvent { .. } => false, // Events not wired in V1
            };

            if triggered {
                rule.fired = true;

                match &rule.action {
                    EncounterAction::ChangePhase { phase } => {
                        let old_phase = self.phase.clone();
                        self.phase = phase.clone();
                        self.phase_start_tick = current_tick;
                        outputs.push(EncounterOutput::ChangeBossPhase {
                            boss_entity_id: self.boss_entity,
                            new_phase: phase.to_phase_number(),
                            entered_at_tick: current_tick.0,
                        });
                        log::info!(
                            "Encounter: boss {} phase {:?} → {:?} at tick {}",
                            self.boss_entity.0, old_phase, phase, current_tick.0
                        );
                        // Reset fired state for OnStateEnter rules targeting the NEW phase
                        // so they can fire on the next evaluate call.
                    }
                    EncounterAction::CastSkill { .. }
                    | EncounterAction::StartMechanic { .. }
                    | EncounterAction::SpawnNpc { .. } => {
                        // V1: log only — these actions will be wired in later phases.
                        log::info!(
                            "Encounter: boss {} triggered {:?} (not yet wired)",
                            self.boss_entity.0, rule.action
                        );
                    }
                }
            }
        }

        outputs
    }
}

/// On-disk format for encounter definitions.
#[derive(Clone, Debug, Deserialize)]
pub struct EncounterFile {
    pub encounters: Vec<EncounterDef>,
}

/// A single encounter definition keyed by boss_name.
#[derive(Clone, Debug, Deserialize)]
pub struct EncounterDef {
    pub boss_name: String,
    pub rules: Vec<EncounterRule>,
}

/// Registry of encounter definitions keyed by boss name.
pub struct EncounterRegistry {
    defs: std::collections::HashMap<String, Vec<EncounterRule>>,
}

impl EncounterRegistry {
    pub fn new() -> Self {
        Self { defs: std::collections::HashMap::new() }
    }

    pub fn register(&mut self, boss_name: String, rules: Vec<EncounterRule>) {
        self.defs.insert(boss_name, rules);
    }

    pub fn get(&self, boss_name: &str) -> Option<&Vec<EncounterRule>> {
        self.defs.get(boss_name)
    }

    /// Lookup encounter rules by boss name, returning a clone suitable for
    /// creating a new `EncounterState`.
    pub fn rules_for(&self, boss_name: &str) -> Option<Vec<EncounterRule>> {
        self.defs.get(boss_name).cloned()
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hp_threshold_fires_once() {
        let rules = vec![
            EncounterRule {
                trigger: EncounterTrigger::OnHpBelowOnce { percent: 0.5 },
                action: EncounterAction::ChangePhase { phase: BossPhase::Phase2 },
                fired: false,
            },
        ];
        let mut enc = EncounterState::new(EntityId(1), rules, TickId(0));

        // Above threshold — no output.
        let out = enc.evaluate(0.6, TickId(10));
        assert!(out.is_empty());

        // Below threshold — fires.
        let out = enc.evaluate(0.4, TickId(20));
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], EncounterOutput::ChangeBossPhase { new_phase: 2, .. }));
        assert_eq!(enc.phase, BossPhase::Phase2);

        // Already fired — does not fire again.
        let out = enc.evaluate(0.3, TickId(30));
        assert!(out.is_empty());
    }

    #[test]
    fn timer_trigger() {
        let rules = vec![
            EncounterRule {
                trigger: EncounterTrigger::OnTimerOnce { ticks: 100 },
                action: EncounterAction::ChangePhase { phase: BossPhase::Enrage },
                fired: false,
            },
        ];
        let mut enc = EncounterState::new(EntityId(2), rules, TickId(50));

        let out = enc.evaluate(1.0, TickId(140));
        assert!(out.is_empty());

        let out = enc.evaluate(1.0, TickId(150));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn encounter_def_ron_roundtrip() {
        let ron_str = r#"(encounters: [(boss_name: "Golem", rules: [(trigger: OnHpBelowOnce(percent: 0.5), action: ChangePhase(phase: Phase2), fired: false)])])"#;
        let file: EncounterFile = ron::from_str(ron_str).expect("valid RON");
        assert_eq!(file.encounters.len(), 1);
        assert_eq!(file.encounters[0].boss_name, "Golem");
    }
}
