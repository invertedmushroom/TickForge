use serde::{Deserialize, Serialize};

/// Canonical simulation tick identifier.
///
/// All authoritative state changes reference a tick.
/// The tick number is the single source of time truth
/// for the simulation — wall clock is never used for
/// gameplay decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TickId(pub u64);

impl TickId {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl std::fmt::Display for TickId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tick({})", self.0)
    }
}

/// Configuration for the simulation tick rate.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TickConfig {
    /// Ticks per second (e.g. 20 for 20Hz).
    pub rate_hz: u32,
    /// Fixed delta time in seconds, derived from rate_hz.
    /// Stored explicitly to avoid repeated division.
    pub dt: f32,
    /// Global maximum lag compensation rewind depth (ticks).
    /// Per-ability `max_rewind_ticks` is capped by this value.
    /// Default: 4 (200 ms at 20 Hz).
    pub global_max_rewind_ticks: u32,
}

impl TickConfig {
    pub fn new(rate_hz: u32) -> Self {
        Self {
            rate_hz,
            dt: 1.0 / rate_hz as f32,
            global_max_rewind_ticks: 4,
        }
    }

    /// Default 20Hz tick rate (50ms per tick).
    /// Matches the SpacetimeDB scheduled reducer tutorial pattern.
    pub fn default_20hz() -> Self {
        Self::new(20)
    }
}
