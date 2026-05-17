use game_schema::EntityKind;

/// Authoritative per-entity-kind base movement speed (units/second).
///
/// These are the canonical speeds before any buff modifier is applied.
/// `apply_movement` in the tick pipeline multiplies this by `speed_pct`
/// from active `BuffModifiers` to produce the effective speed.
///
/// | Kind       | Speed | Rationale                                             |
/// |------------|-------|-------------------------------------------------------|
/// | Player     | 5.0   | Baseline feel; matches previous hardcoded value        |
/// | Npc        | 3.5   | Slower than player to allow kiting                    |
/// | Boss       | 2.5   | Bosses are slow but hit hard                          |
/// | Projectile | 12.0  | Projectiles move fast; kinematics override per ability |
/// | Hazard     | 0.0   | Hazards are stationary by default                     |
pub fn base_speed(kind: EntityKind) -> f32 {
    match kind {
        EntityKind::Player    => 5.0,
        EntityKind::Npc       => 3.5,
        EntityKind::Boss      => 2.5,
        EntityKind::Projectile => 12.0,
        EntityKind::Hazard    => 0.0,
    }
}
