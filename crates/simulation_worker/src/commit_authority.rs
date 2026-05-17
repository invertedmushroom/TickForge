//! Single owner of "what counts as committed."
//!
//! All tick advancement, idempotency checks, and in-flight commit tracking
//! are centralized here.  The coordinator delegates to `CommitAuthority`
//! instead of managing `last_processed_tick` and `pending_commit_tick`
//! fields directly.
//!
//! **Invariants:**
//! - `last_processed_tick` advances only via `acknowledge_success`.
//! - `pending_commit_tick` is set only via `mark_in_flight` and cleared
//!   only via `acknowledge_success` or `acknowledge_failure`.
//! - No tick is processed while another is in-flight (`can_process_tick`
//!   returns `CommitPending` when `pending_commit_tick.is_some()`).

use std::time::Instant;

use log::{debug, error, info, warn};

/// Maximum number of retry attempts before giving up on a failed commit.
///
/// After exhaustion, `pending_commit_tick` is cleared and server-side
/// backpressure stalls the pipeline naturally.
const MAX_COMMIT_RETRIES: u32 = 3;

/// What the caller should do after a commit failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureAction {
    /// Re-send the same payload — pending remains set, pipeline blocked.
    Retry,
    /// Retries exhausted — pending cleared, pipeline unblocked, backpressure engages.
    Exhausted,
}

/// Result of checking whether a tick can be processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanProcessResult {
    /// The tick is eligible for processing.
    Proceed,
    /// The tick has already been committed — skip silently.
    AlreadyProcessed,
    /// A commit for the given tick is still in-flight — skip with warning.
    CommitPending(u64),
}

/// Centralized commit acknowledgement authority.
///
/// Owns the two state fields that control tick advancement and commit
/// pipelining.  Thread-safe usage is the caller's responsibility (the
/// coordinator wraps this inside `Arc<Mutex<CoordinatorState>>`).
pub struct CommitAuthority {
    last_processed_tick: u64,
    pending_commit_tick: Option<u64>,
    retry_count: u32,
    commit_sent_at: Option<Instant>,
}

impl Default for CommitAuthority {
    fn default() -> Self {
        Self::new()
    }
}

impl CommitAuthority {
    /// Create a new authority starting at tick 0 with nothing in-flight.
    pub fn new() -> Self {
        Self {
            last_processed_tick: 0,
            pending_commit_tick: None,
            retry_count: 0,
            commit_sent_at: None,
        }
    }

    /// Seed the baseline tick from the subscription snapshot.
    ///
    /// Called once in `on_applied` to set the high-water mark from DB state
    /// so historical ticks are not replayed after a worker restart.
    pub fn seed(&mut self, tick: u64) {
        self.last_processed_tick = tick;
        debug!("CommitAuthority seeded at tick={tick}");
    }

    /// Check whether the given tick should be processed.
    ///
    /// Three outcomes:
    /// - `Proceed` — tick is new and no commit is in-flight.
    /// - `AlreadyProcessed` — tick ≤ last committed tick.
    /// - `CommitPending(pending)` — a prior commit is still unacknowledged.
    pub fn can_process_tick(&self, tick: u64) -> CanProcessResult {
        if tick <= self.last_processed_tick {
            return CanProcessResult::AlreadyProcessed;
        }
        if let Some(pending) = self.pending_commit_tick {
            return CanProcessResult::CommitPending(pending);
        }
        CanProcessResult::Proceed
    }

    /// Mark a tick as having a commit in-flight.
    ///
    /// Must be called *after* `can_process_tick` returns `Proceed` and
    /// *before* the async reducer call is issued.  Panics in debug mode
    /// if another commit is already pending (invariant violation).
    pub fn mark_in_flight(&mut self, tick: u64) {
        debug_assert!(
            self.pending_commit_tick.is_none(),
            "mark_in_flight called while tick={} is still pending",
            self.pending_commit_tick.unwrap_or(0),
        );
        self.pending_commit_tick = Some(tick);
        self.commit_sent_at = Some(Instant::now());
    }

    /// Acknowledge a successful commit — advance the cursor and clear pending.
    pub fn acknowledge_success(&mut self, tick: u64) {
        if let Some(sent_at) = self.commit_sent_at.take() {
            let latency_us = sent_at.elapsed().as_micros();
            info!("tick={tick} commit_latency_us={latency_us}");
        }
        self.last_processed_tick = tick;
        self.pending_commit_tick = None;
        self.retry_count = 0;
        debug!("tick={tick} commit acknowledged");
    }

    /// Acknowledge a failed commit and decide whether to retry.
    ///
    /// - `Retry`: `pending_commit_tick` stays set (blocking new ticks),
    ///    retry counter incremented.  Caller should buffer the payload
    ///    and re-send on the next opportunity.
    /// - `Exhausted`: retries exceeded `MAX_COMMIT_RETRIES`, pending
    ///    cleared, retry counter reset.  Server-side backpressure will
    ///    stall `tick_trigger` because `last_committed_tick` never advanced.
    pub fn acknowledge_failure(&mut self, tick: u64, reason: &str) -> FailureAction {
        self.retry_count += 1;
        if self.retry_count <= MAX_COMMIT_RETRIES {
            // Keep pending set — blocks new ticks until retry succeeds.
            warn!(
                "tick={tick} commit failed (attempt {}/{}): {reason}",
                self.retry_count, MAX_COMMIT_RETRIES
            );
            FailureAction::Retry
        } else {
            self.pending_commit_tick = None;
            self.retry_count = 0;
            error!(
                "tick={tick} commit failed after {MAX_COMMIT_RETRIES} retries: {reason} — \
                 giving up; backpressure will engage"
            );
            FailureAction::Exhausted
        }
    }

    /// The last tick whose commit was acknowledged by the server.
    pub fn last_processed_tick(&self) -> u64 {
        self.last_processed_tick
    }

    /// The tick currently awaiting commit acknowledgement, if any.
    pub fn pending_tick(&self) -> Option<u64> {
        self.pending_commit_tick
    }

    /// Current retry attempt count (0 = no failure yet).
    pub fn retry_count(&self) -> u32 {
        self.retry_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_authority_starts_at_zero() {
        let ca = CommitAuthority::new();
        assert_eq!(ca.last_processed_tick(), 0);
        assert_eq!(ca.pending_tick(), None);
    }

    #[test]
    fn seed_sets_baseline() {
        let mut ca = CommitAuthority::new();
        ca.seed(42);
        assert_eq!(ca.last_processed_tick(), 42);
        assert_eq!(ca.can_process_tick(42), CanProcessResult::AlreadyProcessed);
        assert_eq!(ca.can_process_tick(43), CanProcessResult::Proceed);
    }

    #[test]
    fn normal_commit_flow() {
        let mut ca = CommitAuthority::new();
        ca.seed(10);

        assert_eq!(ca.can_process_tick(11), CanProcessResult::Proceed);
        ca.mark_in_flight(11);
        assert_eq!(ca.pending_tick(), Some(11));

        // New ticks blocked while commit is in-flight.
        assert_eq!(ca.can_process_tick(12), CanProcessResult::CommitPending(11));

        ca.acknowledge_success(11);
        assert_eq!(ca.last_processed_tick(), 11);
        assert_eq!(ca.pending_tick(), None);
        assert_eq!(ca.can_process_tick(12), CanProcessResult::Proceed);
    }

    #[test]
    fn failure_keeps_pending_for_retry() {
        let mut ca = CommitAuthority::new();
        ca.seed(10);

        assert_eq!(ca.can_process_tick(11), CanProcessResult::Proceed);
        ca.mark_in_flight(11);
        let action = ca.acknowledge_failure(11, "reducer rejected");

        // First failure → retry: cursor stays, pending stays set, pipeline blocked.
        assert_eq!(action, FailureAction::Retry);
        assert_eq!(ca.last_processed_tick(), 10);
        assert_eq!(ca.pending_tick(), Some(11));
        assert_eq!(ca.retry_count(), 1);
        assert_eq!(ca.can_process_tick(12), CanProcessResult::CommitPending(11));
    }

    #[test]
    fn exhausted_retries_clear_pending() {
        let mut ca = CommitAuthority::new();
        ca.seed(10);

        ca.mark_in_flight(11);

        // Fail MAX_COMMIT_RETRIES times → all return Retry.
        for i in 1..=MAX_COMMIT_RETRIES {
            let action = ca.acknowledge_failure(11, "transient error");
            assert_eq!(action, FailureAction::Retry);
            assert_eq!(ca.retry_count(), i);
            assert_eq!(ca.pending_tick(), Some(11));
        }

        // One more failure → Exhausted, pending cleared.
        let action = ca.acknowledge_failure(11, "final failure");
        assert_eq!(action, FailureAction::Exhausted);
        assert_eq!(ca.pending_tick(), None);
        assert_eq!(ca.retry_count(), 0);
        assert_eq!(ca.last_processed_tick(), 10);
        // Pipeline unblocked — server backpressure will stall independently.
        assert_eq!(ca.can_process_tick(12), CanProcessResult::Proceed);
    }

    #[test]
    fn success_after_failure_resets_retry_count() {
        let mut ca = CommitAuthority::new();
        ca.seed(10);

        ca.mark_in_flight(11);
        let action = ca.acknowledge_failure(11, "transient");
        assert_eq!(action, FailureAction::Retry);
        assert_eq!(ca.retry_count(), 1);

        // Retry succeeds.
        ca.acknowledge_success(11);
        assert_eq!(ca.retry_count(), 0);
        assert_eq!(ca.last_processed_tick(), 11);
        assert_eq!(ca.pending_tick(), None);
    }

    #[test]
    fn already_processed_ticks_are_skipped() {
        let mut ca = CommitAuthority::new();
        ca.seed(5);

        assert_eq!(ca.can_process_tick(3), CanProcessResult::AlreadyProcessed);
        assert_eq!(ca.can_process_tick(5), CanProcessResult::AlreadyProcessed);
        assert_eq!(ca.can_process_tick(6), CanProcessResult::Proceed);
    }

    #[test]
    fn consecutive_commits() {
        let mut ca = CommitAuthority::new();

        for tick in 1..=5 {
            assert_eq!(ca.can_process_tick(tick), CanProcessResult::Proceed);
            ca.mark_in_flight(tick);
            ca.acknowledge_success(tick);
            assert_eq!(ca.last_processed_tick(), tick);
        }
    }

    #[test]
    fn send_failure_triggers_retry() {
        let mut ca = CommitAuthority::new();
        ca.seed(10);

        ca.mark_in_flight(11);
        // Simulate send_result Err — coordinator calls acknowledge_failure.
        let action = ca.acknowledge_failure(11, "send failed");

        // Send failure is retryable — pending stays set.
        assert_eq!(action, FailureAction::Retry);
        assert_eq!(ca.pending_tick(), Some(11));
        assert_eq!(ca.last_processed_tick(), 10);
    }
}
