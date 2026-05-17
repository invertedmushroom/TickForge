# Community 8

> 48 nodes · cohesion 0.11

## Key Concepts

- **.process_tick()** (19 connections) — `simulation_worker\src\tick_driver.rs`
- **commit_authority.rs** (18 connections) — `simulation_worker\src\commit_authority.rs`
- **.new()** (17 connections) — `simulation_worker\src\commit_authority.rs`
- **CommitAuthority** (14 connections) — `simulation_worker\src\commit_authority.rs`
- **.mark_in_flight()** (13 connections) — `simulation_worker\src\commit_authority.rs`
- **.seed()** (13 connections) — `simulation_worker\src\commit_authority.rs`
- **tick_driver.rs** (11 connections) — `simulation_worker\src\tick_driver.rs`
- **mock_pipeline()** (11 connections) — `simulation_worker\src\tick_driver.rs`
- **.new()** (11 connections) — `simulation_worker\src\tick_driver.rs`
- **.acknowledge_success()** (8 connections) — `simulation_worker\src\commit_authority.rs`
- **success_after_failure_resets_retry_count()** (6 connections) — `simulation_worker\src\commit_authority.rs`
- **acked_tick_cannot_be_reprocessed_or_resent()** (5 connections) — `simulation_worker\src\commit_authority.rs`
- **.acknowledge_failure()** (5 connections) — `simulation_worker\src\commit_authority.rs`
- **exhausted_retries_clear_pipeline()** (5 connections) — `simulation_worker\src\commit_authority.rs`
- **failure_keeps_pending_for_retry()** (5 connections) — `simulation_worker\src\commit_authority.rs`
- **next_expected_tick_accounts_for_in_flight()** (5 connections) — `simulation_worker\src\commit_authority.rs`
- **normal_commit_flow()** (5 connections) — `simulation_worker\src\commit_authority.rs`
- **pipeline_acks_must_be_fifo()** (5 connections) — `simulation_worker\src\commit_authority.rs`
- **pipeline_depth_2_allows_second_tick_before_ack()** (5 connections) — `simulation_worker\src\commit_authority.rs`
- **send_failure_triggers_retry()** (5 connections) — `simulation_worker\src\commit_authority.rs`
- **already_processed_tick_is_skipped()** (5 connections) — `simulation_worker\src\tick_driver.rs`
- **backlog_processes_next_expected_tick()** (5 connections) — `simulation_worker\src\tick_driver.rs`
- **consecutive_ticks_after_ack()** (5 connections) — `simulation_worker\src\tick_driver.rs`
- **pipelined_ticks_without_intermediate_ack()** (5 connections) — `simulation_worker\src\tick_driver.rs`
- **test_registry()** (5 connections) — `simulation_worker\src\tick_driver.rs`
- *... and 23 more nodes in this community*

## Relationships

- No strong cross-community connections detected

## Source Files

- `simulation_worker\src\commit_authority.rs`
- `simulation_worker\src\tick_driver.rs`
- `simulation_worker\src\tick_pipeline\mod.rs`

## Audit Trail

- EXTRACTED: 241 (90%)
- INFERRED: 28 (10%)
- AMBIGUOUS: 0 (0%)

---

*Part of the graphify knowledge wiki. See [[index]] to navigate.*