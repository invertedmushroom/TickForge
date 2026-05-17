//! Commit cursor and in-flight queue authority.
//!
//! Owns tick advancement, idempotency checks, and retry/backpressure state.

use std::collections::VecDeque;
use std::time::Instant;

use log::{debug, error, info, warn};

/// Maximum number of retry attempts before giving up on a failed commit.
///
/// After exhaustion the process should exit so the supervisor can restart
/// the worker with a clean reseed.
const MAX_COMMIT_RETRIES: u32 = 3;

/// Default pipeline depth — how many commits may be in-flight at once.
///
/// A depth of 2 allows the simulation to compute tick N+1 while tick N's
/// commit is still round-tripping to the database, roughly doubling
/// throughput when commit latency exceeds tick simulation time.
const DEFAULT_PIPELINE_DEPTH: usize = 2;

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
    /// The pipeline is full — all slots occupied by in-flight commits.
    PipelineFull(u64),
}

/// One in-flight commit being tracked by the authority.
struct InFlightCommit {
    tick: u64,
    sent_at: Instant,
}

/// Centralized commit acknowledgement authority.
///
/// Owns the state fields that control tick advancement and pipelined
/// commit tracking.  Thread-safe usage is the caller's responsibility
/// (the coordinator wraps this inside `Arc<Mutex<CoordinatorState>>`).
pub struct CommitAuthority {
    last_processed_tick: u64,
    in_flight: VecDeque<InFlightCommit>,
    max_pipeline_depth: usize,
    retry_count: u32,
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
            in_flight: VecDeque::new(),
            max_pipeline_depth: DEFAULT_PIPELINE_DEPTH,
            retry_count: 0,
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

    /// Check whether a canonical tick can run now.
    pub fn can_process_tick(&self, tick: u64) -> CanProcessResult {
        // Already-committed ticks are covered by last_processed_tick plus
        // any in-flight ticks that have been simulated but not yet acked.
        let highest_simulated = self.last_processed_tick + self.in_flight.len() as u64;
        if tick <= highest_simulated {
            return CanProcessResult::AlreadyProcessed;
        }
        if self.in_flight.len() >= self.max_pipeline_depth {
            let oldest = self.in_flight.front().map(|c| c.tick).unwrap_or(0);
            if self.max_pipeline_depth == 1 {
                return CanProcessResult::CommitPending(oldest);
            }
            return CanProcessResult::PipelineFull(oldest);
        }
        CanProcessResult::Proceed
    }

    /// The next contiguous tick that must be processed to avoid reducer gaps.
    #[inline]
    pub fn next_expected_tick(&self) -> u64 {
        self.last_processed_tick + self.in_flight.len() as u64 + 1
    }

    /// Mark a tick as having a commit in-flight.
    ///
    /// Must be called *after* `can_process_tick` returns `Proceed` and
    /// *before* the async reducer call is issued.  Panics in debug mode
    /// if the pipeline is already full (invariant violation).
    pub fn mark_in_flight(&mut self, tick: u64) {
        debug_assert!(
            self.in_flight.len() < self.max_pipeline_depth,
            "mark_in_flight called while pipeline is full ({} in-flight, depth={})",
            self.in_flight.len(),
            self.max_pipeline_depth,
        );
        self.in_flight.push_back(InFlightCommit {
            tick,
            sent_at: Instant::now(),
        });
    }

    /// Acknowledge a successful commit — advance the cursor and pop the
    /// oldest in-flight entry.
    ///
    /// Commits must be acknowledged in FIFO order (tick N before N+1).
    pub fn acknowledge_success(&mut self, tick: u64) {
        let front = self.in_flight.front();
        debug_assert!(
            front.map(|c| c.tick) == Some(tick),
            "acknowledge_success({tick}) but front of pipeline is {:?}",
            front.map(|c| c.tick),
        );
        if let Some(commit) = self.in_flight.pop_front() {
            let latency_us = commit.sent_at.elapsed().as_micros();
            info!("tick={tick} commit_latency_us={latency_us}");
        }
        self.last_processed_tick = tick;
        self.retry_count = 0;
        debug!(
            "tick={tick} commit acknowledged — {} still in-flight",
            self.in_flight.len()
        );
    }

    /// Acknowledge a failed commit and decide whether to retry.
    ///
    /// - `Retry`: the oldest in-flight entry stays (blocking the pipeline
    ///    from filling further), retry counter incremented.  Caller should
    ///    re-send the payload.
    /// - `Exhausted`: retries exceeded `MAX_COMMIT_RETRIES`.  All in-flight
    ///    commits are tainted — the caller should crash for a clean reseed.
    pub fn acknowledge_failure(&mut self, tick: u64, reason: &str) -> FailureAction {
        self.retry_count += 1;
        if self.retry_count <= MAX_COMMIT_RETRIES {
            warn!(
                "tick={tick} commit failed (attempt {}/{}): {reason}",
                self.retry_count, MAX_COMMIT_RETRIES
            );
            FailureAction::Retry
        } else {
            // All in-flight commits are tainted — clear everything.
            self.in_flight.clear();
            self.retry_count = 0;
            error!(
                "tick={tick} commit failed after {MAX_COMMIT_RETRIES} retries: {reason} — \
                 giving up; all in-flight commits invalidated"
            );
            FailureAction::Exhausted
        }
    }

    /// The last tick whose commit was acknowledged by the server.
    pub fn last_processed_tick(&self) -> u64 {
        self.last_processed_tick
    }

    /// The oldest tick currently awaiting commit acknowledgement, if any.
    pub fn pending_tick(&self) -> Option<u64> {
        self.in_flight.front().map(|c| c.tick)
    }

    /// Number of commits currently in-flight.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
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
        assert_eq!(ca.in_flight_count(), 0);
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
        assert_eq!(ca.in_flight_count(), 1);

        // With depth=2, tick 12 is still allowed.
        assert_eq!(ca.can_process_tick(12), CanProcessResult::Proceed);

        ca.acknowledge_success(11);
        assert_eq!(ca.last_processed_tick(), 11);
        assert_eq!(ca.pending_tick(), None);
        assert_eq!(ca.in_flight_count(), 0);
        assert_eq!(ca.can_process_tick(12), CanProcessResult::Proceed);
    }

    #[test]
    fn pipeline_depth_2_allows_second_tick_before_ack() {
        let mut ca = CommitAuthority::new();
        ca.seed(10);

        // Tick 11 — first in-flight.
        assert_eq!(ca.can_process_tick(11), CanProcessResult::Proceed);
        assert_eq!(ca.next_expected_tick(), 11);
        ca.mark_in_flight(11);
        assert_eq!(ca.in_flight_count(), 1);

        // Tick 12 — second in-flight (pipeline depth = 2, still has room).
        assert_eq!(ca.can_process_tick(12), CanProcessResult::Proceed);
        assert_eq!(ca.next_expected_tick(), 12);
        ca.mark_in_flight(12);
        assert_eq!(ca.in_flight_count(), 2);

        // Tick 13 — pipeline full.
        assert!(matches!(
            ca.can_process_tick(13),
            CanProcessResult::PipelineFull(11)
        ));

        // Ack tick 11 — frees one slot.
        ca.acknowledge_success(11);
        assert_eq!(ca.last_processed_tick(), 11);
        assert_eq!(ca.in_flight_count(), 1);
        assert_eq!(ca.pending_tick(), Some(12));

        // Now tick 13 can proceed.
        assert_eq!(ca.can_process_tick(13), CanProcessResult::Proceed);
        assert_eq!(ca.next_expected_tick(), 13);
    }

    #[test]
    fn already_simulated_ticks_are_skipped() {
        let mut ca = CommitAuthority::new();
        ca.seed(10);

        ca.mark_in_flight(11);
        ca.mark_in_flight(12);

        // Tick 11 and 12 are in-flight (already simulated) — should be AlreadyProcessed.
        assert_eq!(ca.can_process_tick(11), CanProcessResult::AlreadyProcessed);
        assert_eq!(ca.can_process_tick(12), CanProcessResult::AlreadyProcessed);
    }

    #[test]
    fn failure_keeps_pending_for_retry() {
        let mut ca = CommitAuthority::new();
        ca.seed(10);

        assert_eq!(ca.can_process_tick(11), CanProcessResult::Proceed);
        ca.mark_in_flight(11);
        let action = ca.acknowledge_failure(11, "reducer rejected");

        // First failure → retry: cursor stays, in-flight stays, pipeline blocked.
        assert_eq!(action, FailureAction::Retry);
        assert_eq!(ca.last_processed_tick(), 10);
        assert_eq!(ca.pending_tick(), Some(11));
        assert_eq!(ca.retry_count(), 1);
    }

    #[test]
    fn exhausted_retries_clear_pipeline() {
        let mut ca = CommitAuthority::new();
        ca.seed(10);

        ca.mark_in_flight(11);
        ca.mark_in_flight(12);
        assert_eq!(ca.in_flight_count(), 2);

        // Fail MAX_COMMIT_RETRIES times → all return Retry.
        for i in 1..=MAX_COMMIT_RETRIES {
            let action = ca.acknowledge_failure(11, "transient error");
            assert_eq!(action, FailureAction::Retry);
            assert_eq!(ca.retry_count(), i);
        }

        // One more failure → Exhausted, entire pipeline cleared.
        let action = ca.acknowledge_failure(11, "final failure");
        assert_eq!(action, FailureAction::Exhausted);
        assert_eq!(ca.pending_tick(), None);
        assert_eq!(ca.in_flight_count(), 0);
        assert_eq!(ca.retry_count(), 0);
        assert_eq!(ca.last_processed_tick(), 10);
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

        // Send failure is retryable — in-flight stays.
        assert_eq!(action, FailureAction::Retry);
        assert_eq!(ca.pending_tick(), Some(11));
        assert_eq!(ca.last_processed_tick(), 10);
    }

    #[test]
    fn pipeline_acks_must_be_fifo() {
        let mut ca = CommitAuthority::new();
        ca.seed(10);

        ca.mark_in_flight(11);
        ca.mark_in_flight(12);

        // Ack 11 first (FIFO order).
        ca.acknowledge_success(11);
        assert_eq!(ca.last_processed_tick(), 11);
        assert_eq!(ca.in_flight_count(), 1);

        // Ack 12.
        ca.acknowledge_success(12);
        assert_eq!(ca.last_processed_tick(), 12);
        assert_eq!(ca.in_flight_count(), 0);
    }

    #[test]
    fn next_expected_tick_accounts_for_in_flight() {
        let mut ca = CommitAuthority::new();
        ca.seed(10);

        assert_eq!(ca.next_expected_tick(), 11);
        ca.mark_in_flight(11);
        assert_eq!(ca.next_expected_tick(), 12);
        ca.mark_in_flight(12);
        assert_eq!(ca.next_expected_tick(), 13);

        ca.acknowledge_success(11);
        assert_eq!(ca.next_expected_tick(), 13);
    }
}
