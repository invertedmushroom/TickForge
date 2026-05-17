use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;

#[cfg(feature = "connected")]
use crate::input::SkillBindings;

pub use crate::ability_visuals::{AbilityShape, ClientTargetingMode, SkillMenuCategory};

pub struct AbilityBarPlugin;

impl Plugin for AbilityBarPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AbilityCooldowns>();
        app.init_resource::<SkillMenuState>();
        app.add_systems(Startup, (spawn_ability_bar, spawn_skill_menu));
        app.add_systems(Update, (
            track_cooldowns,
            update_ability_bar,
            handle_slot_clicks,
            handle_menu_clicks,
            handle_filter_clicks,
            handle_scroll_controls,
            update_skill_menu_visibility,
        ));
    }
}

/// Reserved ability ID for block — assignable to any skill slot.
pub const BLOCK_ABILITY_ID: u32 = 100;

#[derive(Clone)]
pub struct AbilityDef {
    pub id: u32,
    pub name: String,
    pub damage_type: game_schema::DamageType,
    pub on_hit_buffs: Vec<u32>,
    pub stun_ticks: u32,
    pub knockdown_ticks: u32,
    pub sleep_ticks: u32,
    pub silence_ticks: u32,
    pub fear_ticks: u32,
    pub category: SkillMenuCategory,
    pub cooldown_ticks: u32,
    pub color: Color,
    pub shape: AbilityShape,
    /// Hitbox offset in entity-local space matching abilities.ron (x=right, y=up, z=forward).
    pub offset: [f32; 3],
    /// Client-side targeting mode — determines how this ability constructs its
    /// `AbilityTarget` before sending to the server.
    pub targeting: ClientTargetingMode,
    /// Client-side max range hint used for target clamping and preview feel.
    /// Must stay aligned with the server ability data when set.
    pub max_range: Option<f32>,
    /// Projectile speed in units/tick from abilities.ron.
    pub projectile_speed: Option<f32>,
    /// Lock-on session timeout in ticks from abilities.ron.
    pub lock_on_timeout_ticks: Option<u32>,
    /// How many ticks the hitbox persists (RemoveHitbox - SpawnHitbox in the timeline).
    /// 0 means single-frame or projectile (no lingering hitbox visual).
    pub linger_ticks: u32,
    /// Non-zero when the ability re-damages periodically (e.g. every 20 ticks).
    pub damage_interval_ticks: u32,
    /// Optional charge thresholds parsed from abilities.ron for UI/tooltips.
    pub charge_tiers: Vec<game_core::combat::skill::ChargeTierDef>,
}

static ABILITY_DEFS: std::sync::OnceLock<Vec<AbilityDef>> = std::sync::OnceLock::new();

/// Returns the full ability catalog, parsed from the embedded `data/abilities.ron`
/// at first call and cached for the lifetime of the process.
pub fn all_abilities() -> &'static [AbilityDef] {
    ABILITY_DEFS.get_or_init(build_ability_defs)
}

fn build_ability_defs() -> Vec<AbilityDef> {
    use game_core::combat::skill::{AbilityAction, AbilityFile};

    let src = include_str!("../../../data/abilities.ron");
    let file = ron::from_str::<AbilityFile>(src)
        .expect("data/abilities.ron embedded at compile time must be valid RON");

    let mut defs: Vec<AbilityDef> = file
        .abilities
        .iter()
        .map(|data| {
            let (category, color) = crate::ability_visuals::visual_for(data.ability_id);

            let timeline = file
                .timelines
                .iter()
                .find(|t| t.ability_id == data.ability_id);

            let cooldown_ticks = timeline
                .and_then(|t| {
                    t.actions.iter().find_map(|a| match &a.action {
                        AbilityAction::CooldownStart { duration_ticks } => Some(*duration_ticks),
                        _ => None,
                    })
                })
                .unwrap_or(0);

            let timeline = file
                .timelines
                .iter()
                .find(|t| t.ability_id == data.ability_id);

            let (shape, offset) = timeline
                .and_then(|t| {
                    t.actions.iter().find_map(|a| match &a.action {
                        AbilityAction::SpawnHitbox { shape, offset } => {
                            Some((skill_shape_to_client(*shape), [offset.x, offset.y, offset.z]))
                        }
                        _ => None,
                    })
                })
                .unwrap_or((AbilityShape::None, [0.0, 0.0, 0.0]));

            // Compute linger duration from timeline: RemoveHitbox - SpawnHitbox.
            let linger_ticks = timeline
                .map(|t| {
                    let spawn = t.actions.iter().find_map(|a| match &a.action {
                        AbilityAction::SpawnHitbox { .. } => Some(a.tick_offset),
                        _ => None,
                    }).unwrap_or(0);
                    let remove = t.actions.iter().find_map(|a| match &a.action {
                        AbilityAction::RemoveHitbox => Some(a.tick_offset),
                        _ => None,
                    }).unwrap_or(0);
                    remove.saturating_sub(spawn)
                })
                .unwrap_or(0);

            AbilityDef {
                id: data.ability_id,
                name: data.name.clone(),
                damage_type: data.damage_type,
                on_hit_buffs: data.on_hit_buffs.clone(),
                stun_ticks: data.stun_ticks,
                knockdown_ticks: data.knockdown_ticks,
                sleep_ticks: data.sleep_ticks,
                silence_ticks: data.silence_ticks,
                fear_ticks: data.fear_ticks,
                category,
                cooldown_ticks,
                color,
                shape,
                offset,
                targeting: targeting_mode_to_client(data.targeting_mode),
                max_range: data.max_range,
                projectile_speed: data.projectile_speed,
                lock_on_timeout_ticks: data.lock_on_timeout_ticks,
                linger_ticks,
                damage_interval_ticks: data.damage_interval_ticks,
                charge_tiers: data.charge_tiers.clone().unwrap_or_default(),
            }
        })
        .collect();

    // Block (ID 100) is client-only (hold-to-block) — add it if absent from the RON.
    if !defs.iter().any(|d| d.id == BLOCK_ABILITY_ID) {
        let (category, color) = crate::ability_visuals::visual_for(BLOCK_ABILITY_ID);
        defs.push(AbilityDef {
            id: BLOCK_ABILITY_ID,
            name: "Block".to_string(),
            damage_type: game_schema::DamageType::Physical,
            on_hit_buffs: Vec::new(),
            stun_ticks: 0,
            knockdown_ticks: 0,
            sleep_ticks: 0,
            silence_ticks: 0,
            fear_ticks: 0,
            category,
            cooldown_ticks: 0,
            color,
            shape: AbilityShape::None,
            offset: [0.0, 0.0, 0.0],
            targeting: ClientTargetingMode::SelfOnly,
            max_range: None,
            projectile_speed: None,
            lock_on_timeout_ticks: None,
            linger_ticks: 0,
            damage_interval_ticks: 0,
            charge_tiers: Vec::new(),
        });
    }

    defs
}

fn skill_shape_to_client(shape: game_core::combat::skill::SkillShape) -> AbilityShape {
    use game_core::combat::skill::SkillShape;
    match shape {
        SkillShape::CapsuleSweep => AbilityShape::Capsule { radius: 0.75, half_height: 1.0 },
        SkillShape::LineSweep    => AbilityShape::Capsule { radius: 0.5,  half_height: 3.0 },
        SkillShape::Cone         => AbilityShape::Capsule { radius: 0.75, half_height: 1.0 },
        SkillShape::Sphere       => AbilityShape::Sphere  { radius: 2.0 },
        SkillShape::HazardZone   => AbilityShape::Sphere  { radius: 5.0 },
        SkillShape::Projectile   => AbilityShape::None,
    }
}

fn targeting_mode_to_client(mode: game_core::combat::skill::TargetingMode) -> ClientTargetingMode {
    use game_core::combat::skill::TargetingMode;
    match mode {
        TargetingMode::DirectionTarget => ClientTargetingMode::DirectionTarget,
        TargetingMode::EntityTarget    => ClientTargetingMode::EntityTarget,
        TargetingMode::GroundTarget    => ClientTargetingMode::GroundTarget,
        TargetingMode::RaycastStrict   => ClientTargetingMode::RaycastStrict,
        TargetingMode::AimAssist       => ClientTargetingMode::AimAssist,
        TargetingMode::LockOn { .. }   => ClientTargetingMode::LockOn,
        TargetingMode::SelfOnly        => ClientTargetingMode::SelfOnly,
        TargetingMode::CasterOffset    => ClientTargetingMode::CasterOffset,
    }
}

fn format_charge_summary(def: &AbilityDef) -> String {
    if def.charge_tiers.is_empty() {
        return String::new();
    }

    let tiers = def.charge_tiers.len();
    let max_mult = def
        .charge_tiers
        .iter()
        .map(|tier| tier.damage_mult)
        .fold(1.0_f32, f32::max);
    let max_ticks = def.charge_tiers.iter().map(|tier| tier.min_ticks).max().unwrap_or(0);
    let max_secs = max_ticks as f32 / 20.0;

    format!("Charge: {tiers} tiers, max x{max_mult:.1} at {max_secs:.1}s")
}

pub fn damage_type_label(damage_type: game_schema::DamageType) -> &'static str {
    match damage_type {
        game_schema::DamageType::Physical => "Physical",
        game_schema::DamageType::Magical => "Magical",
        game_schema::DamageType::True => "True",
    }
}

fn format_ticks_secs(ticks: u32) -> String {
    format!("{:.1}s", ticks as f32 / 20.0)
}

fn buff_name_label(buff_id: u32) -> String {
    crate::hud::all_buffs()
        .iter()
        .find(|buff| buff.buff_id == buff_id)
        .map(|buff| {
            if buff.name.is_empty() {
                format!("Buff #{buff_id}")
            } else {
                buff.name.clone()
            }
        })
        .unwrap_or_else(|| format!("Buff #{buff_id}"))
}

fn format_effect_summary(def: &AbilityDef) -> String {
    let mut effects = Vec::new();

    if def.stun_ticks > 0 {
        effects.push(format!("Stun {}", format_ticks_secs(def.stun_ticks)));
    }
    if def.knockdown_ticks > 0 {
        effects.push(format!("KD {}", format_ticks_secs(def.knockdown_ticks)));
    }
    if def.sleep_ticks > 0 {
        effects.push(format!("Sleep {}", format_ticks_secs(def.sleep_ticks)));
    }
    if def.silence_ticks > 0 {
        effects.push(format!("Silence {}", format_ticks_secs(def.silence_ticks)));
    }
    if def.fear_ticks > 0 {
        effects.push(format!("Fear {}", format_ticks_secs(def.fear_ticks)));
    }

    for buff_id in def.on_hit_buffs.iter().take(2) {
        effects.push(format!("Applies {}", buff_name_label(*buff_id)));
    }
    if def.on_hit_buffs.len() > 2 {
        effects.push(format!("+{} more", def.on_hit_buffs.len() - 2));
    }

    effects.join(" | ")
}

fn format_targeting_summary(def: &AbilityDef) -> String {
    let mut parts = Vec::new();

    match def.targeting {
        ClientTargetingMode::GroundTarget => parts.push("Ground AoE".to_string()),
        ClientTargetingMode::EntityTarget => parts.push("Targeted".to_string()),
        ClientTargetingMode::RaycastStrict => parts.push("Raycast".to_string()),
        ClientTargetingMode::AimAssist => parts.push("Aim Assist".to_string()),
        ClientTargetingMode::SelfOnly => parts.push("Self".to_string()),
        ClientTargetingMode::CasterOffset => parts.push("Front AoE".to_string()),
        ClientTargetingMode::LockOn => parts.push("Lock-On".to_string()),
        ClientTargetingMode::DirectionTarget => {}
    }

    if let Some(range) = def.max_range {
        parts.push(format!("Range {range:.0}m"));
    }

    if let Some(projectile_speed) = def.projectile_speed {
        parts.push(format!("Proj {:.0}u/s", projectile_speed * 20.0));
    }

    if let Some(timeout_ticks) = def.lock_on_timeout_ticks {
        parts.push(format!("Lock {:.1}s", timeout_ticks as f32 / 20.0));
    }

    parts.join(" | ")
}

fn format_skill_menu_summary(def: &AbilityDef) -> String {
    let mut parts = vec![damage_type_label(def.damage_type).to_string()];
    let targeting_summary = format_targeting_summary(def);
    if !targeting_summary.is_empty() {
        parts.push(targeting_summary);
    }
    let charge_summary = format_charge_summary(def);
    if !charge_summary.is_empty() {
        parts.push(charge_summary);
    }
    let effect_summary = format_effect_summary(def);
    if !effect_summary.is_empty() {
        parts.push(effect_summary);
    }
    parts.join(" | ")
}


/// Tracks the tick at which each ability was last used.
#[derive(Resource)]
pub struct AbilityCooldowns {
    /// (ability_id → tick when cast). If current_tick - cast_tick < cooldown_ticks, on CD.
    last_used: [u64; 128], // index by ability_id
    pub current_tick: u64,
}

impl Default for AbilityCooldowns {
    fn default() -> Self {
        Self {
            last_used: [0; 128],
            current_tick: 0,
        }
    }
}

impl AbilityCooldowns {
    /// Record that an ability was activated on the given tick.
    pub fn activate(&mut self, ability_id: u32, tick: u64) {
        if (ability_id as usize) < self.last_used.len() {
            self.last_used[ability_id as usize] = tick;
        }
    }

    /// Returns remaining cooldown ticks (0 = ready).
    pub fn remaining(&self, ability_id: u32, cooldown_ticks: u32) -> u32 {
        let idx = ability_id as usize;
        if idx >= self.last_used.len() { return 0; }
        let used_at = self.last_used[idx];
        if used_at == 0 { return 0; }
        let elapsed = self.current_tick.saturating_sub(used_at) as u32;
        cooldown_ticks.saturating_sub(elapsed)
    }

    /// Returns 0.0 (ready) to 1.0 (just cast) fraction.
    pub fn fraction(&self, ability_id: u32, cooldown_ticks: u32) -> f32 {
        if cooldown_ticks == 0 { return 0.0; }
        let rem = self.remaining(ability_id, cooldown_ticks);
        rem as f32 / cooldown_ticks as f32
    }
}

/// Tag for the root ability bar container.
#[derive(Component)]
struct AbilityBar;

/// Tag for a single ability slot mapping to a fixed keybind (1, 2, 3)
#[derive(Component)]
struct AbilitySlot {
    slot_index: usize, // 0 = slot1, 1 = slot2, 2 = slot3
}

/// Tag for the cooldown overlay inside a slot.
#[derive(Component)]
struct CooldownOverlay {
    slot_index: usize,
}

/// Tag for the keybind label.
#[derive(Component)]
struct KeybindLabel;

/// Tag for the ability name label.
#[derive(Component)]
struct AbilityNameLabel;

fn spawn_ability_bar(mut commands: Commands) {
    let keys = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];
    // Root container — centered at bottom
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(30.0),
                left: Val::Percent(50.0),
                margin: UiRect { left: Val::Px(-((9.0 * 60.0 + 8.0 * 4.0) / 2.0)), ..default() },
                column_gap: Val::Px(4.0),
                display: Display::Flex,
                flex_direction: FlexDirection::Row,
                ..default()
            },
            AbilityBar,
        ))
        .with_children(|bar| {
            for slot_index in 0..9 {
                // Slot background
                bar.spawn((
                    Button,
                    Node {
                        width: Val::Px(60.0),
                        height: Val::Px(60.0),
                        border: UiRect::all(Val::Px(2.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        flex_direction: FlexDirection::Column,
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.1, 0.1, 0.1, 0.8)),
                    BorderColor(Color::WHITE),
                    AbilitySlot { slot_index },
                ))
                .with_children(|slot| {
                    // Cooldown overlay (stretches over the slot, height driven by fraction)
                    slot.spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            bottom: Val::Px(0.0),
                            left: Val::Px(0.0),
                            width: Val::Percent(100.0),
                            height: Val::Percent(0.0), // 0% = ready, 100% = full CD
                            ..default()
                        },
                        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.6)),
                        CooldownOverlay { slot_index },
                    ));

                    // Keybind label (top)
                    slot.spawn((
                        Text::new(keys[slot_index]),
                        TextFont {
                            font_size: 12.0,
                            ..default()
                        },
                        TextColor(Color::srgba(1.0, 1.0, 1.0, 0.9)),
                        KeybindLabel,
                    ));

                    // Ability name label (center)
                    slot.spawn((
                        Text::new(""),
                        TextFont {
                            font_size: 12.0,
                            ..default()
                        },
                        TextColor(Color::WHITE),
                        AbilityNameLabel,
                    ));
                });
            }

            // Weapon Swap Placeholder UI (right of skills)
            bar.spawn((
                Node {
                    margin: UiRect { left: Val::Px(4.0), ..default() },
                    width: Val::Px(70.0),
                    height: Val::Px(60.0),
                    border: UiRect::all(Val::Px(2.0)),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    flex_direction: FlexDirection::Column,
                    ..default()
                },
                BackgroundColor(Color::srgba(0.2, 0.2, 0.3, 0.8)),
                BorderColor(Color::srgb(0.5, 0.5, 0.7)),
            )).with_children(|swap_btn| {
                swap_btn.spawn((
                    Text::new("`"),
                    TextFont { font_size: 12.0, ..default() },
                    TextColor(Color::srgba(1.0, 1.0, 1.0, 0.9)),
                ));
                swap_btn.spawn((
                    Text::new("W.Swap"),
                    TextFont { font_size: 12.0, ..default() },
                    TextColor(Color::srgb(0.5, 0.5, 0.9)),
                ));
            });
        });
}

// ── Skill Menu Selection ─────────────────────────────────────────────

#[derive(Resource, Default)]
pub struct SkillMenuState {
    pub active_slot: Option<usize>,
    pub active_category: SkillMenuCategory,
    pub scroll_offset: usize,
}

impl SkillMenuState {
    const VISIBLE_ROWS: usize = 6;

    fn filtered_ability_ids(&self) -> Vec<u32> {
        all_abilities()
            .iter()
            .filter(|def| self.active_category == SkillMenuCategory::All || def.category == self.active_category)
            .map(|def| def.id)
            .collect()
    }

    fn clamp_scroll(&mut self) {
        let max_offset = self.filtered_ability_ids().len().saturating_sub(Self::VISIBLE_ROWS);
        self.scroll_offset = self.scroll_offset.min(max_offset);
    }
}

#[derive(Component)]
struct SkillMenuRoot;

#[derive(Component)]
struct SkillMenuButton {
    ability_id: u32,
}

#[derive(Component)]
struct SkillMenuFilterButton {
    category: SkillMenuCategory,
}

#[derive(Component)]
struct SkillMenuScrollButton {
    delta: i32,
}

#[derive(Component)]
struct SkillMenuEntry {
    ability_id: u32,
}

#[derive(Component)]
struct SkillMenuScrollLabel;

fn spawn_skill_menu(mut commands: Commands) {
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            right: Val::Px(20.0),
            top: Val::Percent(20.0),
            display: Display::None, // hidden by default
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(5.0),
            padding: UiRect::all(Val::Px(10.0)),
            width: Val::Px(240.0),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.8)),
        SkillMenuRoot,
    )).with_children(|menu| {
        menu.spawn((
            Text::new("Select Ability"),
            TextFont { font_size: 20.0, ..default() },
            TextColor(Color::WHITE),
        ));

        menu.spawn(Node {
            display: Display::Flex,
            flex_wrap: FlexWrap::Wrap,
            column_gap: Val::Px(4.0),
            row_gap: Val::Px(4.0),
            margin: UiRect { bottom: Val::Px(6.0), ..default() },
            ..default()
        }).with_children(|filters| {
            for category in SkillMenuCategory::FILTERS {
                filters.spawn((
                    Button,
                    Node {
                        width: Val::Px(70.0),
                        height: Val::Px(26.0),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.15, 0.15, 0.15, 0.95)),
                    SkillMenuFilterButton { category },
                )).with_children(|btn| {
                    btn.spawn((
                        Text::new(category.label()),
                        TextFont { font_size: 12.0, ..default() },
                        TextColor(Color::WHITE),
                    ));
                });
            }
        });

        for def in all_abilities() {
            menu.spawn((
                Button,
                Node {
                    width: Val::Px(220.0),
                    min_height: Val::Px(44.0),
                    padding: UiRect::axes(Val::Px(8.0), Val::Px(6.0)),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::FlexStart,
                    flex_direction: FlexDirection::Column,
                    display: Display::Flex,
                    ..default()
                },
                BackgroundColor(Color::srgba(0.2, 0.2, 0.2, 0.9)),
                SkillMenuButton { ability_id: def.id },
                SkillMenuEntry { ability_id: def.id },
            )).with_children(|btn| {
                btn.spawn((
                    Text::new(def.name.as_str()),
                    TextFont { font_size: 16.0, ..default() },
                    TextColor(def.color),
                ));

                let summary = format_skill_menu_summary(def);
                if !summary.is_empty() {
                    btn.spawn((
                        Text::new(summary),
                        TextFont { font_size: 11.0, ..default() },
                        TextColor(Color::srgba(0.85, 0.85, 0.85, 0.85)),
                    ));
                }
            });
        }

        menu.spawn(Node {
            display: Display::Flex,
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            margin: UiRect { top: Val::Px(6.0), ..default() },
            ..default()
        }).with_children(|footer| {
            for (label, delta) in [("Up", -1), ("Down", 1)] {
                footer.spawn((
                    Button,
                    Node {
                        width: Val::Px(72.0),
                        height: Val::Px(28.0),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.15, 0.15, 0.15, 0.95)),
                    SkillMenuScrollButton { delta },
                )).with_children(|btn| {
                    btn.spawn((
                        Text::new(label),
                        TextFont { font_size: 13.0, ..default() },
                        TextColor(Color::WHITE),
                    ));
                });
            }

            footer.spawn((
                Text::new(""),
                TextFont { font_size: 12.0, ..default() },
                TextColor(Color::srgb(0.8, 0.8, 0.8)),
                SkillMenuScrollLabel,
            ));
        });
    });
}


/// Keep cooldowns.current_tick in sync with TickCounter.
fn track_cooldowns(
    mut cooldowns: ResMut<AbilityCooldowns>,
    #[cfg(feature = "connected")]
    tick_counter: Option<Res<crate::spacetime::TickCounter>>,
) {
    #[cfg(feature = "connected")]
    if let Some(tc) = tick_counter {
        cooldowns.current_tick = tc.last_tick;
    }
}

/// Update cooldown overlay heights + slot border brightness based on remaining CD.
/// Also handles block-slot highlighting when the bound key is held.
fn update_ability_bar(
    keyboard: Res<ButtonInput<KeyCode>>,
    #[cfg(feature = "connected")] bindings: Option<Res<SkillBindings>>,
    cooldowns: Res<AbilityCooldowns>,
    menu_state: Res<SkillMenuState>,
    mut overlay_q: Query<(&CooldownOverlay, &mut Node)>,
    mut slot_q: Query<(&AbilitySlot, &mut BorderColor, &mut BackgroundColor, &Children)>,
    mut text_q: Query<(&mut Text, &mut TextColor), With<AbilityNameLabel>>,
) {
    #[cfg(feature = "connected")]
    let bound_ids = match bindings {
        Some(b) => b.slots,
        None => [1, 2, 3, 0, 0, 0, 0, 0, 0],
    };
    #[cfg(not(feature = "connected"))]
    let bound_ids = [1, 2, 3, 0, 0, 0, 0, 0, 0];

    let slot_keys = [
        KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3,
        KeyCode::Digit4, KeyCode::Digit5, KeyCode::Digit6,
        KeyCode::Digit7, KeyCode::Digit8, KeyCode::Digit9,
    ];

    for (ov, mut node) in overlay_q.iter_mut() {
        let ability_id = bound_ids[ov.slot_index];
        let def = all_abilities().iter().find(|a| a.id == ability_id);
        let Some(def) = def else {
            node.height = Val::Percent(0.0);
            continue;
        };
        let frac = cooldowns.fraction(ability_id, def.cooldown_ticks);
        node.height = Val::Percent(frac * 100.0);
    }

    for (slot, mut border, mut bg, children) in slot_q.iter_mut() {
        let ability_id = bound_ids[slot.slot_index];
        let def = all_abilities().iter().find(|a| a.id == ability_id);

        // Block slot: highlight based on key held state.
        if ability_id == BLOCK_ABILITY_ID {
            let key_held = keyboard.pressed(slot_keys[slot.slot_index]);
            if key_held {
                *border = BorderColor(Color::srgb(0.2, 0.9, 1.0));
                *bg = BackgroundColor(Color::srgba(0.0, 0.4, 0.6, 0.95));
            } else {
                *border = BorderColor(Color::srgb(0.3, 0.6, 1.0));
                *bg = BackgroundColor(Color::srgba(0.1, 0.2, 0.3, 0.8));
            }
            for child in children.iter() {
                if let Ok((mut text, mut color)) = text_q.get_mut(*child) {
                    **text = "Block".to_string();
                    color.0 = Color::srgb(0.3, 0.6, 1.0);
                }
            }
            continue;
        }

        let Some(def) = def else {
            *bg = BackgroundColor(Color::srgba(0.1, 0.1, 0.1, 0.8));
            for child in children.iter() {
                if let Ok((mut text, mut color)) = text_q.get_mut(*child) {
                    **text = String::new();
                    color.0 = Color::WHITE;
                }
            }
            continue;
        };

        // Normal ability slot.
        *bg = BackgroundColor(Color::srgba(0.1, 0.1, 0.1, 0.8));
        if menu_state.active_slot == Some(slot.slot_index) {
            *border = BorderColor(Color::WHITE);
        } else {
            let remaining = cooldowns.remaining(ability_id, def.cooldown_ticks);
            if remaining == 0 {
                *border = BorderColor(def.color);
            } else {
                let Color::Srgba(c) = def.color else { continue };
                *border = BorderColor(Color::srgba(c.red * 0.3, c.green * 0.3, c.blue * 0.3, 0.5));
            }
        }

        // Update name text
        for child in children.iter() {
            if let Ok((mut text, mut color)) = text_q.get_mut(*child) {
                **text = def.name.to_string();
                color.0 = def.color;
            }
        }
    }
}

fn handle_slot_clicks(
    mut interaction_q: Query<(&Interaction, &AbilitySlot), Changed<Interaction>>,
    mut menu_state: ResMut<SkillMenuState>,
) {
    for (interaction, slot) in interaction_q.iter_mut() {
        if *interaction == Interaction::Pressed {
            // Toggle active slot
            if menu_state.active_slot == Some(slot.slot_index) {
                menu_state.active_slot = None;
            } else {
                menu_state.active_slot = Some(slot.slot_index);
            }
        }
    }
}

fn handle_menu_clicks(
    #[cfg(feature = "connected")] mut bindings: Option<ResMut<SkillBindings>>,
    mut interaction_q: Query<(&Interaction, &SkillMenuButton, &mut BackgroundColor), Changed<Interaction>>,
    mut menu_state: ResMut<SkillMenuState>,
) {
    for (interaction, btn, mut bg) in interaction_q.iter_mut() {
        match *interaction {
            Interaction::Pressed => {
                #[cfg(feature = "connected")]
                if let Some(ref mut b) = bindings {
                    if let Some(slot) = menu_state.active_slot {
                        if slot < b.slots.len() {
                            b.slots[slot] = btn.ability_id;
                        }
                    }
                }
                menu_state.active_slot = None; // Hide menu after selection
            }
            Interaction::Hovered => *bg = BackgroundColor(Color::srgba(0.3, 0.3, 0.3, 0.9)),
            Interaction::None => *bg = BackgroundColor(Color::srgba(0.2, 0.2, 0.2, 0.9)),
        }
    }
}

fn handle_filter_clicks(
    mut interaction_q: Query<(&Interaction, &SkillMenuFilterButton, &mut BackgroundColor), Changed<Interaction>>,
    mut menu_state: ResMut<SkillMenuState>,
) {
    for (interaction, btn, mut bg) in interaction_q.iter_mut() {
        match *interaction {
            Interaction::Pressed => {
                menu_state.active_category = btn.category;
                menu_state.scroll_offset = 0;
            }
            Interaction::Hovered => *bg = BackgroundColor(Color::srgba(0.25, 0.25, 0.25, 0.95)),
            Interaction::None => {
                *bg = if menu_state.active_category == btn.category {
                    BackgroundColor(Color::srgba(0.25, 0.45, 0.7, 0.95))
                } else {
                    BackgroundColor(Color::srgba(0.15, 0.15, 0.15, 0.95))
                };
            }
        }
    }
}

fn handle_scroll_controls(
    mut wheel_events: EventReader<MouseWheel>,
    mut interaction_q: Query<(&Interaction, &SkillMenuScrollButton, &mut BackgroundColor), Changed<Interaction>>,
    mut menu_state: ResMut<SkillMenuState>,
) {
    if menu_state.active_slot.is_some() {
        let mut wheel_delta = 0;
        for ev in wheel_events.read() {
            wheel_delta += ev.y.round() as i32;
        }
        if wheel_delta != 0 {
            let next = menu_state.scroll_offset as i32 - wheel_delta;
            menu_state.scroll_offset = next.max(0) as usize;
        }
    } else {
        wheel_events.clear();
    }

    for (interaction, btn, mut bg) in interaction_q.iter_mut() {
        match *interaction {
            Interaction::Pressed => {
                let next = menu_state.scroll_offset as i32 + btn.delta;
                menu_state.scroll_offset = next.max(0) as usize;
            }
            Interaction::Hovered => *bg = BackgroundColor(Color::srgba(0.25, 0.25, 0.25, 0.95)),
            Interaction::None => *bg = BackgroundColor(Color::srgba(0.15, 0.15, 0.15, 0.95)),
        }
    }

    menu_state.clamp_scroll();
}

fn update_skill_menu_visibility(
    menu_state: Res<SkillMenuState>,
    mut root_q: Query<&mut Node, (With<SkillMenuRoot>, Without<SkillMenuEntry>)>,
    mut filter_q: Query<
        (&SkillMenuFilterButton, &mut BackgroundColor),
        (Without<SkillMenuEntry>, Without<SkillMenuRoot>),
    >,
    mut entry_q: Query<
        (&SkillMenuEntry, &mut Node, &mut BackgroundColor),
        (Without<SkillMenuFilterButton>, Without<SkillMenuRoot>),
    >,
    mut scroll_label_q: Query<&mut Text, With<SkillMenuScrollLabel>>,
) {
    let Ok(mut node) = root_q.get_single_mut() else { return };
    if menu_state.active_slot.is_some() {
        node.display = Display::Flex;
    } else {
        node.display = Display::None;
    }

    let filtered_ids = menu_state.filtered_ability_ids();
    let start = menu_state.scroll_offset.min(filtered_ids.len().saturating_sub(SkillMenuState::VISIBLE_ROWS));
    let end = (start + SkillMenuState::VISIBLE_ROWS).min(filtered_ids.len());
    let visible_ids = &filtered_ids[start..end];

    for (filter, mut bg) in filter_q.iter_mut() {
        bg.0 = if menu_state.active_category == filter.category {
            Color::srgba(0.25, 0.45, 0.7, 0.95)
        } else {
            Color::srgba(0.15, 0.15, 0.15, 0.95)
        };
    }

    for (entry, mut entry_node, mut bg) in entry_q.iter_mut() {
        let visible = visible_ids.contains(&entry.ability_id);
        entry_node.display = if visible { Display::Flex } else { Display::None };

        if let Some(def) = all_abilities().iter().find(|def| def.id == entry.ability_id) {
            bg.0 = if visible && menu_state.active_category != SkillMenuCategory::All {
                match def.color {
                    Color::Srgba(tint) => Color::srgba(tint.red * 0.25, tint.green * 0.25, tint.blue * 0.25, 0.95),
                    _ => Color::srgba(0.2, 0.2, 0.2, 0.9),
                }
            } else {
                Color::srgba(0.2, 0.2, 0.2, 0.9)
            };
        }
    }

    let Ok(mut scroll_text) = scroll_label_q.get_single_mut() else { return };
    **scroll_text = if filtered_ids.is_empty() {
        "0 / 0".to_string()
    } else {
        format!("{}-{} / {}", start + 1, end, filtered_ids.len())
    };
}


