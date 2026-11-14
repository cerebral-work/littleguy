---
status: implemented
created: 2026-11-13
tags: [littleguy, cortex, wake, channel, pool, littleman]
priority: high
depends_on:
  - 0005-lg-pool-consumer
  - 0006-container-image-and-api-key-file
---

# 0007-cortex-channel-wake

## 1. Summary

`lg serve` and `lg pool` gain native Cortex channel-based wake support (`EST-1614`)
so interactive `littleman` lanes and `littleman` pool workers wake immediately on
mesh notifications without PTY keystroke injection or blind `5_000 ms` HTTP polling:

1. **`WakeMessageId` boundary newtype (`littleguy-core::ids`):** Validates and
   lowercases a `36`-byte hyphenated hexadecimal UUID (`8-4-4-4-12`). Zero
   peer-chosen text crosses the wake boundary.
2. **Native `lane-wake` Unix socket listener (`littleguy::wake`):** Implements the
   `cortex-edge` `claude-channel` Unix socket contract (`${CORTEX_STATE_DIR:-$HOME/.cortex}/lane-wake/<safe_pane>.sock`
   or `--wake-socket <PATH>`) with `.sock.lock` `flock(LOCK_EX)` serialization,
   live-listener connect probing, `0700` parent directory and `0600` socket permissions,
   recorded `st_ino` unlink verification on drop, `SO_PEERCRED` same-UID verification,
   bounded `4_096`-byte / `5 s` newline JSON reads (`{"message_id":"<uuid>"}`), and
   `ok\n` / `err\n` / `err not your user\n` replies.
3. **`lg serve` `cortex` channel routing (`daemon` + `[cortex]` config):** Registers
   the `cortex` channel (`pairing = false`, allowed sender `cortex:mesh`) and routes
   each validated `WakeMessageId` as an idempotent `Inbound` op (`message_id = "cortex:<uuid>"`,
   fixed notice `"New cortex mesh message <uuid>. Read it with cortex__mesh_read."`).
   Wakes arriving while an approval wait is open are held in FIFO order and released
   when the approval settles.
4. **`lg pool` channel-based wake (`pool` + `PoolArgs`):** Supports both local Unix
   socket wake (`--wake-socket` / `LG_WAKE_SOCKET` / `LG_POOL_WAKE_SOCKET`) and Cortex
   SSE push stream wake (`--wake-channel` / `LG_POOL_WAKE_CHANNEL`, subscribing to
   `GET /v1/mesh/pool/wake?peer_id=<uuid>`). When at least one wake channel is live,
   `PoolWorker` enables its `Notify` waiter before `step_with_wait(0)`, drains ready
   tasks to `Idle`, and parks until woken or until `--wake-fallback-ms` (default
   `60_000 ms`) elapses. When no wake channel is live (including `404` or `405` from an
   older Cortex server), `PoolWorker` falls back seamlessly to standard `--wait-ms`
   long-polling (`REQ-LG-POOL-02`).

## 2. Config and CLI Surface

### `lg.toml` `[cortex]` section (optional)

```toml
[cortex]
wake = true
# Optional explicit override; otherwise derived from HERDR_SESSION + HERDR_PANE_ID:
# wake_socket = "/home/mark/.cortex/lane-wake/estate-w5Z-pG.sock"

[[mcp.server]]
id = "cortex"
command = "cortex-mcp"
env = ["HERDR_PANE_ID", "HERDR_SESSION", "CORTEX_API", "CORTEX_STATE_DIR", "CORTEX_HANDOFFS"]
labels = ["untrusted_input"]
effect = "external"
```

### CLI flags and environment variables

| Command | Flag | Environment | Default | Purpose |
|---|---|---|---|---|
| `lg serve` / `lg pool` | `--wake-socket <PATH>` | `LG_WAKE_SOCKET` (`LG_POOL_WAKE_SOCKET` fallback on `lg pool`) | `None` (or `[cortex]` config on `lg serve`) | Local `lane-wake` Unix socket path |
| `lg pool` | `--wake-channel` | `LG_POOL_WAKE_CHANNEL` | `false` | Subscribe to `GET /v1/mesh/pool/wake?peer_id=<uuid>` SSE stream |
| `lg pool` | `--wake-fallback-ms <MS>` | `LG_POOL_WAKE_FALLBACK_MS` | `60_000` | Safety reconciliation poll interval while parked on a live wake channel |

## 3. Bounds and Security Invariants

| Constant | Value | Purpose |
|---|---|---|
| `MAX_WAKE_LINE_BYTES` | `4_096` (4 KiB) | Cap on one newline JSON frame on the Unix wake socket or SSE stream |
| `WAKE_IO_TIMEOUT` | `5 s` | Per-connection read/write timeout on the Unix wake socket |
| `WAKE_CONNECT_PROBE_TIMEOUT` | `500 ms` | Connect probe timeout when checking whether an existing `.sock` has a live owner |
| `SSE_IDLE_TIMEOUT` | `45 s` | Chunk idle timeout on `GET /v1/mesh/pool/wake` (server sends `: keepalive` every `15 s`) |
| `DEFAULT_WAKE_FALLBACK_MS` | `60_000 ms` | Bounded reconciliation poll interval while a pool wake channel is live |
| `MIN_WAKE_FALLBACK_MS` | `250 ms` | Floor on `--wake-fallback-ms` so zero or tiny values never spin |

1. **Same-UID peer check:** Every connection on `WakeListener` checks `SO_PEERCRED`
   (`peer_cred().uid() == expected_uid`) before reading payload bytes. A different UID
   receives `err not your user\n` and is closed immediately.
2. **Exclusive lock and inode-safe cleanup:** `WakeListener::bind_for_uid` opens
   `<socket>.lock` with `O_CREAT | O_RDWR | O_NOFOLLOW` (`0600`) and acquires an
   exclusive `flock`. It refuses symlinks and non-socket files at `<socket>`, probes
   existing sockets via `UnixStream::connect` so a second process never steals a live
   listener's socket, sets mode `0600` on the bound socket, records `st_ino`, and on
   `Drop` unlinks `<socket>` only if `lstat(<socket>).st_ino` still matches the
   recorded inode.
3. **Zero untrusted text on the wake path:** `parse_wake_line` extracts only a
   validated `WakeMessageId` (`8-4-4-4-12` lowercase hex). Any extra or malformed
   text is discarded or rejected with `err\n`. The assistant prompt generated by
   `lg serve` contains only the fixed template and the validated UUID.

## 4. Requirements (EARS)

- **REQ-LG-WAKE-01:** When `WakeMessageId::new` validates a candidate string, it shall accept `8-4-4-4-12` ASCII hexadecimal UUIDs, normalize uppercase hex digits to lowercase, and reject empty, oversized, non-ASCII, or non-UUID inputs with `IdError`.
  Proof: unit tests in `littleguy-core::ids` and `littleguy::wake`.
- **REQ-LG-WAKE-02:** When `WakeListener::bind_for_uid` binds `<path>`, it shall hold `<path>.lock` under exclusive `flock`, refuse non-socket files and symlinks at `<path>`, refuse to unlink or steal `<path>` when a live listener answers `UnixStream::connect`, remove stale dead sockets, bind with mode `0600`, and unlink `<path>` on drop only when its inode matches the bound socket.
  Proof: tests `second_listener_refuses_to_steal_live_socket_and_preserves_owner`, `stale_dead_socket_is_reclaimed_and_unlinked_on_drop_by_inode`, `non_socket_path_is_refused_without_unlinking`.
- **REQ-LG-WAKE-03:** When a client connects to `WakeListener`, the listener shall reject peers whose `SO_PEERCRED` UID differs from `expected_uid` with `err not your user\n`, reject oversized (`> 4_096` bytes), malformed JSON, or non-UUID `message_id` lines with `err\n`, and reply `ok\n` after delivering a valid `WakeMessageId`.
  Proof: tests `wrong_uid_is_rejected_with_not_your_user`, `malformed_oversized_and_injection_lines_are_rejected_with_err`, `valid_wake_line_normalizes_uuid_and_replies_ok`.
- **REQ-LG-WAKE-04:** When `lg serve` runs with a `WakeListener` (via `--wake-socket` or `[cortex]`), each accepted `WakeMessageId` shall route over the `cortex` channel (`sender = "cortex:mesh"`, `message_id = "cortex:<uuid>"`) with fixed prompt text `"New cortex mesh message <uuid>. Read it with cortex__mesh_read."`, deduplicate repeated deliveries of the same UUID through `autumn-harvest`, and hold wakes arriving during an open approval wait until the decision settles.
  Proof: tests `daemon_wake_routes_fixed_notice_and_deduplicates_uuid`, `daemon_holds_wake_while_approval_waits_and_releases_after_decision`, `config_parses_cortex_section_and_derives_herdr_socket`.
- **REQ-LG-WAKE-05:** While `lg pool` runs with a live wake source (`--wake-socket` and/or `200 OK` `GET /v1/mesh/pool/wake`), `PoolWorker` shall register its `Notify` waiter before `step_with_wait(0)`, drain ready deliveries to `Idle`, park without polling `/v1/mesh/pool/recv` until woken or `--wake-fallback-ms` elapses, and fall back cleanly to `--wait-ms` long-polling when no wake source is live (including `404`/`405` on `/v1/mesh/pool/wake`).
  Proof: tests `pool_worker_parks_without_polling_and_wakes_immediately_on_unix_socket`, `pool_sse_wake_stream_triggers_immediate_recv_and_404_falls_back_cleanly`.
