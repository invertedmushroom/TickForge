use bevy::prelude::*;

pub struct AbilityBarPlugin;

impl Plugin for AbilityBarPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AbilityCooldowns>();
        app.add_systems(Startup, spawn_ability_bar);
        app.add_systems(Update, (track_cooldowns, update_ability_bar));
    }
}

/// Ability definition for the UI.
struct AbilityDef {
    id: u32,
    name: &'static str,
    keybind: &'static str,
    cooldown_ticks: u32,
    color: Color,
}

const ABILITIES: &[AbilityDef] = &[
    AbilityDef { id: 1, name: "Slash",    keybind: "1", cooldown_ticks: 20, color: Color::srgb(1.0, 0.8, 0.2) },
    AbilityDef { id: 2, name: "Fireball", keybind: "2", cooldown_ticks: 40, color: Color::srgb(1.0, 0.4, 0.1) },
    AbilityDef { id: 3, name: "Smash",    keybind: "3", cooldown_ticks: 50, color: Color::srgb(0.6, 0.3, 1.0) },
    AbilityDef { id: 4, name: "Block",    keybind: "Q", cooldown_ticks:  0, color: Color::srgb(0.3, 0.6, 1.0) },
];

/// Tracks the tick at which each ability was last used.
#[derive(Resource, Default)]
pub struct AbilityCooldowns {
    /// (ability_id → tick when cast). If current_tick - cast_tick < cooldown_ticks, on CD.
    last_used: [u64; 5], // index by ability_id (1-4); slot 0 unused
    pub current_tick: u64,
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

/// Tag for a single ability slot (carries ability_id).
#[derive(Component)]
struct AbilitySlot {
    ability_id: u32,
}

/// Tag for the cooldown overlay inside a slot.
#[derive(Component)]
struct CooldownOverlay {
    ability_id: u32,
}

/// Tag for the keybind label.
#[derive(Component)]
struct KeybindLabel;

/// Tag for the ability name label.
#[derive(Component)]
struct AbilityNameLabel;

fn spawn_ability_bar(mut commands: Commands) {
    // Root container — centered at bottom
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(30.0),
                left: Val::Percent(50.0),
                margin: UiRect { left: Val::Px(-((4.0 * 80.0 + 3.0 * 8.0) / 2.0)), ..default() },
                column_gap: Val::Px(8.0),
                display: Display::Flex,
                flex_direction: FlexDirection::Row,
                ..default()
            },
            AbilityBar,
        ))
        .with_children(|bar| {
            for def in ABILITIES {
                // Slot background
                bar.spawn((
                    Node {
                        width: Val::Px(80.0),
                        height: Val::Px(80.0),
                        border: UiRect::all(Val::Px(2.0)),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        flex_direction: FlexDirection::Column,
                        ..default()
                    },
                    BackgroundColor(Color::srgba(0.1, 0.1, 0.1, 0.8)),
                    BorderColor(def.color),
                    AbilitySlot { ability_id: def.id },
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
                        CooldownOverlay { ability_id: def.id },
                    ));

                    // Keybind label (top)
                    slot.spawn((
                        Text::new(def.keybind),
                        TextFont {
                            font_size: 12.0,
                            ..default()
                        },
                        TextColor(Color::srgba(1.0, 1.0, 1.0, 0.6)),
                        KeybindLabel,
                    ));

                    // Ability name label (center)
                    slot.spawn((
                        Text::new(def.name),
                        TextFont {
                            font_size: 16.0,
                            ..default()
                        },
                        TextColor(def.color),
                        AbilityNameLabel,
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
fn update_ability_bar(
    cooldowns: Res<AbilityCooldowns>,
    mut overlay_q: Query<(&CooldownOverlay, &mut Node)>,
    mut slot_q: Query<(&AbilitySlot, &mut BorderColor)>,
) {
    for (ov, mut node) in overlay_q.iter_mut() {
        let def = ABILITIES.iter().find(|a| a.id == ov.ability_id);
        let Some(def) = def else { continue };
        let frac = cooldowns.fraction(ov.ability_id, def.cooldown_ticks);
        node.height = Val::Percent(frac * 100.0);
    }

    for (slot, mut border) in slot_q.iter_mut() {
        let def = ABILITIES.iter().find(|a| a.id == slot.ability_id);
        let Some(def) = def else { continue };
        let remaining = cooldowns.remaining(slot.ability_id, def.cooldown_ticks);
        if remaining == 0 {
            *border = BorderColor(def.color);
        } else {
            // Dim the border when on cooldown.
            let Color::Srgba(c) = def.color else { continue };
            *border = BorderColor(Color::srgba(c.red * 0.3, c.green * 0.3, c.blue * 0.3, 0.5));
        }
    }
}
