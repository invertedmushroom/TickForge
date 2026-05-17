use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_protocol::types::Vec3f;
use game_schema::CCEffect;
use serde::{Serialize, Deserialize};

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
/// Root sources are tracked per-origin so clearing one source never clobbers
/// another. `is_rooted()` on `TickPipeline` derives the composite value from
/// `blocking || movement_conditions.is_set() || arc_state.is_some() || buff root`.
///
/// Owner per flag:
/// - `blocking`, `block_start_tick`: written by controller (Phase 2 `Block` intent),
///   cleared by `CooldownTracker` subsystem (Phase 8 sweep — `blocking` only).
/// - `dodge_stacks`: written/cleared by `AbilityTimeline` (Phase 3 `StanceBegin`/`StanceEnd`).
/// - `movement_conditions`: bitflag set; individual flags inserted/removed by
///   timeline actions (`StanceBegin`/`StanceEnd`, `SetMovement`) and controller
///   (charge-driven `INPUT_LOCK`). Multiple sources compose without clobbering.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TacticalState {
    /// Bitflag set of active movement conditions. Multiple conditions may
    /// be active simultaneously (e.g. charge INPUT_LOCK + stance ROOTED).
    /// Default is empty (no restrictions).
    pub movement_conditions: MovementConditions,
    /// Entity is executing a blocking stance this tick (hold-to-block).
    /// Phase 8 clears this each tick; the `Block` intent re-asserts it.
    /// Phase 6 checks this before applying damage — a successful block
    /// halves damage and may trigger a perfect-block bonus.
    /// Also implies rooted (entity cannot move while blocking).
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
    /// ROOTED is set by timeline actions (`StanceBegin`, `SetMovement`).
    /// INPUT_LOCK is set by charge-driven roots. Both compose cleanly via bitflags.
    /// 1-tick grace period for hold-to-block intent delivery gaps.
    /// When `blocking` is not re-asserted in a tick but `block_start_tick` is set,
    /// the block sequence gets one grace tick before emitting `BlockEnd`.
    /// This absorbs timing mismatches between client intent throttle and server ticks.
    pub block_grace: bool,
    /// Root expiry tick for `RootForTicks` timeline actions.
    /// When `Some(until)`, `is_rooted()` treats the entity as rooted while
    /// `current_tick < until`. This is used by timeline-driven exact-duration
    /// roots so they compose with other root sources without clobbering them.
    pub rooted_until_tick: Option<TickId>,
    /// Active arc-movement state (vault / leap). `None` when not in an arc.
    /// Phase 3 `ArcMovement` sets this; Phase 2 integrates gravity and feeds
    /// the result into `move_character`. Cleared when grounded or on `StanceEnd`.
    /// `is_some()` implies rooted.
    pub arc_state: Option<ArcState>,
    /// True when the entity was last observed touching the ground.
    /// Updated by `apply_movement` (from `MoveResult::grounded`) and
    /// `drive_arc_movement` (on landing). Defaults `true` — entities
    /// are assumed grounded at spawn. Read by `apply_gravity_if_airborne`
    /// to decide whether to inject a fall arc when a root is applied.
    pub is_grounded: bool,
    /// Recovery ticks to apply as a timed CC after landing from an arc
    /// (launch, pull, knockback). Set in Phase 6 when arc CC is applied.
    /// Already DR-reduced and cc_duration_reduced at authoring time.
    /// Consumed by `drive_arc_movement` on landing.
    pub arc_recovery_ticks: u32,
    /// Which CC effect to apply as recovery after arc landing.
    /// `None` means no recovery CC (pure displacement). When `Some`,
    /// `drive_arc_movement` inserts the corresponding movement condition
    /// and CC debuff for `arc_recovery_ticks` duration.
    pub arc_recovery_effect: Option<CCEffect>,
    /// Entity that authored the arc CC (attacker). Stored so that
    /// `drive_arc_movement` can attribute the recovery debuff correctly
    /// instead of using the target as its own source.
    pub arc_attacker: Option<EntityId>,
    /// Source of the fear effect (entity that caused the fear). Used to
    /// compute the flee direction in Phase 2.
    pub fear_source: Option<EntityId>,
    /// Diminishing returns tracker. Tracks recent CC applications per
    /// category so repeated CC of the same kind has reduced duration.
    pub dr_tracker: DRTracker,
    /// When `true`, this entity is immune to diminishing returns (DR is
    /// skipped). Defaults to `true` for Boss entities (they use breakbar
    /// instead). Togglable per-entity so specific boss phases can opt in.
    pub dr_immune: bool,
}

/// CC category for diminishing returns bucketing.
///
/// Multiple `CCEffect` values map to the same DR category so that
/// e.g. Stun and Knockdown share a single DR counter (HardCC).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CCCategory {
    /// Stun, Knockdown, Sleep — full action disable.
    HardCC,
    /// Fear — forced movement.
    Soft,
    /// Root (ability-driven, not currently a CCEffect but reserved).
    Root,
    /// Silence — blocks abilities only.
    Silence,
}

impl CCCategory {
    /// Map a `CCEffect` to its DR category.
    pub fn from_cc_effect(effect: CCEffect) -> Self {
        match effect {
            CCEffect::Stun | CCEffect::Knockdown | CCEffect::Sleep | CCEffect::Knockback => CCCategory::HardCC,
            CCEffect::Fear => CCCategory::Soft,
            CCEffect::Silence => CCCategory::Silence,
        }
    }

    /// Array index for the fixed-size `DRTracker` entries.
    #[inline]
    fn index(self) -> usize {
        match self {
            CCCategory::HardCC => 0,
            CCCategory::Soft => 1,
            CCCategory::Root => 2,
            CCCategory::Silence => 3,
        }
    }
}

/// One DR entry per category — tracks how many times this CC category
/// has been applied within the DR window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DREntry {
    /// Tick when this category was last applied.
    pub last_applied: TickId,
    /// Number of applications within the DR window.
    pub count: u8,
}

/// Fixed-size tracker for diminishing returns across 4 CC categories.
///
/// O(1) lookup by category index. 300-tick (15 s at 20 Hz) decay window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DRTracker {
    pub entries: [Option<DREntry>; 4],
}

impl Default for DRTracker {
    fn default() -> Self {
        Self { entries: [None; 4] }
    }
}

impl DRTracker {
    /// DR window in ticks (15 seconds at 20 Hz).
    pub const WINDOW_TICKS: u64 = 300;

    /// Record a CC application of the given category at `now`.
    /// Returns the DR multiplier to apply to the CC duration:
    /// 1st = 1.0, 2nd = 0.5, 3rd = 0.25, 4th+ = 0.0 (immune).
    pub fn apply(&mut self, category: CCCategory, now: TickId) -> f32 {
        let idx = category.index();
        let entry = &mut self.entries[idx];
        let count = match entry {
            Some(e) if now.0.saturating_sub(e.last_applied.0) < Self::WINDOW_TICKS => {
                let new_count = e.count.saturating_add(1);
                e.count = new_count;
                // Only extend the window for non-immune applications.
                // Once immune (4+), don't update last_applied — the window
                // should expire 300 ticks after the last successful CC.
                if new_count < 4 {
                    e.last_applied = now;
                }
                new_count
            }
            _ => {
                // First application or window expired — reset.
                *entry = Some(DREntry { last_applied: now, count: 1 });
                1
            }
        };
        match count {
            1 => 1.0,
            2 => 0.5,
            3 => 0.25,
            _ => 0.0, // immune
        }
    }
}

/// Bitflag set of active movement conditions.
///
/// Multiple conditions may be active simultaneously without clobbering each other.
/// Use `insert` / `remove` to change individual flags rather than wholesale assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MovementConditions(u8);

impl MovementConditions {
    /// Player input is locked (no WASD); external forces may still apply.
    /// Set by charge-driven roots (`charge_roots_while_charging = true`).
    pub const INPUT_LOCK: Self = MovementConditions(0b0000_0001);
    /// Entity is fully anchored by a timeline action or ability root.
    /// Set by `StanceBegin { rooted: true }` and `SetMovement`.
    pub const ROOTED: Self = MovementConditions(0b0000_0010);
    /// Entity is stunned and cannot act (move, cast, block).
    /// Set by timed stun or arc-based CC (knockback/pull). Cleared on
    /// timer expiry or arc landing.
    pub const STUNNED: Self = MovementConditions(0b0000_0100);
    /// Entity is knocked down on the ground and cannot act.
    /// Set by explicit knockdown CC or as recovery after a launch landing.
    pub const KNOCKED_DOWN: Self = MovementConditions(0b0000_1000);
    /// Entity is airborne from a launch/float CC and cannot act.
    /// Cleared on arc landing, then transitions to KNOCKED_DOWN for recovery.
    pub const FLOATING: Self = MovementConditions(0b0001_0000);
    /// Entity is asleep — cannot act. Broken by incoming damage.
    /// Stability does NOT absorb sleep.
    pub const SLEEPING: Self = MovementConditions(0b0010_0000);
    /// Entity is silenced — cannot cast abilities but can move/jump/block.
    /// NOT included in CC_DISABLED (movement is allowed).
    pub const SILENCED: Self = MovementConditions(0b0100_0000);
    /// Entity is feared — forced movement away from fear source.
    /// Included in CC_DISABLED (entity cannot act).
    pub const FEARED: Self = MovementConditions(0b1000_0000);

    /// All conditions that prevent the entity from acting (casting, blocking, etc.).
    /// SILENCED is intentionally excluded — silenced entities can still move/jump/block.
    pub const CC_DISABLED: Self = MovementConditions(0b1011_1100); // STUNNED | KNOCKED_DOWN | FLOATING | SLEEPING | FEARED

    #[inline] pub fn empty() -> Self { Self(0) }
    #[inline] pub fn is_empty(self) -> bool { self.0 == 0 }
    #[inline] pub fn contains(self, other: Self) -> bool { self.0 & other.0 == other.0 }
    #[inline] pub fn intersects(self, other: Self) -> bool { self.0 & other.0 != 0 }
    #[inline] pub fn insert(&mut self, other: Self) { self.0 |= other.0; }
    #[inline] pub fn remove(&mut self, other: Self) { self.0 &= !other.0; }
}

impl std::ops::BitOr for MovementConditions {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Self(self.0 | rhs.0) }
}

/// Kinematic arc state for vault/leap abilities.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArcState {
    /// Current velocity — mutated each tick as gravity is applied.
    pub velocity: Vec3f,
    /// Gravity acceleration (positive = downward pull per second).
    pub gravity: f32,
    /// `true` when this arc was injected by `apply_gravity_if_airborne`
    /// (i.e., the entity was rooted mid-air and needs to fall naturally).
    /// `StanceEnd` does not clear gravity-only arcs — only landing does.
    /// Deliberate `ArcMovement` timeline actions set this `false`.
    pub gravity_only: bool,
}

impl Default for TacticalState {
    fn default() -> Self {
        Self {
            movement_conditions: MovementConditions::empty(),
            blocking: false,
            block_start_tick: None,
            dodge_stacks: 0,
            block_grace: false,
            rooted_until_tick: None,
            arc_state: None,
            is_grounded: true, // entities start on the ground
            arc_recovery_ticks: 0,
            arc_recovery_effect: None,
            arc_attacker: None,
            fear_source: None,
            dr_tracker: DRTracker::default(),
            dr_immune: false,
        }
    }
}

impl TacticalState {
    /// Returns true if the entity has at least one active iframe window.
    pub fn is_dodging(&self) -> bool {
        self.dodge_stacks > 0
    }
}
