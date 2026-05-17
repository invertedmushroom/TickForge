//! Lag compensation — server-side rewind hit detection.
//!
//! At 20 Hz (50 ms ticks) a client with 100 ms RTT sees target positions ~2 ticks
//! behind the server.  The attacker's hitbox evaluates against the target's *current*
//! position, causing clean-looking hits to miss on the server.
//!
//! This module implements position-history rewind: Phase 6 performs standalone Parry
//! shape intersection tests between the hitbox geometry and the target hurtbox at
//! historical positions, without touching Rapier bodies or the BVH.
//!
//! # Data flow
//!
//! 1. **Phase 10** snapshots entity positions into `TransformHistory` (ring buffer).
//! 2. **Phase 2** computes `rewind_ticks` from the intent's tick delta and stores it
//!    on `AbilityExecutionContext`.
//! 3. **Phase 3** copies `rewind_ticks` from the execution context to `ActiveHitbox`.
//! 4. **Phase 6** calls [`resolve_compensated_hits`] after the normal contact-based
//!    resolution.  For each armed hitbox with `rewind_ticks > 0`, it runs standalone
//!    intersection tests against each entity's historical position and processes any
//!    new overlaps through the same damage pipeline.
//!
//! # Why not rewind Rapier bodies?
//!
//! Moving kinematic bodies in Phase 6 does **not** teleport them — it queues the
//! movement for the next `step()`.  Even if you force the position, the BroadPhase
//! BVH is stale until rebuilt, so contact queries return the old state.  Rebuilding
//! the BVH per compensated attack is O(N) and prohibitively expensive in the hot loop.
//!
//! Parry's standalone `intersection_test` takes two positioned shapes and returns a
//! bool in O(1).  For ≤200 entities and ≤10 compensated hitboxes, the cost is
//! negligible (~50 µs per tick).

use std::collections::VecDeque;

use game_protocol::entity_id::EntityId;
use game_protocol::tick::TickId;
use game_schema::Vec3f;
use game_core::combat::skill::SkillShape;
use game_core::physics_backend::SensorShape;

// ── Constants ───────────────────────────────────────────────────

/// Maximum number of ticks a hitbox can rewind.  200 ms at 20 Hz.
///
/// Clamped in `compute_rewind_ticks` — even if the client reports a larger delta,
/// the server never rewinds further than this.
pub const MAX_REWIND_TICKS: u32 = 4;

/// Number of historical snapshots retained.  Must be ≥ MAX_REWIND_TICKS.
/// Extra headroom handles edge cases where a hitbox was spawned one tick
/// after the intent was consumed.
pub const MAX_HISTORY_TICKS: usize = 8;

/// Standard character hurtbox: capsule with half_height=0.5, radius=0.3.
/// Must match the shape used in `PhysicsWorld::spawn_character_body`.
pub const HURTBOX_HALF_HEIGHT: f32 = 0.5;
pub const HURTBOX_RADIUS: f32 = 0.3;

// ── Transform history ───────────────────────────────────────────

/// Per-tick position snapshot stored in a ring buffer on `TickPipeline`.
///
/// Only positions — rotation and velocity do not affect hitbox overlap tests.
#[derive(Clone, Debug)]
pub struct TransformSnapshot {
    pub tick: TickId,
    pub positions: Vec<(EntityId, Vec3f)>,
}

/// Ring buffer of recent transform snapshots for lag compensation rewind.
#[derive(Clone, Debug)]
pub struct TransformHistory {
    buffer: VecDeque<TransformSnapshot>,
}

impl TransformHistory {
    pub fn new() -> Self {
        Self {
            buffer: VecDeque::with_capacity(MAX_HISTORY_TICKS + 1),
        }
    }

    /// Record a snapshot of all entity positions at the given tick.
    /// Called at the end of Phase 10 (commit) before advancing the tick counter.
    pub fn record(&mut self, tick: TickId, positions: Vec<(EntityId, Vec3f)>) {
        if self.buffer.len() >= MAX_HISTORY_TICKS {
            self.buffer.pop_front();
        }
        self.buffer.push_back(TransformSnapshot { tick, positions });
    }

    /// Look up the nearest snapshot at or before the requested tick.
    /// Returns `None` if no snapshot is old enough (history too short).
    pub fn get_snapshot(&self, tick: TickId) -> Option<&TransformSnapshot> {
        // Walk from newest to oldest; return the first snapshot whose tick ≤ requested.
        self.buffer.iter().rev().find(|s| s.tick <= tick)
    }

    /// Look up a specific entity's position in the snapshot closest to `tick`.
    pub fn get_position(&self, tick: TickId, entity_id: EntityId) -> Option<Vec3f> {
        let snapshot = self.get_snapshot(tick)?;
        snapshot
            .positions
            .iter()
            .find(|(eid, _)| *eid == entity_id)
            .map(|(_, pos)| *pos)
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }
}

impl Default for TransformHistory {
    fn default() -> Self {
        Self::new()
    }
}

// ── Rewind calculation ──────────────────────────────────────────

/// Compute the rewind tick count from an intent's observed-tick field.
///
/// `client_observed_tick` is the tick the client had last rendered when the player
/// pressed the button — transmitted via the `client_observed_tick` field on `PlayerIntent`
/// (repurposed from wall-clock timestamp to tick number by the client).
///
/// If `client_observed_tick == 0`, no rewind is applied (legacy or local client).
pub fn compute_rewind_ticks(current_tick: TickId, client_observed_tick: u64) -> u32 {
    if client_observed_tick == 0 {
        return 0;
    }
    let delta = current_tick.0.saturating_sub(client_observed_tick);
    (delta as u32).min(MAX_REWIND_TICKS)
}

// ── Shape intersection ──────────────────────────────────────────

/// Standard character hurtbox shape for intersection tests.
pub fn hurtbox_sensor_shape() -> SensorShape {
    SensorShape::Capsule {
        half_height: HURTBOX_HALF_HEIGHT,
        radius: HURTBOX_RADIUS,
    }
}

/// Convert a `SkillShape` to the sensor shape used for intersection tests.
/// Mirrors `skill_shape_to_sensor` in tick_pipeline.rs — kept in sync.
pub fn hitbox_sensor_shape(shape: SkillShape) -> SensorShape {
    match shape {
        SkillShape::Sphere       => SensorShape::Sphere { radius: 2.0 },
        SkillShape::Cone         => SensorShape::Capsule { half_height: 1.5, radius: 1.0 },
        SkillShape::CapsuleSweep => SensorShape::Capsule { half_height: 1.0, radius: 0.75 },
        SkillShape::Projectile   => SensorShape::Sphere { radius: 0.3 },
        SkillShape::LineSweep    => SensorShape::Capsule { half_height: 3.0, radius: 0.5 },
        SkillShape::HazardZone   => SensorShape::Sphere { radius: 5.0 },
    }
}

/// Run a standalone shape intersection test between a hitbox and a hurtbox
/// at specified world positions.
///
/// Uses Parry's `intersection_test` — no Rapier BVH, no physics state mutation.
/// The hitbox position includes the attacker's facing-rotated offset.
///
/// Returns `true` if the two shapes overlap.
pub fn shapes_intersect(
    hitbox_shape: SensorShape,
    hitbox_world_pos: Vec3f,
    hurtbox_world_pos: Vec3f,
) -> bool {
    use rapier3d::parry::query::intersection_test as parry_intersect;
    use rapier3d::parry::math::Pose3;
    use rapier3d::parry::shape::{Ball, Capsule};

    let iso_hitbox = Pose3::translation(hitbox_world_pos.x, hitbox_world_pos.y, hitbox_world_pos.z);
    let iso_hurtbox = Pose3::translation(hurtbox_world_pos.x, hurtbox_world_pos.y, hurtbox_world_pos.z);

    let hurtbox = Capsule::new_y(HURTBOX_HALF_HEIGHT, HURTBOX_RADIUS);

    match hitbox_shape {
        SensorShape::Sphere { radius } => {
            let shape = Ball::new(radius);
            parry_intersect(&iso_hitbox, &shape, &iso_hurtbox, &hurtbox).unwrap_or(false)
        }
        SensorShape::Capsule { half_height, radius } => {
            let shape = Capsule::new_y(half_height, radius);
            parry_intersect(&iso_hitbox, &shape, &iso_hurtbox, &hurtbox).unwrap_or(false)
        }
    }
}

/// Compute the world position of a hitbox sensor, applying the facing-rotated
/// entity-local offset to the attacker's current position.
///
/// The offset is in entity-local space where +Z is "forward". The `facing` vector
/// (unit XZ-plane direction from Phase 2 snapshot) rotates the offset:
///   world = pos + rotate_y(offset, atan2(facing.x, facing.z))
pub fn hitbox_world_position(attacker_pos: Vec3f, facing: Vec3f, offset: Vec3f) -> Vec3f {
    if offset.x.abs() < 1e-6 && offset.y.abs() < 1e-6 && offset.z.abs() < 1e-6 {
        return attacker_pos;
    }
    let yaw = facing.x.atan2(facing.z);
    let cos_yaw = yaw.cos();
    let sin_yaw = yaw.sin();
    // Rotate offset around Y axis by yaw, then translate by attacker position.
    Vec3f {
        x: attacker_pos.x + offset.x * cos_yaw + offset.z * sin_yaw,
        y: attacker_pos.y + offset.y,
        z: attacker_pos.z - offset.x * sin_yaw + offset.z * cos_yaw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_rewind_zero_observed() {
        assert_eq!(compute_rewind_ticks(TickId(100), 0), 0);
    }

    #[test]
    fn compute_rewind_normal_delta() {
        assert_eq!(compute_rewind_ticks(TickId(100), 98), 2);
    }

    #[test]
    fn compute_rewind_clamped_to_max() {
        assert_eq!(compute_rewind_ticks(TickId(100), 90), MAX_REWIND_TICKS);
    }

    #[test]
    fn compute_rewind_future_observed_returns_zero() {
        // Client claims to have observed a future tick — impossible, return 0.
        assert_eq!(compute_rewind_ticks(TickId(100), 105), 0);
    }

    #[test]
    fn transform_history_records_and_retrieves() {
        let mut history = TransformHistory::new();
        let positions = vec![
            (EntityId(1), Vec3f::new(1.0, 0.0, 0.0)),
            (EntityId(2), Vec3f::new(2.0, 0.0, 0.0)),
        ];
        history.record(TickId(10), positions);

        let pos = history.get_position(TickId(10), EntityId(1));
        assert!(pos.is_some());
        assert!((pos.unwrap().x - 1.0).abs() < 1e-6);
    }

    #[test]
    fn transform_history_returns_nearest_older_snapshot() {
        let mut history = TransformHistory::new();
        history.record(TickId(10), vec![(EntityId(1), Vec3f::new(1.0, 0.0, 0.0))]);
        history.record(TickId(12), vec![(EntityId(1), Vec3f::new(3.0, 0.0, 0.0))]);

        // Ask for tick 11 — should get tick 10's data
        let pos = history.get_position(TickId(11), EntityId(1)).unwrap();
        assert!((pos.x - 1.0).abs() < 1e-6);

        // Ask for tick 12 — should get tick 12's data
        let pos = history.get_position(TickId(12), EntityId(1)).unwrap();
        assert!((pos.x - 3.0).abs() < 1e-6);
    }

    #[test]
    fn transform_history_evicts_oldest() {
        let mut history = TransformHistory::new();
        for t in 0..MAX_HISTORY_TICKS as u64 + 5 {
            history.record(TickId(t), vec![(EntityId(1), Vec3f::new(t as f32, 0.0, 0.0))]);
        }
        assert!(history.len() <= MAX_HISTORY_TICKS);
        // Oldest should be evicted — tick 0 should not be findable
        assert!(history.get_position(TickId(0), EntityId(1)).is_none());
    }

    #[test]
    fn shapes_intersect_overlapping() {
        // Hitbox sphere at origin, hurtbox capsule at origin — must overlap.
        assert!(shapes_intersect(
            SensorShape::Sphere { radius: 2.0 },
            Vec3f::new(0.0, 0.0, 0.0),
            Vec3f::new(0.0, 0.0, 0.0),
        ));
    }

    #[test]
    fn shapes_intersect_separated() {
        // Hitbox sphere radius 1.0 at origin, hurtbox capsule 100 units away.
        assert!(!shapes_intersect(
            SensorShape::Sphere { radius: 1.0 },
            Vec3f::new(0.0, 0.0, 0.0),
            Vec3f::new(100.0, 0.0, 0.0),
        ));
    }

    #[test]
    fn shapes_intersect_edge_overlap() {
        // Sphere radius 2.0, hurtbox capsule radius 0.3 — should overlap when
        // centers are 2.2 apart (< 2.0 + 0.3 = 2.3).
        assert!(shapes_intersect(
            SensorShape::Sphere { radius: 2.0 },
            Vec3f::new(0.0, 0.0, 0.0),
            Vec3f::new(2.2, 0.0, 0.0),
        ));
    }

    #[test]
    fn hitbox_world_position_zero_offset() {
        let pos = hitbox_world_position(
            Vec3f::new(5.0, 1.0, 3.0),
            Vec3f::new(0.0, 0.0, 1.0),
            Vec3f::ZERO,
        );
        assert!((pos.x - 5.0).abs() < 1e-6);
        assert!((pos.z - 3.0).abs() < 1e-6);
    }

    #[test]
    fn hitbox_world_position_forward_offset() {
        // Facing +Z (forward), offset 2 units forward → world Z += 2.
        let pos = hitbox_world_position(
            Vec3f::new(0.0, 0.0, 0.0),
            Vec3f::new(0.0, 0.0, 1.0),
            Vec3f::new(0.0, 0.0, 2.0),
        );
        assert!((pos.x).abs() < 1e-4);
        assert!((pos.z - 2.0).abs() < 1e-4);
    }

    #[test]
    fn hitbox_world_position_rotated_offset() {
        // Facing +X (right), offset 2 units forward (+Z local) → world X += 2.
        let pos = hitbox_world_position(
            Vec3f::new(0.0, 0.0, 0.0),
            Vec3f::new(1.0, 0.0, 0.0),
            Vec3f::new(0.0, 0.0, 2.0),
        );
        assert!((pos.x - 2.0).abs() < 0.1, "x={}", pos.x);
        assert!((pos.z).abs() < 0.1, "z={}", pos.z);
    }
}
