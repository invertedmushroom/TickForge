use std::fmt;

pub const DEFAULT_INTENT_INPUT_LEAD_TICKS: u64 = 1;
pub const DEFAULT_OBSERVED_FUTURE_TOLERANCE_TICKS: u64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntentClockBinding {
    pub target_tick: u64,
    pub client_observed_tick: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservedTickError {
    TooFarInFuture {
        observed_tick: u64,
        max_allowed_tick: u64,
    },
}

impl fmt::Display for ObservedTickError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooFarInFuture {
                observed_tick,
                max_allowed_tick,
            } => write!(
                f,
                "client_observed_tick {observed_tick} is too far in the future; max allowed is {max_allowed_tick}"
            ),
        }
    }
}

pub fn compute_intent_target_tick(current_tick: u64, lead_ticks: u64) -> u64 {
    current_tick.saturating_add(lead_ticks)
}

pub fn normalize_client_observed_tick(
    current_tick: u64,
    observed_tick: u64,
    max_rewind_ticks: u64,
    future_tolerance_ticks: u64,
) -> Result<u64, ObservedTickError> {
    if observed_tick == 0 {
        return Ok(0);
    }

    let max_allowed_tick = current_tick.saturating_add(future_tolerance_ticks);
    if observed_tick > max_allowed_tick {
        return Err(ObservedTickError::TooFarInFuture {
            observed_tick,
            max_allowed_tick,
        });
    }

    Ok(observed_tick.max(current_tick.saturating_sub(max_rewind_ticks)))
}

pub fn bind_intent_clock(
    current_tick: u64,
    lead_ticks: u64,
    client_observed_tick: u64,
    max_rewind_ticks: u64,
    future_tolerance_ticks: u64,
) -> Result<IntentClockBinding, ObservedTickError> {
    Ok(IntentClockBinding {
        target_tick: compute_intent_target_tick(current_tick, lead_ticks),
        client_observed_tick: normalize_client_observed_tick(
            current_tick,
            client_observed_tick,
            max_rewind_ticks,
            future_tolerance_ticks,
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_tick_uses_server_owned_input_lead() {
        assert_eq!(compute_intent_target_tick(40, 1), 41);
        assert_eq!(compute_intent_target_tick(40, 2), 42);
    }

    #[test]
    fn target_tick_saturates_on_overflow() {
        assert_eq!(compute_intent_target_tick(u64::MAX, 2), u64::MAX);
    }

    #[test]
    fn observed_tick_zero_stays_legacy_no_rewind() {
        assert_eq!(normalize_client_observed_tick(100, 0, 4, 1), Ok(0));
    }

    #[test]
    fn observed_tick_clamps_to_rewind_floor() {
        assert_eq!(normalize_client_observed_tick(100, 80, 4, 1), Ok(96));
    }

    #[test]
    fn observed_tick_allows_small_future_tolerance() {
        assert_eq!(normalize_client_observed_tick(100, 101, 4, 1), Ok(101));
    }

    #[test]
    fn observed_tick_rejects_implausible_future_tick() {
        assert_eq!(
            normalize_client_observed_tick(100, 102, 4, 1),
            Err(ObservedTickError::TooFarInFuture {
                observed_tick: 102,
                max_allowed_tick: 101,
            })
        );
    }

    #[test]
    fn binding_returns_target_and_normalized_observed_tick() {
        assert_eq!(
            bind_intent_clock(100, 1, 80, 4, 1),
            Ok(IntentClockBinding {
                target_tick: 101,
                client_observed_tick: 96,
            })
        );
    }
}
