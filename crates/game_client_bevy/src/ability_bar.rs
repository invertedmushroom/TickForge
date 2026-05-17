use bevy::prelude::*;

#[cfg(feature = "connected")]
use crate::input::SkillBindings;

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
            update_skill_menu_visibility,
        ));
    }
}

/// Reserved ability ID for block — assignable to any skill slot.
pub const BLOCK_ABILITY_ID: u32 = 100;

/// Hitbox shape for client-side visualization.
/// Variants match the simulation's `skill_shape_to_sensor()` output shapes.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub enum AbilityShape {
    /// Capsule sensor (CapsuleSweep, LineSweep, Cone in the simulation).
    Capsule { radius: f32, half_height: f32 },
    /// Sphere sensor (PBAoE, HazardZone).
    Sphere { radius: f32 },
    /// No melee hitbox visual — projectiles use their own VFX path.
    None,
}

/// Client-side targeting mode — mirrors the server's `TargetingMode` enum
/// (game_core::combat::skill) without needing a crate dependency.
/// Controls what `AbilityTarget` variant the client sends in the UseAbility intent.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ClientTargetingMode {
    /// Directional cast — client sends a normalized aim direction.
    DirectionTarget,
    /// Explicit tab-targeted single-entity cast.
    EntityTarget,
    /// Client sends a world position for AoE placement.
    GroundTarget,
    /// Client must send Direction — server raycasts to find a target.
    RaycastStrict,
    /// Soft-aim: send Direction plus an optional non-authoritative target hint.
    AimAssist,
    /// Always self-cast — no targeting data needed.
    SelfOnly,
    /// Stationary world placement resolved relative to the caster.
    CasterOffset,
    /// TERA-style multi-step lock-on (open → tag → fire).
    LockOn,
}

#[derive(Clone, Copy)]
pub struct AbilityDef {
    pub id: u32,
    pub name: &'static str,
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
}

pub const ALL_ABILITIES: &[AbilityDef] = &[
    // Sensor shapes match simulation's skill_shape_to_sensor() in tick_pipeline/mod.rs:
    //   CapsuleSweep → Capsule { half_height: 1.0, radius: 0.75 }
    //   LineSweep    → Capsule { half_height: 3.0, radius: 0.5  }
    //   Projectile   → separate VFX path (AbilityShape::None)
    // Offsets from abilities.ron timeline SpawnHitbox actions.
    AbilityDef { id: 1,  name: "Slash",       cooldown_ticks: 20,  color: Color::srgb(1.0, 0.8, 0.2), shape: AbilityShape::Capsule { radius: 0.75, half_height: 1.0 }, offset: [0.0, 0.0, 0.0], targeting: ClientTargetingMode::DirectionTarget, max_range: None },
    AbilityDef { id: 2,  name: "Fireball",    cooldown_ticks: 40,  color: Color::srgb(1.0, 0.4, 0.1), shape: AbilityShape::None, offset: [0.0, 1.0, 0.0], targeting: ClientTargetingMode::AimAssist, max_range: None },
    AbilityDef { id: 3,  name: "Smash",       cooldown_ticks: 50,  color: Color::srgb(0.6, 0.3, 1.0), shape: AbilityShape::Capsule { radius: 0.5, half_height: 3.0 }, offset: [0.0, 0.0, 0.0], targeting: ClientTargetingMode::DirectionTarget, max_range: None },
    AbilityDef { id: 4,  name: "Slash Combo", cooldown_ticks: 20,  color: Color::srgb(1.0, 0.9, 0.4), shape: AbilityShape::Capsule { radius: 0.75, half_height: 1.0 }, offset: [0.0, 0.0, 0.0], targeting: ClientTargetingMode::DirectionTarget, max_range: None },
    AbilityDef { id: 10, name: "Shield Bash", cooldown_ticks: 60,  color: Color::srgb(0.5, 0.5, 0.5), shape: AbilityShape::Capsule { radius: 0.75, half_height: 1.0 }, offset: [0.0, 0.0, 0.0], targeting: ClientTargetingMode::DirectionTarget, max_range: None },
    AbilityDef { id: 11, name: "Uppercut",    cooldown_ticks: 80,  color: Color::srgb(0.8, 0.1, 0.1), shape: AbilityShape::Capsule { radius: 0.75, half_height: 1.0 }, offset: [0.0, 0.0, 0.0], targeting: ClientTargetingMode::DirectionTarget, max_range: None },
    AbilityDef { id: 12, name: "Grapple",     cooldown_ticks: 100, color: Color::srgb(0.1, 0.8, 0.1), shape: AbilityShape::None, offset: [0.0, 1.0, 0.0], targeting: ClientTargetingMode::EntityTarget, max_range: Some(20.0) },
    AbilityDef { id: 13, name: "Leg Sweep",   cooldown_ticks: 40,  color: Color::srgb(0.6, 0.4, 0.2), shape: AbilityShape::Capsule { radius: 0.75, half_height: 1.0 }, offset: [0.0, 0.0, 0.0], targeting: ClientTargetingMode::DirectionTarget, max_range: None },
    // Lock-on / teleport abilities
    AbilityDef { id: 20, name: "Chain Ltng",  cooldown_ticks: 60,  color: Color::srgb(0.3, 0.7, 1.0), shape: AbilityShape::None, offset: [0.0, 0.0, 0.0], targeting: ClientTargetingMode::LockOn, max_range: Some(15.0) },
    AbilityDef { id: 21, name: "Backstab",    cooldown_ticks: 80,  color: Color::srgb(0.8, 0.2, 0.5), shape: AbilityShape::Capsule { radius: 0.75, half_height: 1.0 }, offset: [0.0, 0.0, 0.0], targeting: ClientTargetingMode::RaycastStrict, max_range: None },
    AbilityDef { id: 22, name: "Blink",       cooldown_ticks: 40,  color: Color::srgb(0.2, 0.9, 0.9), shape: AbilityShape::None, offset: [0.0, 0.0, 0.0], targeting: ClientTargetingMode::DirectionTarget, max_range: None },
    // Caster-attached / hazard zone abilities
    AbilityDef { id: 23, name: "Flame Aura",  cooldown_ticks: 200, color: Color::srgb(1.0, 0.5, 0.0), shape: AbilityShape::Sphere { radius: 2.0 }, offset: [0.0, 0.0, 0.0], targeting: ClientTargetingMode::SelfOnly, max_range: None },
    AbilityDef { id: 24, name: "Fire Patch",  cooldown_ticks: 160, color: Color::srgb(0.9, 0.3, 0.0), shape: AbilityShape::Sphere { radius: 5.0 }, offset: [0.0, 0.0, 0.0], targeting: ClientTargetingMode::GroundTarget, max_range: Some(30.0) },
    AbilityDef { id: 25, name: "Lava Pool",   cooldown_ticks: 100, color: Color::srgb(1.0, 0.2, 0.0), shape: AbilityShape::Sphere { radius: 5.0 }, offset: [0.0, 0.0, 2.0], targeting: ClientTargetingMode::CasterOffset, max_range: None },
    AbilityDef { id: BLOCK_ABILITY_ID, name: "Block", cooldown_ticks: 0, color: Color::srgb(0.3, 0.6, 1.0), shape: AbilityShape::None, offset: [0.0, 0.0, 0.0], targeting: ClientTargetingMode::SelfOnly, max_range: None },
];

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
}

#[derive(Component)]
struct SkillMenuRoot;

#[derive(Component)]
struct SkillMenuButton {
    ability_id: u32,
}

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

        for def in ALL_ABILITIES {
            menu.spawn((
                Button,
                Node {
                    width: Val::Px(150.0),
                    height: Val::Px(40.0),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    ..default()
                },
                BackgroundColor(Color::srgba(0.2, 0.2, 0.2, 0.9)),
                SkillMenuButton { ability_id: def.id },
            )).with_children(|btn| {
                btn.spawn((
                    Text::new(def.name),
                    TextFont { font_size: 16.0, ..default() },
                    TextColor(def.color),
                ));
            });
        }
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
        let def = ALL_ABILITIES.iter().find(|a| a.id == ability_id);
        let Some(def) = def else {
            node.height = Val::Percent(0.0);
            continue;
        };
        let frac = cooldowns.fraction(ability_id, def.cooldown_ticks);
        node.height = Val::Percent(frac * 100.0);
    }

    for (slot, mut border, mut bg, children) in slot_q.iter_mut() {
        let ability_id = bound_ids[slot.slot_index];
        let def = ALL_ABILITIES.iter().find(|a| a.id == ability_id);

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

fn update_skill_menu_visibility(
    menu_state: Res<SkillMenuState>,
    mut root_q: Query<&mut Node, With<SkillMenuRoot>>,
) {
    let Ok(mut node) = root_q.get_single_mut() else { return };
    if menu_state.active_slot.is_some() {
        node.display = Display::Flex;
    } else {
        node.display = Display::None;
    }
}


