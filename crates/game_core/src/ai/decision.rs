use game_protocol::entity_id::EntityId;
use game_protocol::types::Vec3f;
use game_schema::NpcAiState;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::sync::Arc;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AiStringId(Arc<str>);

impl AiStringId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for AiStringId {
    fn from(value: &str) -> Self {
        Self(Arc::from(value))
    }
}

impl From<String> for AiStringId {
    fn from(value: String) -> Self {
        Self(Arc::from(value))
    }
}

impl Serialize for AiStringId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AiStringId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from)
    }
}

/// Read-only per-entity snapshot for AI decision policies.
///
/// This intentionally stores stable `EntityId` values and value snapshots only.
/// Worker-side code must resolve IDs to fresh dense indices when applying actions.
#[derive(Clone, Debug, PartialEq)]
pub struct AiBlackboard<'a> {
    pub self_id: EntityId,
    pub state: NpcAiState,
    pub layer: u32,
    pub position: Vec3f,
    pub home_position: Option<Vec3f>,
    pub top_threat: Option<EntityId>,
    pub selected_target: Option<EntityId>,
    pub current_target: Option<EntityId>,
    pub goal_kind: Option<&'a str>,
    pub cc_disabled: bool,
    pub silenced: bool,
    pub rooted: bool,
    pub no_chase: bool,
    pub leash_radius: f32,
    pub aggro_radius: f32,
}

impl<'a> AiBlackboard<'a> {
    pub fn new(self_id: EntityId, state: NpcAiState, position: Vec3f) -> Self {
        Self {
            self_id,
            state,
            layer: 0,
            position,
            home_position: None,
            top_threat: None,
            selected_target: None,
            current_target: None,
            goal_kind: None,
            cc_disabled: false,
            silenced: false,
            rooted: false,
            no_chase: false,
            leash_radius: 0.0,
            aggro_radius: 0.0,
        }
    }

    pub fn with_top_threat(mut self, target: EntityId) -> Self {
        self.top_threat = Some(target);
        self.selected_target = Some(target);
        self
    }

    pub fn with_home(mut self, home_position: Vec3f, leash_radius: f32) -> Self {
        self.home_position = Some(home_position);
        self.leash_radius = leash_radius;
        self
    }

    pub fn has_goal(&self, goal_kind: &str) -> bool {
        self.goal_kind == Some(goal_kind)
    }

    pub fn exceeds_leash(&self) -> bool {
        let Some(home_position) = self.home_position else {
            return false;
        };
        if self.leash_radius <= 0.0 || !self.leash_radius.is_finite() {
            return false;
        }
        let delta_x = self.position.x - home_position.x;
        let delta_z = self.position.z - home_position.z;
        let distance_sq = delta_x * delta_x + delta_z * delta_z;
        distance_sq.is_finite() && distance_sq > self.leash_radius * self.leash_radius
    }
}

/// Worker-internal AI request emitted by decision policies.
///
/// Applying these actions remains the tick pipeline's responsibility; this type is
/// not a network or database contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DesiredAiAction {
    SetState {
        state: NpcAiState,
        reason: AiStateChangeReason,
    },
    StopMovement,
    ClearThreat,
    MoveTowardEntity(EntityId),
    MoveAwayFromEntity(EntityId),
    MoveToPoint(Vec3f),
    FollowRoute {
        route_id: AiStringId,
    },
    EvadeHome {
        home_position: Vec3f,
    },
    TryCastBestAbility {
        target: EntityId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AiStateChangeReason {
    ToCombat,
    CombatToIdle,
    LeashEvade,
    FleeToIdle,
    GoalIdle,
    EvadeNoHome,
}

pub trait AiDecisionPolicy {
    /// Append this tick's desired actions into `out`.
    ///
    /// The buffer is not cleared by the policy — callers own its lifecycle so a
    /// single scratch buffer can be reused across all entities each tick.
    fn decide_into(&self, blackboard: &AiBlackboard<'_>, out: &mut Vec<DesiredAiAction>);

    /// Convenience wrapper that allocates a fresh buffer. Prefer
    /// [`AiDecisionPolicy::decide_into`] with a reusable buffer on hot paths.
    fn decide(&self, blackboard: &AiBlackboard<'_>) -> Vec<DesiredAiAction> {
        let mut out = Vec::new();
        self.decide_into(blackboard, &mut out);
        out
    }
}

/// Policy representation of the current Phase 7 FSM behavior.
///
/// This policy is deliberately conservative: it models the existing high-level
/// decisions so worker code can later route through the boundary without changing
/// gameplay semantics.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsmAiPolicy;

impl AiDecisionPolicy for FsmAiPolicy {
    fn decide_into(&self, blackboard: &AiBlackboard<'_>, actions: &mut Vec<DesiredAiAction>) {
        let mut effective_state = blackboard.state;

        match blackboard.state {
            NpcAiState::Idle | NpcAiState::Patrol => {
                if blackboard.selected_target.is_some() {
                    actions.push(DesiredAiAction::SetState {
                        state: NpcAiState::Combat,
                        reason: AiStateChangeReason::ToCombat,
                    });
                    effective_state = NpcAiState::Combat;
                }
            }
            NpcAiState::Combat => match blackboard.selected_target {
                None => {
                    actions.push(DesiredAiAction::SetState {
                        state: NpcAiState::Idle,
                        reason: AiStateChangeReason::CombatToIdle,
                    });
                    effective_state = NpcAiState::Idle;
                }
                Some(_) if blackboard.exceeds_leash() => {
                    actions.push(DesiredAiAction::SetState {
                        state: NpcAiState::Evade,
                        reason: AiStateChangeReason::LeashEvade,
                    });
                    effective_state = NpcAiState::Evade;
                }
                Some(_) => {}
            },
            NpcAiState::Flee => {
                if blackboard.selected_target.is_none() {
                    actions.push(DesiredAiAction::SetState {
                        state: NpcAiState::Idle,
                        reason: AiStateChangeReason::FleeToIdle,
                    });
                    effective_state = NpcAiState::Idle;
                }
            }
            NpcAiState::Scripted => {
                if blackboard.has_goal("go_idle") {
                    actions.push(DesiredAiAction::SetState {
                        state: NpcAiState::Idle,
                        reason: AiStateChangeReason::GoalIdle,
                    });
                    effective_state = NpcAiState::Idle;
                }
            }
            NpcAiState::Evade => {
                if blackboard.home_position.is_none() {
                    actions.push(DesiredAiAction::SetState {
                        state: NpcAiState::Idle,
                        reason: AiStateChangeReason::EvadeNoHome,
                    });
                    effective_state = NpcAiState::Idle;
                }
            }
        }

        if blackboard.cc_disabled {
            return;
        }

        match effective_state {
            NpcAiState::Combat => {
                if let Some(target) = blackboard.selected_target {
                    if !blackboard.no_chase {
                        actions.push(DesiredAiAction::MoveTowardEntity(target));
                    }
                    if !blackboard.silenced {
                        actions.push(DesiredAiAction::TryCastBestAbility { target });
                    }
                }
            }
            NpcAiState::Flee => {
                if let Some(target) = blackboard.selected_target {
                    actions.push(DesiredAiAction::MoveAwayFromEntity(target));
                }
            }
            NpcAiState::Patrol => {
                if let Some(home_position) = blackboard.home_position {
                    actions.push(DesiredAiAction::MoveToPoint(home_position));
                }
            }
            NpcAiState::Evade => {
                if let Some(home_position) = blackboard.home_position {
                    actions.push(DesiredAiAction::EvadeHome { home_position });
                }
            }
            NpcAiState::Idle | NpcAiState::Scripted => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(id: u64) -> EntityId {
        EntityId(id)
    }

    fn position(x: f32, z: f32) -> Vec3f {
        Vec3f { x, y: 0.0, z }
    }

    #[test]
    fn idle_with_threat_enters_combat_and_acts() {
        let policy = FsmAiPolicy;
        let blackboard = AiBlackboard::new(entity(10), NpcAiState::Idle, position(0.0, 0.0))
            .with_top_threat(entity(1));

        assert_eq!(
            policy.decide(&blackboard),
            vec![
                DesiredAiAction::SetState {
                    state: NpcAiState::Combat,
                    reason: AiStateChangeReason::ToCombat,
                },
                DesiredAiAction::MoveTowardEntity(entity(1)),
                DesiredAiAction::TryCastBestAbility { target: entity(1) },
            ]
        );
    }

    #[test]
    fn combat_without_threat_returns_idle() {
        let policy = FsmAiPolicy;
        let blackboard = AiBlackboard::new(entity(10), NpcAiState::Combat, position(0.0, 0.0));

        assert_eq!(
            policy.decide(&blackboard),
            vec![DesiredAiAction::SetState {
                state: NpcAiState::Idle,
                reason: AiStateChangeReason::CombatToIdle,
            }]
        );
    }

    #[test]
    fn combat_exceeding_leash_enters_evade_and_clears_threat() {
        let policy = FsmAiPolicy;
        let home = position(0.0, 0.0);
        let blackboard = AiBlackboard::new(entity(10), NpcAiState::Combat, position(10.0, 0.0))
            .with_home(home, 5.0)
            .with_top_threat(entity(1));

        assert_eq!(
            policy.decide(&blackboard),
            vec![
                DesiredAiAction::SetState {
                    state: NpcAiState::Evade,
                    reason: AiStateChangeReason::LeashEvade,
                },
                DesiredAiAction::EvadeHome {
                    home_position: home,
                },
            ]
        );
    }

    #[test]
    fn no_chase_combat_still_casts() {
        let policy = FsmAiPolicy;
        let mut blackboard = AiBlackboard::new(entity(10), NpcAiState::Combat, position(0.0, 0.0))
            .with_top_threat(entity(1));
        blackboard.no_chase = true;

        assert_eq!(
            policy.decide(&blackboard),
            vec![DesiredAiAction::TryCastBestAbility { target: entity(1) }]
        );
    }

    #[test]
    fn silenced_combat_still_moves_but_does_not_cast() {
        let policy = FsmAiPolicy;
        let mut blackboard = AiBlackboard::new(entity(10), NpcAiState::Combat, position(0.0, 0.0))
            .with_top_threat(entity(1));
        blackboard.silenced = true;

        assert_eq!(
            policy.decide(&blackboard),
            vec![DesiredAiAction::MoveTowardEntity(entity(1))]
        );
    }

    #[test]
    fn cc_disabled_suppresses_movement_and_casting_but_keeps_transition() {
        let policy = FsmAiPolicy;
        let mut blackboard = AiBlackboard::new(entity(10), NpcAiState::Idle, position(0.0, 0.0))
            .with_top_threat(entity(1));
        blackboard.cc_disabled = true;

        assert_eq!(
            policy.decide(&blackboard),
            vec![DesiredAiAction::SetState {
                state: NpcAiState::Combat,
                reason: AiStateChangeReason::ToCombat,
            }]
        );
    }

    #[test]
    fn flee_with_threat_moves_away() {
        let policy = FsmAiPolicy;
        let blackboard = AiBlackboard::new(entity(10), NpcAiState::Flee, position(0.0, 0.0))
            .with_top_threat(entity(1));

        assert_eq!(
            policy.decide(&blackboard),
            vec![DesiredAiAction::MoveAwayFromEntity(entity(1))]
        );
    }

    #[test]
    fn flee_without_threat_returns_idle() {
        let policy = FsmAiPolicy;
        let blackboard = AiBlackboard::new(entity(10), NpcAiState::Flee, position(0.0, 0.0));

        assert_eq!(
            policy.decide(&blackboard),
            vec![DesiredAiAction::SetState {
                state: NpcAiState::Idle,
                reason: AiStateChangeReason::FleeToIdle,
            }]
        );
    }

    #[test]
    fn scripted_go_idle_sets_idle_without_clearing_threat() {
        let policy = FsmAiPolicy;
        let mut blackboard =
            AiBlackboard::new(entity(10), NpcAiState::Scripted, position(0.0, 0.0))
                .with_top_threat(entity(1));
        blackboard.goal_kind = Some("go_idle");

        assert_eq!(
            policy.decide(&blackboard),
            vec![DesiredAiAction::SetState {
                state: NpcAiState::Idle,
                reason: AiStateChangeReason::GoalIdle,
            }]
        );
    }

    #[test]
    fn evade_without_home_returns_idle() {
        let policy = FsmAiPolicy;
        let blackboard = AiBlackboard::new(entity(10), NpcAiState::Evade, position(0.0, 0.0));

        assert_eq!(
            policy.decide(&blackboard),
            vec![DesiredAiAction::SetState {
                state: NpcAiState::Idle,
                reason: AiStateChangeReason::EvadeNoHome,
            }]
        );
    }
}
