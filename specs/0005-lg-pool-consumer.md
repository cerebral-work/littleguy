---
status: implemented
created: 2026-11-13
tags: [littleguy, pool, cortex, escapement, ghost, littleman]
priority: high
depends_on:
  - 0001-substrate-seat-and-biscuit-mandate-integration
  - 0004-ghost-binding
---

# 0005 - `lg pool`: the littleman pool consumer

## 1. Overview

`lg pool` pulls `littleman` tasks from Cortex (`/v1/mesh/pool/recv`), validates
sender identity, payload size, and any optional `GhostBinding` (`specs/0004`),
runs one bounded turn per `(task_id, attempt)` on the single-writer runtime and
seat substrate, replies to `escapement-serve` over `/v1/mesh/send`, and settles
the lease (`ack`, `nack`, or `term`).

## 2. CLI and Token File Hygiene

`lg pool` requires `--cortex-url`, `--token-file`, `--peer-id` (hyphenated UUID,
`8-4-4-4-12` hex, no default), and `--shell-id`, plus `--allowed-sender`
(default `service-escapement-pool`), `--wait-ms` (default `5000`), `--role`
(default `service-pool-littleman-1`), `--config`, `--db`, `--model`,
`--model-name`, and `--base-url`.

The bearer token file at `--token-file` is opened with `O_NOFOLLOW | O_NONBLOCK`
and must satisfy four checks on the opened file descriptor before `lg pool`
starts:
1. It is a regular file (`is_file()`) and not a symbolic link, FIFO, socket, or directory.
2. Its permission mode is `0600` (`mode & 0o7777 == 0o600`).
3. Its owner UID equals the effective UID (`geteuid`) of the `lg` process.
4. Its size is at most `MAX_TOKEN_FILE_BYTES` (`4_096` bytes).

`PoolToken` redacts its `Debug` output and never exposes token bytes in logs or
errors.

## 3. Bounds and Wire Contract

| Bound | Value | `escapement-serve` source |
|---|---|---|
| `MAX_TOKEN_FILE_BYTES` | `4_096` (4 KiB) | Bounded `--token-file` read |
| `MAX_DELIVERY_BODY_BYTES` | `12_288` (12 KiB) | `escapement-serve/src/pool.rs:44` (`POOL_BODY_MAX`) |
| `MAX_DELIVERY_SUBJECT_BYTES` | `512` | `escapement-serve/src/pool.rs:48` (`POOL_SUBJECT_MAX`) |
| `MAX_DELIVERY_REFS_BYTES` | `16_384` (16 KiB) | `escapement-serve/src/pool.rs:50` (`POOL_REFS_MAX`) |
| `MAX_REPLY_BODY_BYTES` | `8_192` (8 KiB) | `escapement-serve/src/pool.rs:1005` (`MAX_RESULT_BYTES`) |
| `MAX_ERROR_CHARS` | `1_024` chars | Char-boundary cap on `refs.error` and `body.error` |
| `MAX_CACHED_RESULTS` | `256` | Bounded FIFO `(TaskId, attempt)` idempotency map |

Inbound reply fields and `refs` keys that `escapement-serve` parses:

| Field or `refs` key | Rule | `escapement-serve` source |
|---|---|---|
| `message.kind` | `"msg"` | `escapement-serve/src/pool.rs:957` |
| `message.from_session` | `service-pool-littleman-<n>` | `escapement-serve/src/pool.rs:182`, `:472`, `:971` |
| `message.body` | JSON object text, at most `8_192` bytes | `escapement-serve/src/pool.rs:1005`, `:1013` |
| `refs.task_id` | Task id string | `escapement-serve/src/pool.rs:960` |
| `refs.attempt` | Attempt number (`u32`) | `escapement-serve/src/pool.rs:978` |
| `refs.stream_seq` | Delivery stream sequence (`u64`) | `escapement-serve/src/pool.rs:987` |
| `refs.outcome` | `"ok"` or `"failed"` | `escapement-serve/src/pool.rs:996`, `:999`, `:1021` |
| `refs.transient` | Boolean retry hint when `"failed"` | `escapement-serve/src/pool.rs:1033` |
| `refs.error` | Failure text when `"failed"`, capped to `1_024` chars on a UTF-8 boundary | `escapement-serve/src/pool.rs:1040` |
| `refs.ghost_id`, `refs.molt_epoch` | Included when `ghost_binding` is present; omitted when absent or `null` | GHOST receipt provenance (`specs/0004`) |
| `refs.transcript_sha256` | 64-char lowercase hex SHA-256 of the turn transcript | Turn provenance |

## 4. Requirements (EARS)

- **REQ-LG-POOL-01:** When `lg pool` starts, the CLI shall require a hyphenated UUID `--peer-id`, and the loader shall open `--token-file` with `O_NOFOLLOW | O_NONBLOCK` and refuse to start if the opened fd is not a regular file, has mode other than `0600`, is not owned by the effective UID (`geteuid`), or exceeds `4_096` bytes, without printing the token in `Debug`, logs, or errors.
  Proof: tests `token_file_mode_other_than_0600_or_wrong_owner_refuses_to_start_and_redacts`, `symlink_and_oversize_token_file_are_refused`, `malformed_or_missing_peer_id_is_refused_at_startup`.
- **REQ-LG-POOL-02:** When a delivery arrives from `POST /v1/mesh/pool/recv` (`free_slots: 1`) with a malformed envelope, an invalid `delivery_token`, `from_session` different from `--allowed-sender`, or a field over its section 3 bound, the worker shall log bounded fields, settle with `POST /v1/mesh/pool/term` when Cortex can accept the token (or skip so the lease expires), not run a turn, and continue polling.
  Proof: tests `wrong_sender_is_refused_with_term_and_runs_no_turn`, `malformed_ghost_binding_and_oversized_body_are_termed_without_turn`, `bad_delivery_token_and_malformed_recv_do_not_stop_worker`.
- **REQ-LG-POOL-03:** Where a delivery carries a non-null `ghost_binding`, the worker shall validate it with `littleguy_core::ghost_binding::GhostBinding` and, if invalid, shall call `POST /v1/mesh/pool/term` with a reason and shall not run a turn.
  Proof: test `malformed_ghost_binding_and_oversized_body_are_termed_without_turn`.
- **REQ-LG-POOL-04:** Where a delivery carries `"ghost_binding": null`, the worker shall treat `ghost_binding` as absent, matching a missing key.
  Proof: test `null_ghost_binding_is_treated_as_absent_and_runs_unbound_turn`.
- **REQ-LG-POOL-05:** While a valid turn runs behind `TurnRunner` (`RuntimeTurnRunner` on `LittleguySeatSubstrate` and single-writer `SqliteRuntime`), the worker shall extend the lease via `POST /v1/mesh/pool/extend`.
  Proof: test `happy_path_runs_turn_extends_lease_sends_reply_and_acks`.
- **REQ-LG-POOL-06:** When a turn finishes, the worker shall post the bounded result (`refs.error` and `body.error` capped at `1_024` chars) to `POST /v1/mesh/send` and call `POST /v1/mesh/pool/ack`; retryable turn or send failures shall keep any cached reply and call `nack` with delay, non-retryable send failures shall call `term` with a reason, and only startup/config or `401`/`403` auth errors shall stop the worker.
  Proof: tests `happy_path_runs_turn_extends_lease_sends_reply_and_acks`, `transient_turn_failure_nacks_with_delay`, `non_retryable_422_on_send_settles_delivery_and_handles_next`, `permanent_failure_caps_refs_error_on_char_boundary`.
- **REQ-LG-POOL-07:** When a `(task_id, attempt)` entry already exists in the bounded result cache, the worker shall re-send the cached reply and call `ack` without running a second turn.
  Proof: test `redelivery_resends_cached_reply_without_running_a_second_turn`.
