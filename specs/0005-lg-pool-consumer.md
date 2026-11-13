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

`lg pool` accepts `--cortex-url`, `--token-file`, `--allowed-sender` (default
`service-escapement-pool`), `--wait-ms` (default `5000`), and `--shell-id`, plus
optional `--peer-id`, `--role` (default `service-pool-littleman-1`), `--config`,
`--db`, `--model`, `--model-name`, and `--base-url`.

The bearer token file at `--token-file` must satisfy three checks via
`symlink_metadata` before `lg pool` starts:
1. It is a regular file and not a symbolic link.
2. Its permission mode is `0600` (`mode & 0o7777 == 0o600`).
3. Its owner UID equals the effective UID of the `lg` process.

`PoolToken` redacts its `Debug` output and never exposes token bytes in logs or
errors.

## 3. Bounds and Wire Contract

| Bound | Value | `escapement-serve` source |
|---|---|---|
| `MAX_DELIVERY_BODY_BYTES` | `12_288` (12 KiB) | `escapement-serve/src/pool.rs:44` (`POOL_BODY_MAX`) |
| `MAX_DELIVERY_SUBJECT_BYTES` | `512` | `escapement-serve/src/pool.rs:48` (`POOL_SUBJECT_MAX`) |
| `MAX_DELIVERY_REFS_BYTES` | `16_384` (16 KiB) | `escapement-serve/src/pool.rs:50` (`POOL_REFS_MAX`) |
| `MAX_REPLY_BODY_BYTES` | `8_192` (8 KiB) | `escapement-serve/src/pool.rs:1005` (`MAX_RESULT_BYTES`) |
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
| `refs.error` | Failure text when `"failed"` | `escapement-serve/src/pool.rs:1040` |
| `refs.ghost_id`, `refs.molt_epoch` | Included when `ghost_binding` is present; omitted when absent or `null` | GHOST receipt provenance (`specs/0004`) |
| `refs.transcript_sha256` | 64-char lowercase hex SHA-256 of the turn transcript | Turn provenance |

## 4. Requirements (EARS)

- **REQ-LG-POOL-01:** When `lg pool` starts, the loader shall check `--token-file` with `symlink_metadata` and shall refuse to start if the path is not a regular file, has mode other than `0600`, or is not owned by the current user, without printing the token in `Debug`, logs, or errors.
  Proof: test `token_file_mode_other_than_0600_or_wrong_owner_refuses_to_start_and_redacts`.
- **REQ-LG-POOL-02:** When a delivery arrives from `POST /v1/mesh/pool/recv` (`free_slots: 1`) with `from_session` different from `--allowed-sender` or with a field over its section 3 bound, the worker shall call `POST /v1/mesh/pool/term` with a reason and shall not run a turn.
  Proof: tests `wrong_sender_is_refused_with_term_and_runs_no_turn`, `malformed_ghost_binding_and_oversized_body_are_termed_without_turn`.
- **REQ-LG-POOL-03:** Where a delivery carries a non-null `ghost_binding`, the worker shall validate it with `littleguy_core::ghost_binding::GhostBinding` and, if invalid, shall call `POST /v1/mesh/pool/term` with a reason and shall not run a turn.
  Proof: test `malformed_ghost_binding_and_oversized_body_are_termed_without_turn`.
- **REQ-LG-POOL-04:** Where a delivery carries `"ghost_binding": null`, the worker shall treat `ghost_binding` as absent, matching a missing key.
  Proof: test `null_ghost_binding_is_treated_as_absent_and_runs_unbound_turn`.
- **REQ-LG-POOL-05:** While a valid turn runs behind `TurnRunner` (`RuntimeTurnRunner` on `LittleguySeatSubstrate` and single-writer `SqliteRuntime`), the worker shall extend the lease via `POST /v1/mesh/pool/extend`.
  Proof: test `happy_path_runs_turn_extends_lease_sends_reply_and_acks`.
- **REQ-LG-POOL-06:** When a turn finishes, the worker shall post the bounded result to `POST /v1/mesh/send` with the section 3 `refs` keys and then call `POST /v1/mesh/pool/ack`, or call `POST /v1/mesh/pool/nack` with a delay on transient turn or send failure.
  Proof: tests `happy_path_runs_turn_extends_lease_sends_reply_and_acks`, `transient_turn_failure_nacks_with_delay`.
- **REQ-LG-POOL-07:** When a `(task_id, attempt)` entry already exists in the bounded result cache, the worker shall re-send the cached reply and call `ack` without running a second turn.
  Proof: test `redelivery_resends_cached_reply_without_running_a_second_turn`.
