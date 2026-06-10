use game_protocol::entity_id::EntityId;
use game_schema::NpcAiState;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::decision::{
    AiBlackboard, AiDecisionPolicy, AiStateChangeReason, AiStringId, DesiredAiAction,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BehaviorStatus {
    Success,
    Failure,
    Running,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BehaviorNode {
    Selector(Vec<BehaviorNode>),
    Sequence(Vec<BehaviorNode>),
    Condition(ConditionNode),
    Action(ActionNode),
    Invert(Box<BehaviorNode>),
}

impl BehaviorNode {
    pub fn tick(
        &self,
        blackboard: &AiBlackboard<'_>,
        actions: &mut Vec<DesiredAiAction>,
    ) -> BehaviorStatus {
        match self {
            Self::Selector(children) => tick_selector(children, blackboard, actions),
            Self::Sequence(children) => tick_sequence(children, blackboard, actions),
            Self::Condition(condition) => condition.tick(blackboard),
            Self::Action(action) => action.tick(blackboard, actions),
            Self::Invert(child) => {
                // Invert never commits its child's actions; tick into the shared
                // buffer then roll back to discard whatever the child appended.
                let mark = actions.len();
                let status = child.tick(blackboard, actions);
                actions.truncate(mark);
                match status {
                    BehaviorStatus::Success => BehaviorStatus::Failure,
                    BehaviorStatus::Failure => BehaviorStatus::Success,
                    BehaviorStatus::Running => BehaviorStatus::Running,
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ConditionNode {
    Always,
    IsState(NpcAiState),
    HasGoal(AiStringId),
    HasSelectedTarget,
    ShouldEvade,
    HasHome,
    IsCcDisabled,
    IsSilenced,
    NoChase,
}

impl ConditionNode {
    pub fn tick(&self, blackboard: &AiBlackboard<'_>) -> BehaviorStatus {
        if self.evaluate(blackboard) {
            BehaviorStatus::Success
        } else {
            BehaviorStatus::Failure
        }
    }

    pub fn evaluate(&self, blackboard: &AiBlackboard<'_>) -> bool {
        match self {
            Self::Always => true,
            Self::IsState(state) => blackboard.state == *state,
            Self::HasGoal(goal_kind) => blackboard.has_goal(goal_kind.as_str()),
            Self::HasSelectedTarget => blackboard.selected_target.is_some(),
            Self::ShouldEvade => blackboard.exceeds_leash() && blackboard.selected_target.is_some(),
            Self::HasHome => blackboard.home_position.is_some(),
            Self::IsCcDisabled => blackboard.cc_disabled,
            Self::IsSilenced => blackboard.silenced,
            Self::NoChase => blackboard.no_chase,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ActionNode {
    Emit(DesiredAiAction),
    SetState {
        state: NpcAiState,
        reason: AiStateChangeReason,
    },
    StopMovement,
    ClearThreat,
    MoveTowardSelectedTarget,
    MoveAwayFromSelectedTarget,
    MoveToHome,
    FollowRoute(AiStringId),
    EvadeHome,
    TryCastBestAbilityAtSelectedTarget,
    Idle,
}

impl ActionNode {
    pub fn tick(
        &self,
        blackboard: &AiBlackboard<'_>,
        actions: &mut Vec<DesiredAiAction>,
    ) -> BehaviorStatus {
        match self {
            Self::Emit(action) => {
                actions.push(action.clone());
                BehaviorStatus::Success
            }
            Self::SetState { state, reason } => {
                actions.push(DesiredAiAction::SetState {
                    state: *state,
                    reason: *reason,
                });
                BehaviorStatus::Success
            }
            Self::StopMovement => {
                actions.push(DesiredAiAction::StopMovement);
                BehaviorStatus::Success
            }
            Self::ClearThreat => {
                actions.push(DesiredAiAction::ClearThreat);
                BehaviorStatus::Success
            }
            Self::MoveTowardSelectedTarget => emit_target_action(
                blackboard.selected_target,
                actions,
                DesiredAiAction::MoveTowardEntity,
            ),
            Self::MoveAwayFromSelectedTarget => emit_target_action(
                blackboard.selected_target,
                actions,
                DesiredAiAction::MoveAwayFromEntity,
            ),
            Self::MoveToHome => {
                let Some(home) = blackboard.home_position else {
                    return BehaviorStatus::Failure;
                };
                actions.push(DesiredAiAction::MoveToPoint(home));
                BehaviorStatus::Running
            }
            Self::FollowRoute(route_id) => {
                if route_id.as_str().trim().is_empty() {
                    return BehaviorStatus::Failure;
                }
                actions.push(DesiredAiAction::FollowRoute {
                    route_id: route_id.clone(),
                });
                BehaviorStatus::Running
            }
            Self::EvadeHome => {
                let Some(home) = blackboard.home_position else {
                    return BehaviorStatus::Failure;
                };
                actions.push(DesiredAiAction::EvadeHome {
                    home_position: home,
                });
                BehaviorStatus::Running
            }
            Self::TryCastBestAbilityAtSelectedTarget => {
                if blackboard.silenced {
                    return BehaviorStatus::Failure;
                }
                let Some(target) = blackboard.selected_target else {
                    return BehaviorStatus::Failure;
                };
                actions.push(DesiredAiAction::TryCastBestAbility { target });
                BehaviorStatus::Success
            }
            Self::Idle => BehaviorStatus::Success,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BehaviorTreePolicy {
    root: BehaviorNode,
}

impl BehaviorTreePolicy {
    pub fn new(root: BehaviorNode) -> Self {
        Self { root }
    }

    pub fn scripted_goal_idle_tree() -> Self {
        Self::new(BehaviorNode::Selector(vec![
            BehaviorNode::Sequence(vec![
                BehaviorNode::Condition(ConditionNode::HasGoal("go_idle".into())),
                BehaviorNode::Action(ActionNode::ClearThreat),
                BehaviorNode::Action(ActionNode::SetState {
                    state: NpcAiState::Idle,
                    reason: AiStateChangeReason::GoalIdle,
                }),
            ]),
            BehaviorNode::Action(ActionNode::Idle),
        ]))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BehaviorTreeDef {
    pub id: String,
    pub root: BehaviorNode,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BehaviorTreeFile {
    pub trees: Vec<BehaviorTreeDef>,
}

#[derive(Clone, Debug, Default)]
pub struct BehaviorTreeRegistry {
    trees: HashMap<String, BehaviorNode>,
}

impl BehaviorTreeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_file(file: BehaviorTreeFile) -> Result<Self, String> {
        let mut registry = Self::new();
        for tree in file.trees {
            registry.register(tree)?;
        }
        Ok(registry)
    }

    pub fn from_ron(src: &str) -> Result<Self, String> {
        let file: BehaviorTreeFile = ron::from_str(src).map_err(|err| err.to_string())?;
        Self::from_file(file)
    }

    pub fn register(&mut self, tree: BehaviorTreeDef) -> Result<(), String> {
        if tree.id.trim().is_empty() {
            return Err("behavior tree id must not be empty".to_string());
        }
        if self.trees.contains_key(&tree.id) {
            return Err(format!("duplicate behavior tree id '{}'", tree.id));
        }
        self.trees.insert(tree.id, tree.root);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&BehaviorNode> {
        self.trees.get(id)
    }

    pub fn policy(&self, id: &str) -> Option<BehaviorTreePolicy> {
        self.get(id).cloned().map(BehaviorTreePolicy::new)
    }

    pub fn len(&self) -> usize {
        self.trees.len()
    }

    pub fn is_empty(&self) -> bool {
        self.trees.is_empty()
    }
}

impl AiDecisionPolicy for BehaviorTreePolicy {
    fn decide_into(&self, blackboard: &AiBlackboard<'_>, actions: &mut Vec<DesiredAiAction>) {
        self.root.tick(blackboard, actions);
    }
}

fn tick_selector(
    children: &[BehaviorNode],
    blackboard: &AiBlackboard<'_>,
    actions: &mut Vec<DesiredAiAction>,
) -> BehaviorStatus {
    // Tick children directly into the shared buffer, rolling back any actions a
    // failing child appended so only the first non-failing child commits.
    for child in children {
        let mark = actions.len();
        let status = child.tick(blackboard, actions);
        if status != BehaviorStatus::Failure {
            return status;
        }
        actions.truncate(mark);
    }
    BehaviorStatus::Failure
}

fn tick_sequence(
    children: &[BehaviorNode],
    blackboard: &AiBlackboard<'_>,
    actions: &mut Vec<DesiredAiAction>,
) -> BehaviorStatus {
    // Accumulate child actions in-place; discard the whole sequence's actions if
    // any child fails by truncating back to the pre-sequence buffer length.
    let mark = actions.len();
    for child in children {
        match child.tick(blackboard, actions) {
            BehaviorStatus::Success => {}
            BehaviorStatus::Running => return BehaviorStatus::Running,
            BehaviorStatus::Failure => {
                actions.truncate(mark);
                return BehaviorStatus::Failure;
            }
        }
    }
    BehaviorStatus::Success
}

fn emit_target_action(
    target: Option<EntityId>,
    actions: &mut Vec<DesiredAiAction>,
    constructor: fn(EntityId) -> DesiredAiAction,
) -> BehaviorStatus {
    let Some(target) = target else {
        return BehaviorStatus::Failure;
    };
    actions.push(constructor(target));
    BehaviorStatus::Running
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_protocol::types::Vec3f;

    fn entity(id: u64) -> EntityId {
        EntityId(id)
    }

    fn blackboard(state: NpcAiState) -> AiBlackboard<'static> {
        AiBlackboard::new(entity(10), state, Vec3f::ZERO)
    }

    #[test]
    fn selector_commits_only_first_successful_child_actions() {
        let root = BehaviorNode::Selector(vec![
            BehaviorNode::Sequence(vec![
                BehaviorNode::Condition(ConditionNode::HasSelectedTarget),
                BehaviorNode::Action(ActionNode::Emit(DesiredAiAction::StopMovement)),
            ]),
            BehaviorNode::Action(ActionNode::ClearThreat),
        ]);
        let mut actions = Vec::new();

        let status = root.tick(&blackboard(NpcAiState::Idle), &mut actions);

        assert_eq!(status, BehaviorStatus::Success);
        assert_eq!(actions, vec![DesiredAiAction::ClearThreat]);
    }

    #[test]
    fn sequence_discards_actions_when_later_child_fails() {
        let root = BehaviorNode::Sequence(vec![
            BehaviorNode::Action(ActionNode::ClearThreat),
            BehaviorNode::Condition(ConditionNode::HasSelectedTarget),
        ]);
        let mut actions = Vec::new();

        let status = root.tick(&blackboard(NpcAiState::Idle), &mut actions);

        assert_eq!(status, BehaviorStatus::Failure);
        assert!(actions.is_empty());
    }

    #[test]
    fn scripted_goal_idle_tree_clears_threat_and_sets_idle() {
        let tree = BehaviorTreePolicy::scripted_goal_idle_tree();
        let mut bb = blackboard(NpcAiState::Scripted).with_top_threat(entity(1));
        bb.goal_kind = Some("go_idle");

        assert_eq!(
            tree.decide(&bb),
            vec![
                DesiredAiAction::ClearThreat,
                DesiredAiAction::SetState {
                    state: NpcAiState::Idle,
                    reason: AiStateChangeReason::GoalIdle,
                },
            ]
        );
    }

    #[test]
    fn combat_tree_can_emit_cast_and_movement_for_selected_target() {
        let tree = BehaviorTreePolicy::new(BehaviorNode::Sequence(vec![
            BehaviorNode::Condition(ConditionNode::HasSelectedTarget),
            BehaviorNode::Action(ActionNode::TryCastBestAbilityAtSelectedTarget),
            BehaviorNode::Action(ActionNode::MoveTowardSelectedTarget),
        ]));
        let bb = blackboard(NpcAiState::Combat).with_top_threat(entity(1));

        assert_eq!(
            tree.decide(&bb),
            vec![
                DesiredAiAction::TryCastBestAbility { target: entity(1) },
                DesiredAiAction::MoveTowardEntity(entity(1)),
            ]
        );
    }

    #[test]
    fn silenced_selector_falls_back_to_movement() {
        let tree = BehaviorTreePolicy::new(BehaviorNode::Selector(vec![
            BehaviorNode::Sequence(vec![
                BehaviorNode::Invert(Box::new(BehaviorNode::Condition(ConditionNode::IsSilenced))),
                BehaviorNode::Action(ActionNode::TryCastBestAbilityAtSelectedTarget),
            ]),
            BehaviorNode::Action(ActionNode::MoveTowardSelectedTarget),
        ]));
        let mut bb = blackboard(NpcAiState::Combat).with_top_threat(entity(1));
        bb.silenced = true;

        assert_eq!(
            tree.decide(&bb),
            vec![DesiredAiAction::MoveTowardEntity(entity(1))]
        );
    }

    #[test]
    fn evade_home_returns_running_and_emits_evade_action() {
        let tree = BehaviorTreePolicy::new(BehaviorNode::Action(ActionNode::EvadeHome));
        let mut bb = blackboard(NpcAiState::Evade);
        bb.home_position = Some(Vec3f::new(1.0, 0.0, 2.0));
        let mut actions = Vec::new();

        let status = tree.root.tick(&bb, &mut actions);

        assert_eq!(status, BehaviorStatus::Running);
        assert_eq!(
            actions,
            vec![DesiredAiAction::EvadeHome {
                home_position: Vec3f::new(1.0, 0.0, 2.0),
            }]
        );
    }

    #[test]
    fn follow_route_returns_running_and_emits_route_action() {
        let tree = BehaviorTreePolicy::new(BehaviorNode::Action(ActionNode::FollowRoute(
            "training_patrol_loop".into(),
        )));
        let bb = blackboard(NpcAiState::Patrol);
        let mut actions = Vec::new();

        let status = tree.root.tick(&bb, &mut actions);

        assert_eq!(status, BehaviorStatus::Running);
        assert_eq!(
            actions,
            vec![DesiredAiAction::FollowRoute {
                route_id: "training_patrol_loop".into(),
            }]
        );
    }

    #[test]
    fn parses_shipped_behavior_trees_ron() {
        let registry =
            BehaviorTreeRegistry::from_ron(include_str!("../../../../data/behavior_trees.ron"))
                .expect("behavior_trees.ron should parse");

        assert!(registry.get("scripted_goal_idle").is_some());
        assert!(registry.get("basic_melee_combat").is_some());
    }

    #[test]
    fn registry_rejects_duplicate_ids() {
        let src = r#"
(
    trees: [
        (id: "dup", root: Action(Idle)),
        (id: "dup", root: Action(Idle)),
    ],
)
"#;

        let err = BehaviorTreeRegistry::from_ron(src).expect_err("duplicates should fail");

        assert!(err.contains("duplicate behavior tree id 'dup'"));
    }

    #[test]
    fn registry_policy_runs_loaded_tree() {
        let registry =
            BehaviorTreeRegistry::from_ron(include_str!("../../../../data/behavior_trees.ron"))
                .expect("behavior_trees.ron should parse");
        let tree = registry
            .policy("scripted_goal_idle")
            .expect("scripted_goal_idle should exist");
        let mut bb = blackboard(NpcAiState::Scripted).with_top_threat(entity(1));
        bb.goal_kind = Some("go_idle");

        assert_eq!(
            tree.decide(&bb),
            vec![
                DesiredAiAction::ClearThreat,
                DesiredAiAction::SetState {
                    state: NpcAiState::Idle,
                    reason: AiStateChangeReason::GoalIdle,
                },
            ]
        );
    }
}
