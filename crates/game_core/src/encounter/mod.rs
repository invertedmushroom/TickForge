//! Boss encounter framework — composable authoring model.
//!
//! Each encounter is an `EncounterScript` consisting of [`Rule`]s. A rule is
//! `(Trigger, Cond, Vec<Effect>, RepeatPolicy)`. Triggers gate _when_ a rule
//! potentially fires; conditions gate _whether_ it fires; effects are leaf
//! actions executed in order on the tick the rule fires.
//!
//! `Sequence`/`Parallel`/`Wait` are live composers. A `Sequence` applies steps
//! until the first `Wait`, parks the remaining tail as an encounter-owned
//! continuation, and resumes it from the start of a later `evaluate()` call.
//! Event-driven triggers consume the encounter bus snapshot for the current
//! tick.
//!
//! Mechanic instantiation is decoupled from rule evaluation: `evaluate()`
//! returns an [`EncounterOutput::StartMechanic { name, params }`], which the
//! worker resolves through a [`MechanicRegistry`] (name → factory) and starts
//! with a live [`MechanicCtx`].

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Arc;

use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_protocol::types::Vec3f;
use serde::{Deserialize, Serialize};

use crate::volume::{EntityKindFilter, VolumeId, VolumeShape};

// ── Phase identifier ────────────────────────────────────────────

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

// ── Triggers ────────────────────────────────────────────────────

/// Comparison operator used by `Cond` and counter-style `Trigger`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmpOp {
    Lt,
    Le,
    Eq,
    Ne,
    Ge,
    Gt,
}

impl CmpOp {
    fn cmp_i64(self, lhs: i64, rhs: i64) -> bool {
        match self {
            CmpOp::Lt => lhs < rhs,
            CmpOp::Le => lhs <= rhs,
            CmpOp::Eq => lhs == rhs,
            CmpOp::Ne => lhs != rhs,
            CmpOp::Ge => lhs >= rhs,
            CmpOp::Gt => lhs > rhs,
        }
    }

    fn cmp_f32(self, lhs: f32, rhs: f32) -> bool {
        match self {
            CmpOp::Lt => lhs < rhs,
            CmpOp::Le => lhs <= rhs,
            CmpOp::Eq => lhs == rhs,
            CmpOp::Ne => lhs != rhs,
            CmpOp::Ge => lhs >= rhs,
            CmpOp::Gt => lhs > rhs,
        }
    }
}

/// What event makes a rule potentially fire.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Trigger {
    /// Fires the first tick after the rule's owning encounter enters `phase`.
    /// (`Once` semantics with `OnEachPhaseEntry` re-arms it on each entry.)
    OnEnter { phase: BossPhase },
    /// Fires when boss HP first drops below `percent`.
    OnHpBelow { percent: f32 },
    /// Fires once after `ticks` ticks since current phase began.
    OnAfter { ticks: u32 },
    /// Fires every `ticks` ticks. Combine with `RepeatPolicy::EveryNTicks`.
    OnEvery { ticks: u32 },
    /// Fires when a `Custom` event with `event_name` was pushed into the
    /// encounter bus this tick (typically by `MechanicCtx::log_event`
    /// or `Effect::EmitEncounterEvent`).
    OnEvent { event_name: String },
    /// Fires when an entity classified with `tag` died this tick (boss
    /// or any add the worker is tracking for this encounter).
    OnEntityDied { tag: String },
    /// Counter-on-encounter comparison; fires on the tick the comparison flips
    /// from false → true.
    OnCounter { name: String, op: CmpOp, value: i64 },
    /// Fires when a mechanic with the matching `name` finished this tick.
    OnMechanicEnded { name: String },
    /// An entity entered a volume matching `tag`. Volume events are published
    /// by the volume sync before encounter rule evaluation, and only fire when
    /// a corresponding volume currently exists.
    OnVolumeEnter { tag: String },
    /// An entity exited a volume matching `tag`.
    OnVolumeExit { tag: String },
}

// ── Conditions (composable gate) ────────────────────────────────

/// Condition gate evaluated when a rule's trigger has fired.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Cond {
    /// Always true. Default for rules without an explicit gate.
    Always,
    PhaseIs {
        phase: BossPhase,
    },
    HpPctCmp {
        op: CmpOp,
        value: f32,
    },
    CounterCmp {
        name: String,
        op: CmpOp,
        value: i64,
    },
    All {
        conds: Vec<Cond>,
    },
    Any {
        conds: Vec<Cond>,
    },
    Not {
        cond: Box<Cond>,
    },
    /// True when at least `count` distinct entities of any kind currently
    /// overlap any volume tagged `tag` (across all volumes with that tag).
    OccupancyCmp {
        tag: String,
        op: CmpOp,
        count: i64,
    },
    /// True when every occupant of `source_tag` is present in exactly one of
    /// the `member_tags`. Empty source occupancy is valid.
    VolumeOccupantsExactlyOneOf {
        source_tag: String,
        member_tags: Vec<String>,
    },
    /// True when every current occupant of `tag` has `buff_id`. Empty
    /// occupancy is valid.
    AllVolumeOccupantsHaveBuff {
        tag: String,
        buff_id: u32,
    },
}

impl Default for Cond {
    fn default() -> Self {
        Cond::Always
    }
}

// ── Targets ─────────────────────────────────────────────────────

/// Targeting hint for effects that need an entity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Target {
    /// The boss itself.
    Boss,
    /// Boss's current top-threat target.
    TopThreat,
    /// A pseudo-random player (worker resolves with a deterministic tick seed).
    RandomPlayer,
    /// All players in the encounter.
    AllPlayers,
    /// All entities currently inside any volume matching `tag`.
    VolumeOccupants { tag: String },
    /// Runtime-only explicit entity target, used by mechanics after sampling
    /// players from live state. Authors should not use this in RON.
    RuntimeEntity { entity: EntityId },
    /// A world-space point offset from the boss's cast-time facing.
    BossOffset { offset: [f32; 3] },
    /// A fixed world-space point in the encounter arena.
    FixedPoint { position: [f32; 3] },
}

/// Stable selector for dungeon interactables whose runtime entity IDs are
/// assigned by instance creation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum InteractableSelector {
    ScriptId { script_id: String },
    Tag { tag: String },
}

/// Runtime state value an encounter script can apply to an interactable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InteractableStateValue {
    Idle,
    Active,
    Cooldown,
}

/// Shared volume definition usable by built-in mechanics.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MechanicZoneParam {
    pub tag: String,
    pub shape: VolumeShape,
    #[serde(default = "MechanicZoneParam::default_anchor")]
    pub anchor: VolumeAnchor,
    #[serde(default)]
    pub priority: u32,
    #[serde(default)]
    pub buff_id: Option<u32>,
    #[serde(default)]
    pub lifetime_ticks: Option<u32>,
    /// Optional override for the volume's entity-kind filter. When `None`,
    /// built-in mechanics that spawn this zone default to filtering for
    /// players only — preserving historical behavior. Authors can opt
    /// into NPC/all coverage by setting this explicitly (e.g.
    /// `EntityKindFilter::Any` for a zone that should also affect
    /// scripted adds).
    #[serde(default)]
    pub entity_filter: Option<EntityKindFilter>,
}

impl MechanicZoneParam {
    fn default_anchor() -> VolumeAnchor {
        VolumeAnchor::FollowBoss
    }
}

// ── Mechanic params ─────────────────────────────────────────────

/// Typed bag of named parameters passed to a mechanic factory.
///
/// `BTreeMap` keeps deterministic iteration so RON-loaded params produce
/// reproducible mechanic state across runs and replays.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MechanicParams {
    #[serde(default)]
    pub ints: BTreeMap<String, i64>,
    #[serde(default)]
    pub floats: BTreeMap<String, f64>,
    #[serde(default)]
    pub strings: BTreeMap<String, String>,
    #[serde(default)]
    pub zones: Vec<MechanicZoneParam>,
}

impl MechanicParams {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn int(&self, key: &str) -> Option<i64> {
        self.ints.get(key).copied()
    }

    pub fn float(&self, key: &str) -> Option<f64> {
        self.floats.get(key).copied()
    }

    pub fn string(&self, key: &str) -> Option<&str> {
        self.strings.get(key).map(String::as_str)
    }
}

/// How a buff effect should combine with existing active buffs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BuffApplyMode {
    StackOrRefresh,
    ReplaceAny(Vec<u32>),
}

impl Default for BuffApplyMode {
    fn default() -> Self {
        BuffApplyMode::StackOrRefresh
    }
}

/// Where a client-facing encounter cue should be anchored.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EncounterCueAnchor {
    /// Follow the boss entity. The worker also snapshots the boss position when
    /// the cue is emitted so clients can render immediately.
    Boss,
    /// Follow the target that receives the cue.
    Target,
    /// Render at a fixed world position.
    FixedPoint { position: [f32; 3] },
}

impl Default for EncounterCueAnchor {
    fn default() -> Self {
        EncounterCueAnchor::Boss
    }
}

/// Presentation shape for a client-facing encounter cue.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum EncounterCueShape {
    None,
    Sphere {
        radius: f32,
    },
    Ring {
        inner_radius: f32,
        outer_radius: f32,
        half_height: f32,
    },
}

impl Default for EncounterCueShape {
    fn default() -> Self {
        EncounterCueShape::None
    }
}

// ── Effects (leaves + composers) ─────────────────────────────────

/// Actions emitted by rule evaluation.
///
/// Leaf effects become `EncounterOutput`s or in-memory counter mutations.
/// Composer effects (`Sequence`/`Parallel`/`Wait`) are live: a `Sequence`
/// parks its tail at the first `Wait` and resumes it on a later `evaluate()`
/// call, while `Parallel` applies every step in the current tick.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Effect {
    ChangePhase {
        phase: BossPhase,
    },
    CastSkill {
        skill_id: u32,
        target: Target,
    },
    ReplaceAbilityList {
        ability_ids: Vec<u32>,
    },
    StartMechanic {
        name: String,
        #[serde(default)]
        params: MechanicParams,
    },
    StopMechanic {
        name: String,
    },
    /// Spawn `count` scripted adds of the named NPC archetype, each
    /// labelled with `tags` so downstream rules (`OnEntityDied { tag }`,
    /// `WhenAdds { tag, … }`) can match. See
    /// `docs/contracts/spawn_add_membership_contract.md`.
    SpawnAdds {
        archetype: String,
        count: u32,
        #[serde(default)]
        tags: Vec<String>,
    },
    /// Visual telegraph request. The worker resolves the target and emits
    /// `TelegraphWarning` / area telegraph events when the target is valid.
    Telegraph {
        skill_id: u32,
        target: Target,
        lead_ticks: u32,
    },
    /// Client-facing encounter presentation cue. This is distinct from
    /// `EmitEncounterEvent`: encounter events are internal rule wiring, while
    /// cues are committed for clients to render mechanic UI/VFX.
    EncounterCue {
        target: Target,
        cue_id: String,
        #[serde(default)]
        anchor: EncounterCueAnchor,
        #[serde(default)]
        shape: EncounterCueShape,
        #[serde(default)]
        lead_ticks: u32,
        #[serde(default)]
        duration_ticks: u32,
    },
    /// Apply a buff through the worker's authoritative status pipeline.
    ApplyBuff {
        target: Target,
        buff_id: u32,
        #[serde(default)]
        mode: BuffApplyMode,
    },
    /// Remove specific buffs from target entities. `force=true` bypasses
    /// mechanic-lock policy for cleanup paths owned by the mechanic itself.
    RemoveBuffs {
        target: Target,
        buff_ids: Vec<u32>,
        #[serde(default)]
        force: bool,
    },
    /// Set a dungeon interactable state by stable script selector.
    SetInteractableState {
        selector: InteractableSelector,
        state: InteractableStateValue,
    },
    /// Toggle a dungeon interactable by stable script selector.
    ToggleInteractable {
        selector: InteractableSelector,
    },
    /// Mutate per-encounter counter (in-memory; not committed to DB).
    IncrementCounter {
        name: String,
        delta: i64,
    },
    /// Mutate the world `zone_counter` table through the tick-immediate
    /// commit path. Tier 2 (`world_clock`) consumes the counter later.
    IncrementZoneCounter {
        layer: u32,
        region_x: i32,
        region_z: i32,
        counter_name: String,
        delta: f64,
    },
    /// Push a free-form event into the encounter bus. `OnEvent` triggers can
    /// consume it on the next rule evaluation pass.
    EmitEncounterEvent {
        name: String,
    },
    /// Spawn a gameplay volume. The volume tracks its occupants every tick
    /// and emits `VolumeEnter`/`VolumeExit` events on the sim event bus.
    /// `lifetime_ticks = None` keeps the volume alive until an explicit
    /// `DespawnVolume`.
    SpawnVolume {
        tag: String,
        shape: VolumeShape,
        /// Where to anchor the volume. `Boss` follows the boss this tick;
        /// volumes do not auto-track movement — a follow-up `SpawnVolume`
        /// must replace them if a moving anchor is needed.
        anchor: VolumeAnchor,
        lifetime_ticks: Option<u32>,
        /// Which entities count as occupants. Defaults to `Any`.
        #[serde(default)]
        entity_filter: EntityKindFilter,
    },
    /// Despawn all volumes matching `tag` for this encounter's boss.
    DespawnVolume {
        tag: String,
    },
    /// Sleep `ticks` ticks before applying the next step in a containing
    /// `Sequence`. A bare `Wait` outside a `Sequence` is a no-op.
    Wait {
        ticks: u32,
    },
    /// Apply `steps` in order, with `Wait` parking the remaining tail as a
    /// continuation that resumes on a future tick. The continuation is
    /// owned by the encounter and re-entered at the start of `evaluate`.
    Sequence {
        steps: Vec<Effect>,
    },
    /// Apply every `step` this tick, in order. Equivalent to inlining the
    /// list, but explicit for authoring clarity.
    Parallel {
        steps: Vec<Effect>,
    },
}

/// Where to spawn a volume. Resolved by the worker against the encounter's
/// boss + sim state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum VolumeAnchor {
    /// Anchor at the boss's current position (one-shot at spawn time).
    Boss,
    /// Anchor at a fixed world-space position.
    World { position: [f32; 3] },
    /// Anchor at the boss + this offset (boss-local, applied at spawn time).
    BossOffset { offset: [f32; 3] },
    /// Track the boss every tick. The worker re-resolves the position on
    /// each `phase_volume_sync` and moves the underlying physics sensor.
    FollowBoss,
}

impl VolumeAnchor {
    pub fn world(x: f32, y: f32, z: f32) -> Self {
        VolumeAnchor::World {
            position: [x, y, z],
        }
    }

    pub fn world_vec(&self) -> Option<Vec3f> {
        match self {
            VolumeAnchor::World { position } => Some(Vec3f {
                x: position[0],
                y: position[1],
                z: position[2],
            }),
            _ => None,
        }
    }
}

// ── Repeat policy ───────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RepeatPolicy {
    /// Fire once per encounter lifetime.
    Once,
    /// Re-arm on every phase entry.
    OnEachPhaseEntry,
    /// Periodic with optional cap.
    EveryNTicks {
        interval: u32,
        max_fires: Option<u32>,
    },
}

impl Default for RepeatPolicy {
    fn default() -> Self {
        RepeatPolicy::Once
    }
}

// ── Rule ────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub when: Trigger,
    #[serde(default)]
    pub cond: Cond,
    pub effects: Vec<Effect>,
    #[serde(default)]
    pub repeat: RepeatPolicy,

    /// Total times this rule has fired across the encounter lifetime.
    #[serde(skip)]
    pub fire_count: u32,
    /// Tick this rule last fired (for `EveryNTicks` cadence).
    #[serde(skip)]
    pub last_fire_tick: Option<TickId>,
    /// HP-threshold latch so `OnHpBelow` only fires once per arming
    /// (until next phase entry under `OnEachPhaseEntry`).
    #[serde(skip)]
    pub hp_threshold_armed: bool,
    /// Last counter comparison result, used to detect false→true edges
    /// for `Trigger::OnCounter`.
    #[serde(skip)]
    pub counter_was_true: bool,
}

impl Rule {
    fn re_arm_on_phase_entry(&mut self) {
        if matches!(self.repeat, RepeatPolicy::OnEachPhaseEntry) {
            self.fire_count = 0;
            self.last_fire_tick = None;
            self.hp_threshold_armed = true;
            self.counter_was_true = false;
        }
    }

    /// Unconditionally clear all per-rule firing state. Used by the encounter
    /// leash reset so a re-entered fight re-runs `Once`/`OnEnter` setup and
    /// already-fired `OnHpBelow`/`OnCounter` progression rules from a clean
    /// slate. Unlike [`re_arm_on_phase_entry`](Self::re_arm_on_phase_entry),
    /// this ignores `RepeatPolicy`.
    pub fn reset(&mut self) {
        self.fire_count = 0;
        self.last_fire_tick = None;
        self.hp_threshold_armed = true;
        self.counter_was_true = false;
    }

    fn can_fire(&self) -> bool {
        match &self.repeat {
            RepeatPolicy::Once | RepeatPolicy::OnEachPhaseEntry => self.fire_count == 0,
            RepeatPolicy::EveryNTicks { max_fires, .. } => match max_fires {
                Some(cap) => self.fire_count < *cap,
                None => true,
            },
        }
    }
}

// ── Mechanic trait + ctx ────────────────────────────────────────

/// Runtime context passed to mechanics on every lifecycle callback.
///
/// Mechanics interact with the live simulation through this surface only.
/// The single primitive is `emit_effect`, which routes any `Effect` through
/// the same pipeline rules use. Convenience wrappers (`cast_boss_skill`,
/// etc.) compose on top. Concrete implementations live in the simulation
/// worker — `game_core` only sees the trait, keeping the mechanic API
/// independent of any particular pipeline implementation.
pub trait MechanicCtx {
    /// The current simulation tick.
    fn current_tick(&self) -> TickId;

    /// The boss entity that owns the encounter this mechanic belongs to.
    fn boss_entity_id(&self) -> EntityId;

    /// Queue an [`Effect`] for the encounter to apply after the current
    /// mechanic call returns. The effect is processed identically to one
    /// emitted by a [`Rule`], producing the same [`EncounterOutput`]s and
    /// state mutations (phase change, counter increment, etc.).
    fn emit_effect(&mut self, effect: Effect);

    /// Snapshot occupants of all volumes matching `tag` for this encounter.
    fn volume_occupants(&self, _tag: &str) -> Vec<EntityId> {
        Vec::new()
    }

    /// Returns whether `entity` currently has `buff_id`.
    fn has_buff(&self, _entity: EntityId, _buff_id: u32) -> bool {
        false
    }

    // ── Convenience wrappers (default-implemented over `emit_effect`). ──

    /// Cast a boss skill. Default targets the top-threat entity; mechanics
    /// that need a specific target should call `emit_effect` with an
    /// explicit `Target`.
    fn cast_boss_skill(&mut self, skill_id: u32) -> bool {
        self.emit_effect(Effect::CastSkill {
            skill_id,
            target: Target::TopThreat,
        });
        true
    }

    /// Spawn a gameplay volume.
    fn spawn_volume(
        &mut self,
        tag: String,
        shape: VolumeShape,
        anchor: VolumeAnchor,
        lifetime_ticks: Option<u32>,
        entity_filter: EntityKindFilter,
    ) {
        self.emit_effect(Effect::SpawnVolume {
            tag,
            shape,
            anchor,
            lifetime_ticks,
            entity_filter,
        });
    }

    /// Despawn all volumes matching `tag` for this encounter.
    fn despawn_volume(&mut self, tag: String) {
        self.emit_effect(Effect::DespawnVolume { tag });
    }

    /// Spawn `count` adds of the named NPC archetype, labelled with
    /// `tags` for downstream `OnEntityDied { tag }` matching.
    fn spawn_adds(&mut self, archetype: String, count: u32, tags: Vec<String>) {
        self.emit_effect(Effect::SpawnAdds {
            archetype,
            count,
            tags,
        });
    }

    /// Increment an encounter-scoped counter by `delta`.
    fn increment_counter(&mut self, name: String, delta: i64) {
        self.emit_effect(Effect::IncrementCounter { name, delta });
    }

    /// Switch the boss to a new phase.
    fn change_phase(&mut self, phase: BossPhase) {
        self.emit_effect(Effect::ChangePhase { phase });
    }

    /// Replace the boss AI ability list (composition of available skills).
    fn replace_ability_list(&mut self, ability_ids: Vec<u32>) {
        self.emit_effect(Effect::ReplaceAbilityList { ability_ids });
    }

    /// Telegraph an upcoming cast through the worker warning-event path.
    fn telegraph(&mut self, skill_id: u32, target: Target, lead_ticks: u32) {
        self.emit_effect(Effect::Telegraph {
            skill_id,
            target,
            lead_ticks,
        });
    }

    /// Free-form encounter event. Defaults to `EmitEncounterEvent`, which
    /// feeds `OnEvent` rules through the encounter bus.
    fn log_event(&mut self, event_name: &str) {
        self.emit_effect(Effect::EmitEncounterEvent {
            name: event_name.to_string(),
        });
    }
}

/// Mechanic trait — each mechanic handles its own lifecycle.
pub trait Mechanic: Send {
    fn start(&mut self, ctx: &mut dyn MechanicCtx);
    fn tick(&mut self, ctx: &mut dyn MechanicCtx);
    fn on_event(&mut self, ctx: &mut dyn MechanicCtx, event_name: &str);
    fn is_finished(&self) -> bool;
    /// Name used by the encounter bus when announcing `MechanicEnded`.
    /// Implementations should return a stable identifier matching the
    /// `StartMechanic { name }` value used to spawn them.
    fn name(&self) -> &str {
        ""
    }
    /// Outcome category for `OnMechanicEnded` triggers. Default `None`
    /// means the bus reports `MechanicOutcome::Completed`.
    fn outcome(&self) -> Option<MechanicOutcome> {
        None
    }
}

/// Outcome of a finished mechanic, surfaced through `EncounterEvent::MechanicEnded`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MechanicOutcome {
    Completed,
    Failed,
    Cancelled,
}

/// Minimal built-in mechanic — auto-finishes one tick after start.
pub struct MarkerMechanic {
    name: String,
    started_at: Option<TickId>,
    finished: bool,
}

impl MarkerMechanic {
    pub fn new(name: String) -> Self {
        Self {
            name,
            started_at: None,
            finished: false,
        }
    }
}

impl Mechanic for MarkerMechanic {
    fn start(&mut self, ctx: &mut dyn MechanicCtx) {
        self.started_at = Some(ctx.current_tick());
        self.finished = false;
    }

    fn tick(&mut self, ctx: &mut dyn MechanicCtx) {
        if let Some(started_at) = self.started_at
            && ctx.current_tick().0 > started_at.0
        {
            self.finished = true;
        }
    }

    fn on_event(&mut self, _ctx: &mut dyn MechanicCtx, event_name: &str) {
        let expected = format!("mechanic:{}:stop", self.name);
        if event_name == expected {
            self.finished = true;
        }
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// Generic timed pulse mechanic that owns encounter volumes and emits named
/// encounter events on a fixed cadence.
pub struct TimedVolumePulseMechanic {
    name: String,
    first_pulse_event: String,
    pulse_event: String,
    pulse_interval_ticks: u32,
    initial_delay_ticks: u32,
    zones: Vec<MechanicZoneParam>,
    owned_tags: Vec<String>,
    next_pulse_tick: Option<TickId>,
    pulse_count: u32,
    finished: bool,
    cleanup_sent: bool,
    outcome: Option<MechanicOutcome>,
}

impl TimedVolumePulseMechanic {
    pub fn from_params(params: &MechanicParams) -> Self {
        let first_pulse_event = params
            .string("first_pulse_event")
            .unwrap_or("timed_volume_first_pulse")
            .to_string();
        let pulse_event = params
            .string("pulse_event")
            .unwrap_or("timed_volume_pulse")
            .to_string();
        let pulse_interval_ticks = params
            .int("pulse_interval_ticks")
            .and_then(|v| u32::try_from(v.max(1)).ok())
            .unwrap_or(200);
        let initial_delay_ticks = params
            .int("initial_delay_ticks")
            .and_then(|v| u32::try_from(v.max(1)).ok())
            .unwrap_or(1);
        let mut zones = params.zones.clone();
        zones.sort_by_key(|z| (z.priority, z.tag.clone()));
        let mut owned_tags: Vec<String> = zones.iter().map(|z| z.tag.clone()).collect();
        owned_tags.sort();
        owned_tags.dedup();
        Self {
            name: "timed_volume_pulse".to_string(),
            first_pulse_event,
            pulse_event,
            pulse_interval_ticks,
            initial_delay_ticks,
            zones,
            owned_tags,
            next_pulse_tick: None,
            pulse_count: 0,
            finished: false,
            cleanup_sent: false,
            outcome: None,
        }
    }

    fn cleanup(&mut self, ctx: &mut dyn MechanicCtx) {
        if self.cleanup_sent {
            return;
        }
        self.cleanup_sent = true;
        for tag in &self.owned_tags {
            ctx.emit_effect(Effect::DespawnVolume { tag: tag.clone() });
        }
    }
}

impl Mechanic for TimedVolumePulseMechanic {
    fn start(&mut self, ctx: &mut dyn MechanicCtx) {
        self.next_pulse_tick = Some(TickId(
            ctx.current_tick()
                .0
                .saturating_add(self.initial_delay_ticks as u64),
        ));
        for zone in &self.zones {
            ctx.emit_effect(Effect::SpawnVolume {
                tag: zone.tag.clone(),
                shape: zone.shape,
                anchor: zone.anchor.clone(),
                lifetime_ticks: zone.lifetime_ticks,
                entity_filter: zone.entity_filter.clone().unwrap_or_else(|| {
                    // Default to player-only filtering — historical
                    // behavior of `timed_volume_pulse` and the only
                    // useful setting for typical "stand in / stand out"
                    // boss zones. Authors set `entity_filter` on the
                    // zone param to override (e.g. include adds).
                    EntityKindFilter::Kinds(vec![game_schema::EntityKind::Player])
                }),
            });
        }
    }

    fn tick(&mut self, ctx: &mut dyn MechanicCtx) {
        if self.finished {
            return;
        }
        let Some(next_pulse_tick) = self.next_pulse_tick else {
            return;
        };
        if ctx.current_tick().0 < next_pulse_tick.0 {
            return;
        }

        let event_name = if self.pulse_count == 0 {
            &self.first_pulse_event
        } else {
            &self.pulse_event
        };
        if !event_name.is_empty() {
            ctx.emit_effect(Effect::EmitEncounterEvent {
                name: event_name.clone(),
            });
        }
        self.pulse_count = self.pulse_count.saturating_add(1);
        self.next_pulse_tick = Some(TickId(
            ctx.current_tick()
                .0
                .saturating_add(self.pulse_interval_ticks as u64),
        ));
    }

    fn on_event(&mut self, ctx: &mut dyn MechanicCtx, event_name: &str) {
        let expected = format!("mechanic:{}:stop", self.name);
        if event_name == expected {
            self.cleanup(ctx);
            self.finished = true;
            self.outcome = Some(MechanicOutcome::Cancelled);
        }
    }

    fn is_finished(&self) -> bool {
        self.finished
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn outcome(&self) -> Option<MechanicOutcome> {
        self.outcome
    }
}

// ── Mechanic registry ───────────────────────────────────────────

type MechanicFactory = Arc<dyn Fn(&MechanicParams) -> Box<dyn Mechanic> + Send + Sync>;

/// Name → factory registry for built-in mechanics. Loaded at startup and
/// passed to the tick pipeline. Authors reference mechanics by name in RON.
pub struct MechanicRegistry {
    factories: HashMap<String, MechanicFactory>,
}

impl MechanicRegistry {
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Registry pre-populated with built-in mechanics.
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        reg.register("marker", |_params| {
            Box::new(MarkerMechanic::new("marker".to_string()))
        });
        reg.register("timed_volume_pulse", |params| {
            Box::new(TimedVolumePulseMechanic::from_params(params))
        });
        reg
    }

    pub fn register<F>(&mut self, name: &str, factory: F)
    where
        F: Fn(&MechanicParams) -> Box<dyn Mechanic> + Send + Sync + 'static,
    {
        self.factories.insert(name.to_string(), Arc::new(factory));
    }

    pub fn instantiate(&self, name: &str, params: &MechanicParams) -> Option<Box<dyn Mechanic>> {
        self.factories.get(name).map(|f| f(params))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.factories.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.factories.len()
    }

    pub fn is_empty(&self) -> bool {
        self.factories.is_empty()
    }
}

impl Default for MechanicRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

// ── Outputs ─────────────────────────────────────────────────────

/// Output actions produced by rule evaluation, consumed by the worker.
#[derive(Clone, Debug)]
pub enum EncounterOutput {
    /// Boss phase changed — commit to `boss_phase` table.
    ChangeBossPhase {
        boss_entity_id: EntityId,
        new_phase: u32,
        entered_at_tick: u64,
    },
    /// Increment a world `zone_counter` (Tier-2 commit).
    IncrementZoneCounter {
        layer: u32,
        region_x: i32,
        region_z: i32,
        counter_name: String,
        delta: f64,
    },
    /// Worker should run a scripted boss cast at the resolved target.
    CastSkill {
        boss_entity_id: EntityId,
        skill_id: u32,
        target: Target,
    },
    /// Worker should replace the boss's AI ability list.
    ReplaceAbilityList {
        boss_entity_id: EntityId,
        ability_ids: Vec<u32>,
    },
    /// Worker should resolve `name` against the `MechanicRegistry`,
    /// instantiate, `start(ctx)`, and push onto active mechanics.
    StartMechanic {
        boss_entity_id: EntityId,
        name: String,
        params: MechanicParams,
    },
    /// Worker should mark active mechanics named `name` as finished.
    StopMechanic {
        boss_entity_id: EntityId,
        name: String,
    },
    /// Worker should resolve `archetype` against `NpcArchetypeRegistry` and
    /// emit `count` adds via `DirectorSpawn` near the boss. `tags` are
    /// applied to each add through the `encounter_add` membership
    /// roundtrip (see contract `spawn_add_membership_contract.md`).
    SpawnAdds {
        boss_entity_id: EntityId,
        archetype: String,
        count: u32,
        tags: Vec<String>,
    },
    /// Telegraph output resolved by the worker into entity-target warnings or
    /// area telegraph events.
    Telegraph {
        boss_entity_id: EntityId,
        skill_id: u32,
        target: Target,
        lead_ticks: u32,
    },
    /// Worker should emit client-facing encounter presentation cues.
    EncounterCue {
        boss_entity_id: EntityId,
        target: Target,
        cue_id: String,
        anchor: EncounterCueAnchor,
        shape: EncounterCueShape,
        lead_ticks: u32,
        duration_ticks: u32,
    },
    /// Worker should apply a buff to resolved targets.
    ApplyBuff {
        boss_entity_id: EntityId,
        target: Target,
        buff_id: u32,
        mode: BuffApplyMode,
    },
    /// Worker should remove buffs from resolved targets.
    RemoveBuffs {
        boss_entity_id: EntityId,
        target: Target,
        buff_ids: Vec<u32>,
        force: bool,
    },
    /// Worker should set interactable state for every matching selector target.
    SetInteractableState {
        boss_entity_id: EntityId,
        selector: InteractableSelector,
        state: InteractableStateValue,
    },
    /// Worker should toggle interactable state for every matching selector target.
    ToggleInteractable {
        boss_entity_id: EntityId,
        selector: InteractableSelector,
    },
    /// Worker should spawn a gameplay volume.
    SpawnVolume {
        boss_entity_id: EntityId,
        tag: String,
        shape: VolumeShape,
        anchor: VolumeAnchor,
        lifetime_ticks: Option<u32>,
        entity_filter: EntityKindFilter,
    },
    /// Worker should despawn all volumes matching `tag` owned by this boss.
    DespawnVolume {
        boss_entity_id: EntityId,
        tag: String,
    },
}

// ── EncounterState ──────────────────────────────────────────────

/// Runtime state for an active boss encounter.
pub struct EncounterState {
    pub boss_entity: EntityId,
    pub phase: BossPhase,
    /// Dormant encounters are registered but do not tick mechanics or rules
    /// until the worker activates the boss arena.
    pub active: bool,
    pub activated_at_tick: Option<TickId>,
    pub rules: Vec<Rule>,
    pub active_mechanics: Vec<Box<dyn Mechanic>>,
    pub phase_start_tick: TickId,
    /// Per-encounter named counters (e.g., add kills, mechanic completions).
    pub counters: BTreeMap<String, i64>,
    /// Tracks the lowest hp-pct seen so HP-threshold rules fire only on the
    /// downward edge.
    last_hp_pct: f32,
    /// Typed event bus drained at the start of each `evaluate`.
    pub bus: EncounterBus,
    /// Parked `Sequence` tails waiting for `Wait` ticks to elapse.
    pending_continuations: Vec<Continuation>,
    /// Monotonic id stamped on each new `Sequence` for deterministic
    /// ordering across continuations.
    next_sequence_id: u64,
}

impl EncounterState {
    pub fn new(boss_entity: EntityId, rules: Vec<Rule>, start_tick: TickId) -> Self {
        let mut state = Self {
            boss_entity,
            phase: BossPhase::Phase1,
            active: true,
            activated_at_tick: Some(start_tick),
            rules,
            active_mechanics: Vec::new(),
            phase_start_tick: start_tick,
            counters: BTreeMap::new(),
            last_hp_pct: 1.0,
            bus: EncounterBus::new(),
            pending_continuations: Vec::new(),
            next_sequence_id: 0,
        };
        for rule in &mut state.rules {
            rule.hp_threshold_armed = true;
        }
        state
    }

    pub fn new_dormant(boss_entity: EntityId, rules: Vec<Rule>, start_tick: TickId) -> Self {
        let mut state = Self::new(boss_entity, rules, start_tick);
        state.active = false;
        state.activated_at_tick = None;
        state
    }

    pub fn activate(&mut self, current_tick: TickId) {
        self.active = true;
        self.activated_at_tick = Some(current_tick);
        self.phase_start_tick = current_tick;
        self.last_hp_pct = 1.0;
        self.pending_continuations.clear();
        for rule in &mut self.rules {
            rule.hp_threshold_armed = true;
        }
    }

    /// Leash reset: return an active encounter to its dormant baseline so a
    /// later arena entry re-runs [`activate`](Self::activate) from a clean
    /// slate. Used when the boss's arena goes vacant (every valid target left
    /// the layer). The worker is responsible for stopping any active mechanics
    /// through the normal stop cascade *before* calling this — `active_mechanics`
    /// is cleared here only as a safety net — and for restoring boss HP, threat,
    /// and position. Phase, counters, the event bus, and parked continuations
    /// are reset so progress does not carry across a reset.
    pub fn deactivate(&mut self) {
        self.active = false;
        self.activated_at_tick = None;
        self.phase = BossPhase::Phase1;
        self.phase_start_tick = TickId(0);
        self.active_mechanics.clear();
        self.counters.clear();
        self.bus = EncounterBus::new();
        self.pending_continuations.clear();
        self.next_sequence_id = 0;
        self.last_hp_pct = 1.0;
        // Full firing-state reset (not just re-arming HP thresholds): a leashed
        // boss must re-run `Once`/`OnEnter` setup rules and already-fired
        // progression rules when a player re-enters and re-activates it.
        for rule in &mut self.rules {
            rule.reset();
        }
    }

    /// Tick all active mechanics and prune any that have finished. Pruned
    /// mechanics push a `MechanicEnded` event into the bus so encounter
    /// rules can react next evaluate.
    pub fn tick_mechanics(&mut self, ctx: &mut dyn MechanicCtx) {
        for mechanic in &mut self.active_mechanics {
            mechanic.tick(ctx);
        }
        let mut i = 0;
        while i < self.active_mechanics.len() {
            if self.active_mechanics[i].is_finished() {
                let m = self.active_mechanics.remove(i);
                let name = m.name().to_string();
                let outcome = m.outcome().unwrap_or(MechanicOutcome::Completed);
                if !name.is_empty() {
                    self.bus
                        .push(EncounterEvent::MechanicEnded { name, outcome });
                }
            } else {
                i += 1;
            }
        }
    }

    fn eval_cond(&self, cond: &Cond, hp_pct: f32, inputs: &EncounterEvalInputs<'_>) -> bool {
        match cond {
            Cond::Always => true,
            Cond::PhaseIs { phase } => &self.phase == phase,
            Cond::HpPctCmp { op, value } => op.cmp_f32(hp_pct, *value),
            Cond::CounterCmp { name, op, value } => {
                let n = self.counters.get(name).copied().unwrap_or(0);
                op.cmp_i64(n, *value)
            }
            Cond::All { conds } => conds.iter().all(|c| self.eval_cond(c, hp_pct, inputs)),
            Cond::Any { conds } => conds.iter().any(|c| self.eval_cond(c, hp_pct, inputs)),
            Cond::Not { cond } => !self.eval_cond(cond, hp_pct, inputs),
            Cond::OccupancyCmp { tag, op, count } => {
                let n = inputs
                    .volume_occupancy_by_tag
                    .get(tag.as_str())
                    .copied()
                    .unwrap_or(0) as i64;
                op.cmp_i64(n, *count)
            }
            Cond::VolumeOccupantsExactlyOneOf {
                source_tag,
                member_tags,
            } => inputs
                .volume_occupants_by_tag
                .get(source_tag.as_str())
                .into_iter()
                .flatten()
                .all(|entity| {
                    member_tags
                        .iter()
                        .filter(|tag| {
                            inputs
                                .volume_occupants_by_tag
                                .get(tag.as_str())
                                .is_some_and(|occupants| occupants.contains(entity))
                        })
                        .count()
                        == 1
                }),
            Cond::AllVolumeOccupantsHaveBuff { tag, buff_id } => inputs
                .volume_occupants_by_tag
                .get(tag.as_str())
                .into_iter()
                .flatten()
                .all(|entity| {
                    inputs
                        .entity_buffs_by_entity
                        .get(entity)
                        .is_some_and(|buffs| buffs.contains(buff_id))
                }),
        }
    }

    /// Evaluate all rules for the current tick, returning outputs the worker
    /// applies. Side-effecting state changes (phase, counters, fired flags)
    /// happen here; outward-facing effects flow as `EncounterOutput`.
    pub fn evaluate(
        &mut self,
        boss_hp_pct: f32,
        current_tick: TickId,
        inputs: &EncounterEvalInputs<'_>,
    ) -> Vec<EncounterOutput> {
        let mut outputs = Vec::new();
        let last_hp = self.last_hp_pct;
        self.last_hp_pct = boss_hp_pct;

        // Drain any matured continuations BEFORE rule eval so their tail
        // effects observe the same tick as freshly-fired rules.
        let matured: Vec<Continuation> = {
            let (mature, pending): (Vec<_>, Vec<_>) = self
                .pending_continuations
                .drain(..)
                .partition(|c| c.resume_tick.0 <= current_tick.0);
            self.pending_continuations = pending;
            mature
        };
        // Order by sequence_id for determinism (insertion order ties broken
        // by partition's stable iteration).
        let mut matured = matured;
        matured.sort_by_key(|c| c.sequence_id);
        for cont in matured {
            for effect in cont.remaining {
                self.apply_effect(effect, current_tick, &mut outputs);
            }
        }

        // Recompute `ticks_in_phase` AFTER continuations: a matured
        // continuation may have applied `Effect::ChangePhase`, which
        // resets `phase_start_tick`. Rule triggers like `OnAfter` would
        // otherwise consult a stale value and fire too early in the new
        // phase.
        let ticks_in_phase = current_tick.0.saturating_sub(self.phase_start_tick.0) as u32;

        // Drain typed events for this tick. Triggers consult this snapshot.
        let bus_events: Vec<EncounterEvent> = self.bus.drain();

        // Snapshot counters so all rules see the same view this tick.
        let counters_snapshot: BTreeMap<String, i64> = self.counters.clone();
        // Indices we'll process in input order; collect first to avoid
        // borrow conflicts when applying effects that mutate `self`.
        // Each entry carries (rule_index, fire_count) so that triggers
        // matching multiple events in one tick (e.g. four tagged adds dying
        // simultaneously under `OnEntityDied`) fire the rule once per match
        // rather than collapsing to a single boolean. RepeatPolicy gating is
        // re-applied per occurrence at fire time.
        let rule_count = self.rules.len();
        let mut to_fire: Vec<(usize, u32)> = Vec::new();

        for (idx, rule) in self.rules.iter_mut().enumerate() {
            if !rule.can_fire() {
                continue;
            }

            let (triggered, occurrences) = match &rule.when {
                Trigger::OnEnter { phase } => (&self.phase == phase && rule.fire_count == 0, 1u32),
                Trigger::OnHpBelow { percent } => (
                    rule.hp_threshold_armed && last_hp >= *percent && boss_hp_pct < *percent,
                    1u32,
                ),
                Trigger::OnAfter { ticks } => (ticks_in_phase >= *ticks, 1u32),
                Trigger::OnEvery { ticks } => {
                    let fires = match rule.last_fire_tick {
                        None => ticks_in_phase >= *ticks,
                        Some(last) => current_tick.0.saturating_sub(last.0) as u32 >= *ticks,
                    };
                    (fires, 1u32)
                }
                Trigger::OnEvent { event_name } => {
                    let count = bus_events
                        .iter()
                        .filter(
                            |e| matches!(e, EncounterEvent::Custom { name } if name == event_name),
                        )
                        .count() as u32;
                    (count > 0, count)
                }
                Trigger::OnEntityDied { tag } => {
                    let count = bus_events
                        .iter()
                        .filter(|e| {
                            matches!(
                                e,
                                EncounterEvent::EntityDied { tags, .. } if tags.iter().any(|t| t == tag)
                            )
                        })
                        .count() as u32;
                    (count > 0, count)
                }
                Trigger::OnMechanicEnded { name } => {
                    let count = bus_events
                        .iter()
                        .filter(|e| {
                            matches!(e, EncounterEvent::MechanicEnded { name: n, .. } if n == name)
                        })
                        .count() as u32;
                    (count > 0, count)
                }
                Trigger::OnCounter { name, op, value } => {
                    let n = counters_snapshot.get(name).copied().unwrap_or(0);
                    let now_true = op.cmp_i64(n, *value);
                    let edge = now_true && !rule.counter_was_true;
                    rule.counter_was_true = now_true;
                    (edge, 1u32)
                }
                Trigger::OnVolumeEnter { tag } => {
                    let count = inputs
                        .volume_events
                        .iter()
                        .filter(|e| matches!(e, VolumeRuleEvent::Enter { tag: t, .. } if t == tag))
                        .count() as u32;
                    (count > 0, count)
                }
                Trigger::OnVolumeExit { tag } => {
                    let count = inputs
                        .volume_events
                        .iter()
                        .filter(|e| matches!(e, VolumeRuleEvent::Exit { tag: t, .. } if t == tag))
                        .count() as u32;
                    (count > 0, count)
                }
            };

            if triggered {
                to_fire.push((idx, occurrences.max(1)));
            }
        }

        for (idx, occurrences) in to_fire {
            for _ in 0..occurrences {
                // Re-check can_fire so RepeatPolicy::Once still caps the rule
                // at a single firing even when multiple matching events
                // arrived in one tick.
                if !self.rules[idx].can_fire() {
                    break;
                }

                // Re-borrow per-rule to evaluate cond against current `self`.
                let cond_ok = {
                    let rule = &self.rules[idx];
                    self.eval_cond(&rule.cond, boss_hp_pct, inputs)
                };
                if !cond_ok {
                    continue;
                }

                // Snapshot the rule's effects + bookkeeping.
                let (effects, rule_id) = {
                    let rule = &mut self.rules[idx];
                    rule.fire_count = rule.fire_count.saturating_add(1);
                    rule.last_fire_tick = Some(current_tick);
                    rule.hp_threshold_armed = false;
                    (rule.effects.clone(), rule.id.clone())
                };

                log::info!(
                    "Encounter: boss {} rule '{}' fired at tick {} (phase {:?})",
                    self.boss_entity.0,
                    rule_id,
                    current_tick.0,
                    self.phase,
                );

                for effect in effects {
                    self.apply_effect(effect, current_tick, &mut outputs);
                }
            }
        }

        let _ = rule_count;
        outputs
    }

    /// Apply a sequence of effects emitted from outside the rule system
    /// (e.g., by a mechanic via `MechanicCtx::emit_effect`). The effects
    /// are routed through the same handler used by rules, so behavior is
    /// identical (counter mutations take effect, phase changes re-arm
    /// rules, outputs queue for the worker to consume).
    pub fn apply_external_effects(
        &mut self,
        effects: Vec<Effect>,
        current_tick: TickId,
    ) -> Vec<EncounterOutput> {
        let mut outputs = Vec::new();
        for effect in effects {
            self.apply_effect(effect, current_tick, &mut outputs);
        }
        outputs
    }

    fn apply_effect(
        &mut self,
        effect: Effect,
        current_tick: TickId,
        outputs: &mut Vec<EncounterOutput>,
    ) {
        match effect {
            Effect::ChangePhase { phase } => {
                let old_phase = self.phase.clone();
                self.phase = phase.clone();
                self.phase_start_tick = current_tick;
                for rule in &mut self.rules {
                    rule.re_arm_on_phase_entry();
                }
                outputs.push(EncounterOutput::ChangeBossPhase {
                    boss_entity_id: self.boss_entity,
                    new_phase: phase.to_phase_number(),
                    entered_at_tick: current_tick.0,
                });
                log::info!(
                    "Encounter: boss {} phase {:?} → {:?} at tick {}",
                    self.boss_entity.0,
                    old_phase,
                    phase,
                    current_tick.0
                );
            }
            Effect::CastSkill { skill_id, target } => {
                outputs.push(EncounterOutput::CastSkill {
                    boss_entity_id: self.boss_entity,
                    skill_id,
                    target,
                });
            }
            Effect::ReplaceAbilityList { ability_ids } => {
                outputs.push(EncounterOutput::ReplaceAbilityList {
                    boss_entity_id: self.boss_entity,
                    ability_ids,
                });
            }
            Effect::StartMechanic { name, params } => {
                outputs.push(EncounterOutput::StartMechanic {
                    boss_entity_id: self.boss_entity,
                    name,
                    params,
                });
            }
            Effect::StopMechanic { name } => {
                outputs.push(EncounterOutput::StopMechanic {
                    boss_entity_id: self.boss_entity,
                    name,
                });
            }
            Effect::SpawnAdds {
                archetype,
                count,
                tags,
            } => {
                outputs.push(EncounterOutput::SpawnAdds {
                    boss_entity_id: self.boss_entity,
                    archetype,
                    count,
                    tags,
                });
            }
            Effect::Telegraph {
                skill_id,
                target,
                lead_ticks,
            } => {
                outputs.push(EncounterOutput::Telegraph {
                    boss_entity_id: self.boss_entity,
                    skill_id,
                    target,
                    lead_ticks,
                });
            }
            Effect::EncounterCue {
                target,
                cue_id,
                anchor,
                shape,
                lead_ticks,
                duration_ticks,
            } => {
                outputs.push(EncounterOutput::EncounterCue {
                    boss_entity_id: self.boss_entity,
                    target,
                    cue_id,
                    anchor,
                    shape,
                    lead_ticks,
                    duration_ticks,
                });
            }
            Effect::ApplyBuff {
                target,
                buff_id,
                mode,
            } => {
                outputs.push(EncounterOutput::ApplyBuff {
                    boss_entity_id: self.boss_entity,
                    target,
                    buff_id,
                    mode,
                });
            }
            Effect::RemoveBuffs {
                target,
                buff_ids,
                force,
            } => {
                outputs.push(EncounterOutput::RemoveBuffs {
                    boss_entity_id: self.boss_entity,
                    target,
                    buff_ids,
                    force,
                });
            }
            Effect::SetInteractableState { selector, state } => {
                outputs.push(EncounterOutput::SetInteractableState {
                    boss_entity_id: self.boss_entity,
                    selector,
                    state,
                });
            }
            Effect::ToggleInteractable { selector } => {
                outputs.push(EncounterOutput::ToggleInteractable {
                    boss_entity_id: self.boss_entity,
                    selector,
                });
            }
            Effect::IncrementCounter { name, delta } => {
                let entry = self.counters.entry(name.clone()).or_insert(0);
                *entry = entry.saturating_add(delta);
                log::debug!(
                    "Encounter: boss {} counter '{}' += {} (now {})",
                    self.boss_entity.0,
                    name,
                    delta,
                    *entry,
                );
            }
            Effect::IncrementZoneCounter {
                layer,
                region_x,
                region_z,
                counter_name,
                delta,
            } => {
                outputs.push(EncounterOutput::IncrementZoneCounter {
                    layer,
                    region_x,
                    region_z,
                    counter_name,
                    delta,
                });
            }
            Effect::EmitEncounterEvent { name } => {
                log::info!(
                    "Encounter: boss {} emit event '{}' at tick {}",
                    self.boss_entity.0,
                    name,
                    current_tick.0,
                );
                self.bus.push(EncounterEvent::Custom { name });
            }
            Effect::SpawnVolume {
                tag,
                shape,
                anchor,
                lifetime_ticks,
                entity_filter,
            } => {
                outputs.push(EncounterOutput::SpawnVolume {
                    boss_entity_id: self.boss_entity,
                    tag,
                    shape,
                    anchor,
                    lifetime_ticks,
                    entity_filter,
                });
            }
            Effect::DespawnVolume { tag } => {
                outputs.push(EncounterOutput::DespawnVolume {
                    boss_entity_id: self.boss_entity,
                    tag,
                });
            }
            Effect::Wait { .. } => {
                // Bare `Wait` outside a `Sequence` is a no-op; only the
                // `Sequence` arm consumes wait semantics to park its tail.
            }
            Effect::Sequence { steps } => {
                self.apply_sequence(steps, current_tick, outputs);
            }
            Effect::Parallel { steps } => {
                for step in steps {
                    self.apply_effect(step, current_tick, outputs);
                }
            }
        }
    }

    /// Apply `steps` in order, parking the tail at the first `Wait` as a
    /// pending continuation. Steps before any `Wait` apply this tick.
    fn apply_sequence(
        &mut self,
        steps: Vec<Effect>,
        current_tick: TickId,
        outputs: &mut Vec<EncounterOutput>,
    ) {
        let mut iter = steps.into_iter();
        while let Some(step) = iter.next() {
            match step {
                Effect::Wait { ticks } => {
                    let remaining: Vec<Effect> = iter.collect();
                    if remaining.is_empty() {
                        return;
                    }
                    if self.pending_continuations.len() >= MAX_PENDING_CONTINUATIONS {
                        log::warn!(
                            "Encounter: boss {} dropped Sequence tail (continuation cap {} reached)",
                            self.boss_entity.0,
                            MAX_PENDING_CONTINUATIONS,
                        );
                        return;
                    }
                    let resume_tick = TickId(current_tick.0.saturating_add(ticks as u64));
                    let sequence_id = self.next_sequence_id;
                    self.next_sequence_id = self.next_sequence_id.saturating_add(1);
                    self.pending_continuations.push(Continuation {
                        sequence_id,
                        resume_tick,
                        remaining,
                    });
                    return;
                }
                other => self.apply_effect(other, current_tick, outputs),
            }
        }
    }
}

/// Per-tick inputs threaded into `EncounterState::evaluate` for things the
/// encounter doesn't own (volume occupancy, edge events). Borrowed for the
/// duration of one evaluate call.
#[derive(Default)]
pub struct EncounterEvalInputs<'a> {
    /// Map of `volume.tag` → number of distinct entities currently inside
    /// any volume with that tag.
    pub volume_occupancy_by_tag: HashMap<&'a str, u32>,
    /// Map of `volume.tag` → distinct current occupants for rule-visible
    /// set membership checks.
    pub volume_occupants_by_tag: HashMap<&'a str, Vec<EntityId>>,
    /// Map of entity → active buff ids for entities sampled by encounter
    /// volumes this tick.
    pub entity_buffs_by_entity: HashMap<EntityId, BTreeSet<u32>>,
    /// Volume edge events emitted on the previous occupant-sync step. The
    /// worker drains these into the encounter for one evaluate, then clears.
    pub volume_events: &'a [VolumeRuleEvent],
}

impl<'a> EncounterEvalInputs<'a> {
    pub fn empty() -> Self {
        EncounterEvalInputs {
            volume_occupancy_by_tag: HashMap::new(),
            volume_occupants_by_tag: HashMap::new(),
            entity_buffs_by_entity: HashMap::new(),
            volume_events: &[],
        }
    }
}

/// Volume edge events visible to encounter rules this tick.
#[derive(Clone, Debug, PartialEq)]
pub enum VolumeRuleEvent {
    Enter {
        tag: String,
        entity: EntityId,
        volume_id: VolumeId,
    },
    Exit {
        tag: String,
        entity: EntityId,
        volume_id: VolumeId,
    },
}

/// Typed events visible to `OnEvent`/`OnEntityDied`/`OnMechanicEnded`
/// triggers. The worker pushes events into the bus during its event
/// fan-out phase; the encounter drains the bus at the start of each
/// `evaluate` call so triggers see one tick worth of events.
#[derive(Clone, Debug, PartialEq)]
pub enum EncounterEvent {
    /// An entity died this tick. `tags` carries any opt-in classification
    /// strings the worker attached to the entity (e.g. add archetype name).
    EntityDied { entity: EntityId, tags: Vec<String> },
    /// An entity took damage this tick. Reserved for richer triggers.
    EntityDamaged {
        source: EntityId,
        target: EntityId,
        amount: f32,
    },
    /// A mechanic finished and was pruned this tick.
    MechanicEnded {
        name: String,
        outcome: MechanicOutcome,
    },
    /// A custom named event raised via `MechanicCtx::log_event` /
    /// `Effect::EmitEncounterEvent`.
    Custom { name: String },
    /// Mirrors `VolumeRuleEvent::Enter` for trigger uniformity.
    VolumeEnter {
        tag: String,
        entity: EntityId,
        volume_id: VolumeId,
    },
    /// Mirrors `VolumeRuleEvent::Exit` for trigger uniformity.
    VolumeExit {
        tag: String,
        entity: EntityId,
        volume_id: VolumeId,
    },
}

/// Single-tick FIFO of encounter events. Owned by `EncounterState` and
/// drained at the start of each `evaluate`.
#[derive(Clone, Debug, Default)]
pub struct EncounterBus {
    events: Vec<EncounterEvent>,
}

impl EncounterBus {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, event: EncounterEvent) {
        self.events.push(event);
    }
    pub fn drain(&mut self) -> Vec<EncounterEvent> {
        std::mem::take(&mut self.events)
    }
    pub fn len(&self) -> usize {
        self.events.len()
    }
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// Maximum number of pending continuations a single encounter may queue.
///
/// Authoring note: each `Sequence` that reaches a `Wait` parks one tail here
/// until it resumes. Extra tails are dropped once the cap is reached, bounding
/// memory and protecting the worker from runaway scripted chains.
pub const MAX_PENDING_CONTINUATIONS: usize = 32;

/// Parked tail of a `Sequence` waiting for a `Wait` to elapse before its
/// remaining steps fire. Owned by `EncounterState`.
#[derive(Clone, Debug)]
struct Continuation {
    sequence_id: u64,
    resume_tick: TickId,
    remaining: Vec<Effect>,
}

// ── Files / registry ────────────────────────────────────────────

/// On-disk format for encounter definitions.
#[derive(Clone, Debug, Deserialize)]
pub struct EncounterFile {
    pub encounters: Vec<EncounterScript>,
}

/// One named encounter script.
#[derive(Clone, Debug, Deserialize)]
pub struct EncounterScript {
    pub name: String,
    #[serde(default)]
    pub loot_table_id: Option<String>,
    pub rules: Vec<Rule>,
}

#[derive(Clone, Debug)]
struct EncounterDefinition {
    rules: Vec<Rule>,
    loot_table_id: Option<String>,
}

/// Registry of encounter scripts keyed by name.
pub struct EncounterRegistry {
    defs: HashMap<String, EncounterDefinition>,
}

impl EncounterRegistry {
    pub fn new() -> Self {
        Self {
            defs: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: String, rules: Vec<Rule>) {
        self.register_with_loot(name, rules, None);
    }

    pub fn register_script(&mut self, script: EncounterScript) {
        self.register_with_loot(script.name, script.rules, script.loot_table_id);
    }

    pub fn register_with_loot(
        &mut self,
        name: String,
        rules: Vec<Rule>,
        loot_table_id: Option<String>,
    ) {
        self.defs.insert(
            name,
            EncounterDefinition {
                rules,
                loot_table_id,
            },
        );
    }

    pub fn get(&self, name: &str) -> Option<&Vec<Rule>> {
        self.defs.get(name).map(|def| &def.rules)
    }

    pub fn rules_for(&self, name: &str) -> Option<Vec<Rule>> {
        self.defs.get(name).map(|def| def.rules.clone())
    }

    pub fn loot_table_for(&self, name: &str) -> Option<&str> {
        self.defs
            .get(name)
            .and_then(|def| def.loot_table_id.as_deref())
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

impl Default for EncounterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, when: Trigger, effects: Vec<Effect>, repeat: RepeatPolicy) -> Rule {
        Rule {
            id: id.to_string(),
            when,
            cond: Cond::Always,
            effects,
            repeat,
            fire_count: 0,
            last_fire_tick: None,
            hp_threshold_armed: true,
            counter_was_true: false,
        }
    }

    #[test]
    fn register_script_preserves_loot_table_metadata() {
        let mut registry = EncounterRegistry::new();
        registry.register_script(EncounterScript {
            name: "crucible_warden".to_string(),
            loot_table_id: Some("warden_boss_drops".to_string()),
            rules: vec![rule(
                "p2",
                Trigger::OnHpBelow { percent: 0.5 },
                vec![Effect::ChangePhase {
                    phase: BossPhase::Phase2,
                }],
                RepeatPolicy::Once,
            )],
        });

        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.loot_table_for("crucible_warden"),
            Some("warden_boss_drops")
        );
        assert_eq!(registry.get("crucible_warden").expect("rules").len(), 1);
    }

    #[test]
    fn hp_threshold_fires_on_downward_edge_once() {
        let rules = vec![rule(
            "p2",
            Trigger::OnHpBelow { percent: 0.5 },
            vec![Effect::ChangePhase {
                phase: BossPhase::Phase2,
            }],
            RepeatPolicy::Once,
        )];
        let mut enc = EncounterState::new(EntityId(1), rules, TickId(0));

        // First tick at full HP arms threshold; no fire.
        let out = enc.evaluate(0.9, TickId(1), &EncounterEvalInputs::empty());
        assert!(out.is_empty());

        // Drop below 50% → fires.
        let out = enc.evaluate(0.4, TickId(2), &EncounterEvalInputs::empty());
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            EncounterOutput::ChangeBossPhase { new_phase: 2, .. }
        ));
        assert_eq!(enc.phase, BossPhase::Phase2);

        // Already fired — does not re-fire.
        let out = enc.evaluate(0.3, TickId(3), &EncounterEvalInputs::empty());
        assert!(out.is_empty());

        // Even bouncing back above and below: Once means once.
        let out = enc.evaluate(0.6, TickId(4), &EncounterEvalInputs::empty());
        assert!(out.is_empty());
        let out = enc.evaluate(0.2, TickId(5), &EncounterEvalInputs::empty());
        assert!(out.is_empty());
    }

    #[test]
    fn timer_after_phase_start() {
        let rules = vec![rule(
            "enrage",
            Trigger::OnAfter { ticks: 100 },
            vec![Effect::ChangePhase {
                phase: BossPhase::Enrage,
            }],
            RepeatPolicy::Once,
        )];
        let mut enc = EncounterState::new(EntityId(2), rules, TickId(50));

        let out = enc.evaluate(1.0, TickId(149), &EncounterEvalInputs::empty());
        assert!(out.is_empty());
        let out = enc.evaluate(1.0, TickId(150), &EncounterEvalInputs::empty());
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn cond_phase_gates_effect() {
        let rules = vec![Rule {
            cond: Cond::PhaseIs {
                phase: BossPhase::Phase2,
            },
            ..rule(
                "phase2_only_cast",
                Trigger::OnAfter { ticks: 0 },
                vec![Effect::CastSkill {
                    skill_id: 24,
                    target: Target::TopThreat,
                }],
                RepeatPolicy::Once,
            )
        }];
        let mut enc = EncounterState::new(EntityId(3), rules, TickId(0));
        // Phase1 — gate fails.
        let out = enc.evaluate(1.0, TickId(0), &EncounterEvalInputs::empty());
        assert!(out.is_empty());
        // Manually advance to Phase2 and re-evaluate.
        enc.phase = BossPhase::Phase2;
        let out = enc.evaluate(1.0, TickId(1), &EncounterEvalInputs::empty());
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], EncounterOutput::CastSkill { .. }));
    }

    #[test]
    fn cond_volume_occupants_exactly_one_of_checks_membership() {
        let enc = EncounterState::new(EntityId(33), Vec::new(), TickId(0));
        let cond = Cond::VolumeOccupantsExactlyOneOf {
            source_tag: "arena".to_string(),
            member_tags: vec!["near".to_string(), "far".to_string()],
        };

        let p1 = EntityId(101);
        let p2 = EntityId(102);
        let mut inputs = EncounterEvalInputs::empty();
        inputs.volume_occupants_by_tag.insert("arena", vec![p1, p2]);
        inputs.volume_occupants_by_tag.insert("near", vec![p1]);
        inputs.volume_occupants_by_tag.insert("far", vec![p2]);
        assert!(enc.eval_cond(&cond, 1.0, &inputs));

        inputs.volume_occupants_by_tag.insert("near", Vec::new());
        inputs.volume_occupants_by_tag.insert("far", Vec::new());
        assert!(
            !enc.eval_cond(&cond, 1.0, &inputs),
            "arena occupant in no member zone is invalid"
        );

        inputs.volume_occupants_by_tag.insert("near", vec![p1]);
        inputs.volume_occupants_by_tag.insert("far", vec![p1]);
        assert!(
            !enc.eval_cond(&cond, 1.0, &inputs),
            "arena occupant in multiple member zones is invalid"
        );
    }

    #[test]
    fn cond_all_volume_occupants_have_buff_checks_snapshot() {
        let enc = EncounterState::new(EntityId(34), Vec::new(), TickId(0));
        let cond = Cond::AllVolumeOccupantsHaveBuff {
            tag: "near".to_string(),
            buff_id: 800,
        };
        let p1 = EntityId(201);
        let p2 = EntityId(202);
        let mut inputs = EncounterEvalInputs::empty();
        inputs.volume_occupants_by_tag.insert("near", vec![p1, p2]);
        inputs
            .entity_buffs_by_entity
            .insert(p1, BTreeSet::from([800, 900]));
        inputs
            .entity_buffs_by_entity
            .insert(p2, BTreeSet::from([800]));
        assert!(enc.eval_cond(&cond, 1.0, &inputs));

        inputs
            .entity_buffs_by_entity
            .insert(p2, BTreeSet::from([801]));
        assert!(!enc.eval_cond(&cond, 1.0, &inputs));
    }

    #[test]
    fn counter_increment_and_trigger_edge() {
        let rules = vec![
            rule(
                "tick0_inc",
                Trigger::OnAfter { ticks: 0 },
                vec![Effect::IncrementCounter {
                    name: "kills".to_string(),
                    delta: 4,
                }],
                RepeatPolicy::Once,
            ),
            rule(
                "phase_at_4",
                Trigger::OnCounter {
                    name: "kills".to_string(),
                    op: CmpOp::Ge,
                    value: 4,
                },
                vec![Effect::ChangePhase {
                    phase: BossPhase::Phase2,
                }],
                RepeatPolicy::Once,
            ),
        ];
        let mut enc = EncounterState::new(EntityId(4), rules, TickId(0));

        let out = enc.evaluate(1.0, TickId(0), &EncounterEvalInputs::empty());
        // tick0_inc fires (no commit output for IncrementCounter), kills→4.
        // counter rule's snapshot at start-of-tick saw kills=0 so it does
        // NOT fire on this tick.
        assert!(out.is_empty());
        assert_eq!(enc.counters.get("kills").copied(), Some(4));

        let out = enc.evaluate(1.0, TickId(1), &EncounterEvalInputs::empty());
        assert!(
            out.iter()
                .any(|o| matches!(o, EncounterOutput::ChangeBossPhase { new_phase: 2, .. }))
        );
    }

    #[test]
    fn every_n_ticks_with_max_fires() {
        let rules = vec![rule(
            "tick",
            Trigger::OnEvery { ticks: 5 },
            vec![Effect::EmitEncounterEvent {
                name: "tick".to_string(),
            }],
            RepeatPolicy::EveryNTicks {
                interval: 5,
                max_fires: Some(2),
            },
        )];
        let mut enc = EncounterState::new(EntityId(5), rules, TickId(0));

        // Tick 5 → fires (ticks_in_phase ≥ 5, never fired).
        let out = enc.evaluate(1.0, TickId(5), &EncounterEvalInputs::empty());
        assert!(!out.is_empty() == false || enc.rules[0].fire_count == 1);
        // Tick 9 → still <5 since last fire (5).
        let _ = enc.evaluate(1.0, TickId(9), &EncounterEvalInputs::empty());
        // Tick 10 → 5 ticks since last fire → 2nd fire (cap).
        let _ = enc.evaluate(1.0, TickId(10), &EncounterEvalInputs::empty());
        // Tick 15 → cap reached, no more.
        let _ = enc.evaluate(1.0, TickId(15), &EncounterEvalInputs::empty());

        assert_eq!(enc.rules[0].fire_count, 2);
    }

    #[test]
    fn parses_shipped_encounters_ron() {
        let file: EncounterFile = ron::from_str(include_str!("../../../../data/encounters.ron"))
            .expect("shipped encounters.ron should parse");
        assert!(file.encounters.iter().any(|def| def.name == "default"));
        let all_rules: Vec<&Rule> = file.encounters.iter().flat_map(|d| &d.rules).collect();
        assert!(
            all_rules
                .iter()
                .any(|r| matches!(r.when, Trigger::OnHpBelow { .. }))
        );
        assert!(all_rules.iter().any(|r| {
            r.effects
                .iter()
                .any(|e| matches!(e, Effect::ChangePhase { .. }))
        }));
        assert!(all_rules.iter().any(|r| {
            r.effects
                .iter()
                .any(|e| matches!(e, Effect::CastSkill { .. }))
        }));
        assert!(all_rules.iter().any(|r| {
            r.effects
                .iter()
                .any(|e| matches!(e, Effect::SpawnAdds { .. }))
        }));
    }

    #[test]
    fn mechanic_registry_builtins_have_marker() {
        let reg = MechanicRegistry::with_builtins();
        assert!(reg.contains("marker"));
        assert!(reg.contains("timed_volume_pulse"));
        assert!(!reg.contains("manayas_core"));
        let m = reg
            .instantiate("marker", &MechanicParams::empty())
            .expect("marker instantiates");
        let _ = m;
    }

    /// Test-only `MechanicCtx` impl for unit testing without a pipeline.
    struct FakeMechanicCtx {
        tick: TickId,
        boss: EntityId,
        emitted: Vec<Effect>,
    }

    impl FakeMechanicCtx {
        fn new(tick: TickId, boss: EntityId) -> Self {
            Self {
                tick,
                boss,
                emitted: Vec::new(),
            }
        }
    }

    impl MechanicCtx for FakeMechanicCtx {
        fn current_tick(&self) -> TickId {
            self.tick
        }
        fn boss_entity_id(&self) -> EntityId {
            self.boss
        }
        fn emit_effect(&mut self, effect: Effect) {
            self.emitted.push(effect);
        }
    }

    #[test]
    fn marker_mechanic_via_factory_starts_and_expires() {
        let reg = MechanicRegistry::with_builtins();
        let mut ctx = FakeMechanicCtx::new(TickId(0), EntityId(99));
        let mut m = reg
            .instantiate("marker", &MechanicParams::empty())
            .expect("marker registered");
        m.start(&mut ctx);
        assert!(!m.is_finished());
        ctx.tick = TickId(1);
        m.tick(&mut ctx);
        assert!(m.is_finished());
    }

    #[test]
    fn timed_volume_pulse_spawns_volumes_and_emits_named_pulses() {
        let reg = MechanicRegistry::with_builtins();
        let mut params = MechanicParams::empty();
        params.ints.insert("initial_delay_ticks".to_string(), 1);
        params.ints.insert("pulse_interval_ticks".to_string(), 2);
        params
            .strings
            .insert("first_pulse_event".to_string(), "first".to_string());
        params
            .strings
            .insert("pulse_event".to_string(), "again".to_string());
        params.zones.push(MechanicZoneParam {
            tag: "arena".to_string(),
            shape: VolumeShape::Sphere { radius: 5.0 },
            anchor: VolumeAnchor::FollowBoss,
            priority: 0,
            buff_id: None,
            lifetime_ticks: None,
            entity_filter: None,
        });

        let mut ctx = FakeMechanicCtx::new(TickId(10), EntityId(99));
        let mut m = reg
            .instantiate("timed_volume_pulse", &params)
            .expect("timed pulse instantiates");
        m.start(&mut ctx);
        assert!(matches!(
            ctx.emitted.first(),
            Some(Effect::SpawnVolume { tag, .. }) if tag == "arena"
        ));

        ctx.emitted.clear();
        m.tick(&mut ctx);
        assert!(ctx.emitted.is_empty(), "initial delay should hold pulse");

        ctx.tick = TickId(11);
        m.tick(&mut ctx);
        assert!(matches!(
            ctx.emitted.as_slice(),
            [Effect::EmitEncounterEvent { name }] if name == "first"
        ));

        ctx.emitted.clear();
        ctx.tick = TickId(13);
        m.tick(&mut ctx);
        assert!(matches!(
            ctx.emitted.as_slice(),
            [Effect::EmitEncounterEvent { name }] if name == "again"
        ));

        ctx.emitted.clear();
        m.on_event(&mut ctx, "mechanic:timed_volume_pulse:stop");
        assert!(m.is_finished());
        assert!(matches!(
            ctx.emitted.as_slice(),
            [Effect::DespawnVolume { tag }] if tag == "arena"
        ));
    }

    #[test]
    fn start_mechanic_emits_request_output_only() {
        let rules = vec![rule(
            "spawn_marker",
            Trigger::OnAfter { ticks: 0 },
            vec![Effect::StartMechanic {
                name: "marker".to_string(),
                params: MechanicParams::empty(),
            }],
            RepeatPolicy::Once,
        )];
        let mut enc = EncounterState::new(EntityId(7), rules, TickId(0));
        let out = enc.evaluate(1.0, TickId(0), &EncounterEvalInputs::empty());
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            EncounterOutput::StartMechanic { name, .. } if name == "marker"
        ));
        assert!(
            enc.active_mechanics.is_empty(),
            "evaluate() must not instantiate mechanics — worker handles via factory"
        );
    }

    #[test]
    fn mechanic_ctx_emit_effect_round_trips_through_apply_external_effects() {
        // A mechanic accumulates effects via `emit_effect`; the encounter
        // routes them through `apply_external_effects` and produces the
        // same `EncounterOutput`s a rule would.
        let mut ctx = FakeMechanicCtx::new(TickId(0), EntityId(7));
        ctx.spawn_volume(
            "arena".to_string(),
            VolumeShape::Sphere { radius: 5.0 },
            VolumeAnchor::Boss,
            None,
            crate::volume::EntityKindFilter::Any,
        );
        ctx.cast_boss_skill(42);
        ctx.change_phase(BossPhase::Phase2);

        assert_eq!(ctx.emitted.len(), 3);

        let mut enc = EncounterState::new(EntityId(7), Vec::new(), TickId(0));
        let outs = enc.apply_external_effects(ctx.emitted, TickId(1));

        assert!(matches!(
            outs[0],
            EncounterOutput::SpawnVolume { ref tag, .. } if tag == "arena"
        ));
        assert!(matches!(
            outs[1],
            EncounterOutput::CastSkill { skill_id: 42, .. }
        ));
        assert!(matches!(
            outs[2],
            EncounterOutput::ChangeBossPhase { new_phase: 2, .. }
        ));
        // ChangePhase is also reflected on the state.
        assert_eq!(enc.phase, BossPhase::Phase2);
    }
}
