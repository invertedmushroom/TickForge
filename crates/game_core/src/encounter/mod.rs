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
