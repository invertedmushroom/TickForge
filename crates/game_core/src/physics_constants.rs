// Shared physics constants used across the simulation worker and game core.
// Units: meters (m), meters per second (m/s), meters per second squared (m/s²).
pub const GRAVITY: f32 = 20.0;               // m/s², universal gravity used for arcs
pub const JUMP_SPEED: f32 = 10.0;            // m/s initial Y velocity for player jump
pub const JUMP_GRAVITY: f32 = GRAVITY;       // alias for clarity in jump code
pub const FALL_GRAVITY: f32 = GRAVITY;       // alias for clarity when falling
pub const GROUND_PULL: f32 = 4.0;            // m/s, small downward pull for KCC
pub const INTERACT_RADIUS: f32 = 3.0;        // meters for proximity interactions
pub const EVADE_ARRIVE_RADIUS: f32 = 0.5;    // meters — NPC evade arrival threshold
pub const FALL_DAMAGE_THRESHOLD: f32 = 15.0; // m/s impact speed threshold for fall damage
pub const FALL_DAMAGE_FACTOR: f32 = 5.0;     // HP per m/s over threshold
pub const WEAPON_SWAP_COOLDOWN_TICKS: u32 = 20; // ticks (~1s at 20 Hz) between weapon swaps
pub const DEFAULT_ABILITY_MAX_RANGE: f32 = 30.0; // default max range for abilities (meters)
pub const DEFAULT_PROJECTILE_SPEED: f32 = 1.0;  // units per tick (20 units/sec at 20 Hz)
/// AimAssist soft-lock cone: cos(half_angle) threshold when target_hint present (10°).
pub const AIM_ASSIST_DOT_WITH_HINT: f32 = 0.985;
/// AimAssist soft-lock cone: cos(half_angle) threshold without target_hint (15°).
pub const AIM_ASSIST_DOT_NO_HINT: f32 = 0.966;
pub const CAPSULE_HALF_HEIGHT: f32 = 0.5;    // half-height of character capsule
pub const CAPSULE_RADIUS: f32 = 0.3;         // radius of character capsule
/// Ground surface Y (cuboid half-extent) + capsule bottom-to-center distance.
/// Any character center below this is embedded in the floor.
pub const MIN_CHARACTER_Y: f32 = 0.1 + CAPSULE_HALF_HEIGHT + CAPSULE_RADIUS; // 0.9
