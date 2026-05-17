use game_protocol::tick::TickId;
use game_protocol::types::Vec3f;

/// Per-entity tactical interaction flags.
///
/// Two stance models:
/// - **iframe (dodge, stone):** Timeline-driven, fixed duration. Set by
///   `StanceBegin` in Phase 3, cleared by `StanceEnd` in Phase 3. Not
///   touched by Phase 8.
/// - **hold-to-block:** Intent-driven, variable duration. Set by `Block`
///   intent in Phase 2 each tick the button is held. Phase 8 clears
///   `blocking` so it must be re-asserted by the next tick's intent.
///   `block_start_tick` records when the block began for perfect-block
///   window calculation.
///
/// Named `TacticalState` rather than `AbilityStance` to accommodate future
/// entries that span combat and movement concerns (super armor, forced facing,
/// lock-on channel) without implying they are purely combat concepts.
///
/// Owner per flag:
/// - `blocking`, `block_start_tick`: written by controller (Phase 2 `Block` intent),
///   cleared by `CooldownTracker` subsystem (Phase 8 sweep — `blocking` only).
/// - `dodge_stacks`: written/cleared by `AbilityTimeline` (Phase 3 `StanceBegin`/`StanceEnd`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TacticalState {
    /// Entity is executing a blocking stance this tick (hold-to-block).
    /// Phase 8 clears this each tick; the `Block` intent re-asserts it.
    /// Phase 6 checks this before applying damage — a successful block
    /// halves damage and may trigger a perfect-block bonus.
    pub blocking: bool,
    /// Tick when the current block sequence started. Set on the first
    /// `Block` intent and preserved while `blocking` is re-asserted.
    /// Used by Phase 6 to determine if the hit falls within the
    /// perfect-block window (`current_tick - block_start_tick < PERFECT_BLOCK_TICKS`).
    pub block_start_tick: Option<TickId>,
    /// Saturating refcount for active dodge/iframe windows (timeline-driven).
    /// Incremented by `StanceBegin { dodge_active: true }`, decremented by `StanceEnd`.
    /// Phase 6 checks `is_dodging()` to skip damage entirely.
    /// Using a counter instead of a bool allows overlapping iframe windows
    /// (e.g. dodge-roll + stone-skin) without clobbering each other.
    pub dodge_stacks: u8,
    /// Entity cannot move this tick. Sources:
    /// - Phase 2: `Block` intent sets this alongside `blocking`.
    /// - Phase 3: `StanceBegin { rooted: true }` sets this for channeled skills.
    /// Cleared by Phase 8 (block root) or `StanceEnd` (timeline root).
    pub rooted: bool,
    /// 1-tick grace period for hold-to-block intent delivery gaps.
    /// When `blocking` is not re-asserted in a tick but `block_start_tick` is set,
    /// the block sequence gets one grace tick before emitting `BlockEnd`.
    /// This absorbs timing mismatches between client intent throttle and server ticks.
    pub block_grace: bool,
    /// Active arc-movement state (vault / leap). `None` when not in an arc.
    /// Phase 3 `ArcMovement` sets this; Phase 2 integrates gravity and feeds
    /// the result into `move_character`. Cleared when grounded or on `StanceEnd`.
    pub arc_state: Option<ArcState>,
}

/// Kinematic arc state for vault/leap abilities.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArcState {
    /// Current velocity — mutated each tick as gravity is applied.
    pub velocity: Vec3f,
    /// Gravity acceleration (positive = downward pull per second).
    pub gravity: f32,
}

impl TacticalState {
    /// Returns true if the entity has at least one active iframe window.
    pub fn is_dodging(&self) -> bool {
        self.dodge_stacks > 0
    }
}
