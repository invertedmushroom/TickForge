use bevy::prelude::Color;

/// Client-side ability category for the skill menu filter tabs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkillMenuCategory {
    All,
    Melee,
    Control,
    Mobility,
    Ranged,
    Area,
    Utility,
}

impl SkillMenuCategory {
    pub const FILTERS: [Self; 7] = [
        Self::All,
        Self::Melee,
        Self::Control,
        Self::Mobility,
        Self::Ranged,
        Self::Area,
        Self::Utility,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Melee => "Melee",
            Self::Control => "Control",
            Self::Mobility => "Mobility",
            Self::Ranged => "Ranged",
            Self::Area => "Area",
            Self::Utility => "Utility",
        }
    }
}

impl Default for SkillMenuCategory {
    fn default() -> Self {
        Self::All
    }
}

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

/// Client-side targeting mode — mirrors the server's `TargetingMode` enum.
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

/// Per-ability visual overrides: `(ability_id, category, color)`.
///
/// Edit this table to reclassify an ability or change its display color.
/// Abilities not listed fall back to `(SkillMenuCategory::Utility, Color::WHITE)`.
pub static ABILITY_VISUALS: &[(u32, SkillMenuCategory, Color)] = &[
    // id   category                   color
    (1, SkillMenuCategory::Melee, Color::srgb(1.0, 0.8, 0.2)), // Slash
    (2, SkillMenuCategory::Ranged, Color::srgb(1.0, 0.4, 0.1)), // Fireball
    (3, SkillMenuCategory::Melee, Color::srgb(0.6, 0.3, 1.0)), // Smash
    (4, SkillMenuCategory::Melee, Color::srgb(1.0, 0.9, 0.4)), // Slash Combo
    (10, SkillMenuCategory::Control, Color::srgb(0.5, 0.5, 0.5)), // Shield Bash
    (11, SkillMenuCategory::Control, Color::srgb(0.8, 0.1, 0.1)), // Uppercut
    (12, SkillMenuCategory::Control, Color::srgb(0.1, 0.8, 0.1)), // Grapple
    (13, SkillMenuCategory::Control, Color::srgb(0.6, 0.4, 0.2)), // Leg Sweep
    (20, SkillMenuCategory::Ranged, Color::srgb(0.3, 0.7, 1.0)), // Chain Lightning
    (21, SkillMenuCategory::Mobility, Color::srgb(0.8, 0.2, 0.5)), // Backstab
    (22, SkillMenuCategory::Mobility, Color::srgb(0.2, 0.9, 0.9)), // Blink
    (23, SkillMenuCategory::Area, Color::srgb(1.0, 0.5, 0.0)), // Flame Aura
    (24, SkillMenuCategory::Area, Color::srgb(0.9, 0.3, 0.0)), // Fire Patch
    (25, SkillMenuCategory::Area, Color::srgb(1.0, 0.2, 0.0)), // Lava Pool
    (42, SkillMenuCategory::Melee, Color::srgb(0.8, 0.5, 0.2)), // Charged Smash
    (50, SkillMenuCategory::Ranged, Color::srgb(0.6, 0.8, 1.0)), // Shock Bolt
    (51, SkillMenuCategory::Mobility, Color::srgb(0.7, 0.4, 1.0)), // Phase Sweep
    (52, SkillMenuCategory::Control, Color::srgb(1.0, 0.3, 0.3)), // Test Uppercut
    (60, SkillMenuCategory::Utility, Color::srgb(0.5, 1.0, 0.8)), // Cleanse Pulse
    (61, SkillMenuCategory::Utility, Color::srgb(0.3, 0.9, 0.7)), // Clear CC Pool
    (62, SkillMenuCategory::Utility, Color::srgb(0.2, 0.8, 1.0)), // Stunbreak
    (70, SkillMenuCategory::Control, Color::srgb(0.7, 0.7, 1.0)), // Sleep Dart
    (71, SkillMenuCategory::Control, Color::srgb(0.6, 0.6, 0.9)), // Hush
    (72, SkillMenuCategory::Control, Color::srgb(0.9, 0.5, 0.7)), // Terrify
    (80, SkillMenuCategory::Control, Color::srgb(0.8, 0.6, 0.3)), // Heavy Slam
    (81, SkillMenuCategory::Control, Color::srgb(1.0, 0.7, 0.2)), // Shockwave Slam
    (90, SkillMenuCategory::Mobility, Color::srgb(0.5, 0.9, 0.5)), // Vault
    (99, SkillMenuCategory::Utility, Color::srgb(1.0, 0.85, 0.3)), // Chargey Taunt
    (100, SkillMenuCategory::Utility, Color::srgb(0.3, 0.6, 1.0)), // Block (client-only)
];

/// Returns the `(category, color)` visual override for the given ability ID.
/// Falls back to `(SkillMenuCategory::Utility, Color::WHITE)` for unknown IDs.
pub fn visual_for(id: u32) -> (SkillMenuCategory, Color) {
    ABILITY_VISUALS
        .iter()
        .find(|(aid, _, _)| *aid == id)
        .map(|(_, cat, col)| (*cat, *col))
        .unwrap_or((SkillMenuCategory::Utility, Color::WHITE))
}
