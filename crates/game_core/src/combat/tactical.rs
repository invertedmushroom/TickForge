/// Per-entity tactical interaction flags for the current tick.
///
/// Written by ability timelines (`StanceBegin` action) in Phase 3, read by
/// Phase 6 combat resolution to route hits through block/dodge paths rather
/// than the normal damage path, and cleared at the start of Phase 8b so
/// flags must be re-asserted each tick the stance is active.
///
/// Named `TacticalState` rather than `AbilityStance` to accommodate future
/// entries that span combat and movement concerns (super armor, forced facing,
/// lock-on channel) without implying they are purely combat concepts.
///
/// Owner per flag:
/// - `blocking`, `dodge_active`: written by `AbilityTimeline` (phase 3 StanceBegin),
///   cleared by `CooldownTracker` subsystem (phase 8 sweep).
///   Future: `controller` may also read `speed_pct` / `forced_facing` flags.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TacticalState {
    /// Entity is executing a blocking stance this tick.
    /// Phase 6 checks this before applying damage — a successful block
    /// can redirect to a `Blocked` event or reduced-damage path.
    pub blocking: bool,
    /// Entity is in an active dodge/evade window.
    /// Phase 6 checks this to route hits to a dodge-success event path.
    pub dodge_active: bool,
}
