---
status: implemented
created: 2026-11-13
tags: [littleguy, reflex, blackwall, custody, pool, littleman]
priority: high
depends_on:
  - 0005-lg-pool-consumer
  - 0006-container-image-and-api-key-file
  - 0007-cortex-channel-wake
---

# 0008-reflex-gate-and-blackwall-custody

## 1. Summary

`lg serve` and `lg pool` gain native pre-tool `reflexd` classification (`POST /v1/classify`)
and post-turn `blackwall-serve` custody bundle import (`POST /v1/projects/{project}/runs/import`)
so governed `littleman` pool runs satisfy `ves-domain` `TaskResult::outcome()` (`exit: "ok"`
with a verified 64-hex `blake3` `run_hash`) and every tool invocation on an opted-in lane or
pool worker fails closed on `NEVER`, unknown tiers, HTTP failures, or timeouts:

1. **`ReflexToolPolicy` (`littleguy::reflex_gate`):** Implements `autumn_harvest_agent::ToolPolicy`
   for `lg serve` (`allow_approval = true`) and `lg pool` (`allow_approval = false`). Maps each
   `ToolCall` and optional `Tool` (`ToolEffect`) to a `ves-domain` `ClassifyBody` (`agent`,
   `verb`, `params: { target: <tool_name> }`, 64-hex `payload_sha256`) and sends a `5 s` bounded
   `POST` to `--reflex-classify-url`.
2. **Tier handling and fail-closed safety (`guard` + `daemon` + `pool`):**
   - `GREEN` / `YELLOW` (case-insensitive): delegates to the inner `ToolPolicy` (`ApprovalGate`
     in `lg serve`, or `ToolDecision::Allow` in `lg pool`).
   - `RED` (case-insensitive): records the pending call in `SqliteStores` and returns
     `ToolDecision::RequireApproval { reason }` in `lg serve`, or returns `ToolDecision::Deny { reason }`
     in headless `lg pool`.
   - `NEVER`, unknown tier, non-2xx HTTP status, `5 s` timeout, or transport/JSON error: fails
     closed with `ToolDecision::Deny { reason }`.
3. **`BlackwallProject` boundary newtype (`littleguy-core::ids`):** Validates 1- or 2-segment
   project slugs matching `[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)?` (`<= 128` bytes) and rejects
   empty segments, `.`, and `..`. Delivery JSON bodies in `lg pool` may carry an optional
   `"project"` field that overrides `--blackwall-project`; invalid `"project"` values are
   rejected fail-closed with `term`.
4. **`BlackwallCustodyClient` (`littleguy::custody`):** Loads `--blackwall-token-file` through
   `SecretFile::load` (`O_NOFOLLOW`, regular file, mode `0600`, effective UID, `<= 4_096` bytes,
   redacted `Debug`), constructs a `CustodyBundle` (`version: 1`, `run_hash` = `blake3` hex of
   `serde_json::to_string_pretty(&record)` matching `blackwall_store::RunRecord` byte-for-byte,
   `context` with `parent_world = "littleguy-pool"`, `changeset = ""`, `settle_state = "unsettled"`,
   empty `blobs`), posts it to `{blackwall_url}/v1/projects/{project}/runs/import` (`10 s`
   timeout), and verifies both a `2xx` status and an exact `run_hash` match in the response.
5. **Governed `TaskResult` reply envelope (`littleguy::pool`):** `build_ok_reply` always emits
   `"exit": "ok"` in `body`, and when `out.run_hash` is `Some(h)` emits `"run_hash": h` and
   `"custody": "blackwall-serve"` in `body` plus `"run_hash": h` in `refs`, preserving all
   metadata fields when truncating oversized answers under `MAX_REPLY_BODY_BYTES` (`8_192` bytes).
   `build_failed_reply` emits `"exit": "err"` alongside `"error"` and `"shell_id"`.

## 2. CLI and Environment Surface

| Command | Flag | Environment | Default | Purpose |
|---|---|---|---|---|
| `lg serve` / `lg pool` | `--reflex-classify-url <URL>` | `LG_REFLEX_CLASSIFY_URL` | `None` | `reflexd` base URL or `/v1/classify` endpoint for pre-tool gate checks |
| `lg pool` | `--blackwall-url <URL>` | `LG_BLACKWALL_URL` | `None` | `blackwall-serve` base URL for run custody bundle import |
| `lg pool` | `--blackwall-token-file <PATH>` | `LG_BLACKWALL_TOKEN_FILE` | `None` (required iff `--blackwall-url` is set) | `0600` regular file holding the `blackwall-serve` bearer token |
| `lg pool` | `--blackwall-project <SLUG>` | `LG_BLACKWALL_PROJECT` | `"vespertin"` | Default 1- or 2-segment Blackwall project slug (`org/repo` or `project`) |

## 3. Wire Contracts, Bounds, and Security Invariants

| Constant | Value | Purpose |
|---|---|---|
| `REFLEX_CLASSIFY_TIMEOUT` | `5 s` | Per-request timeout on `POST /v1/classify` |
| `BLACKWALL_IMPORT_TIMEOUT` | `10 s` | Per-request timeout on `POST /v1/projects/{project}/runs/import` |
| `CUSTODY_BUNDLE_VERSION` | `1` | `blackwall_store::CUSTODY_BUNDLE_VERSION` wire format version |
| `MAX_SECRET_FILE_BYTES` | `4_096` (4 KiB) | Cap on `--blackwall-token-file` read via `SecretFile::load` |
| `MAX_REPLY_BODY_BYTES` | `8_192` (8 KiB) | Cap on serialized reply JSON in `build_ok_reply` |

1. **`reflexd` `/v1/classify` wire alignment:** `ClassifyBody` sets `agent`, kebab-case `verb`
   (`read`, `draft`, `schedule`, `file-internal`, or `send-external`), `params: { "target": <tool_name> }`,
   and unprefixed 64-hex `payload_sha256` (`SHA-256` of `call.arguments` JSON), sending zero
   unknown fields so `reflexd`'s `#[serde(deny_unknown_fields)]` parser accepts every request.
2. **Byte-exact `blake3` `RunRecord` hash:** `CustodyRunRecord` serializes fields in the exact
   declaration order of `blackwall_store::RunRecord` (`prompt`, `model`, `stdout`, `stderr`,
   `exit_code`, `project`), omitting default `None`/`false`/empty fields so
   `serde_json::to_string_pretty` and `blake3::hash` match `ObjectStore::import_bundle`
   byte-for-byte.
3. **No custody drop on transient failures:** HTTP `429`, `5xx`, timeouts, and transport errors
   from `blackwall-serve` map to `TurnError::Transient`, causing `PoolWorker` to `nack` for
   redelivery without acknowledging an uncustodied turn; `4xx`, invalid JSON, or `run_hash`
   mismatches map to `TurnError::Permanent`.

## 4. Requirements (EARS)

- **REQ-LG-REFLEX-01:** When `--reflex-classify-url` (`LG_REFLEX_CLASSIFY_URL`) is supplied to `lg serve` or `lg pool`, the process shall validate a clean `http://` or `https://` URL via `parse_clean_base_url` at startup (normalizing bare `/` paths to `/v1/classify`) and refuse malformed, credential-bearing, query-bearing, or non-HTTP(S) URLs before serving or polling.
  Proof: unit tests in `littleguy::reflex_gate` and CLI startup test `cli_rejects_bad_reflex_url_and_mismatched_or_insecure_blackwall_flags`.
- **REQ-LG-REFLEX-02:** When `ReflexToolPolicy` evaluates a `ToolCall`, it shall `POST` `{ "agent", "verb", "params": { "target" }, "payload_sha256" }` to the classify URL with a `5 s` timeout and delegate `GREEN` or `YELLOW` classifications to the inner `ToolPolicy` (or `ToolDecision::Allow` when none is set).
  Proof: tests `reflex_policy_allows_green_and_yellow_and_delegates_to_inner`, `reflex_classify_wire_matches_ves_domain_contract`.
- **REQ-LG-REFLEX-03:** When `ReflexToolPolicy` receives a `RED` classification, it shall record the call in `SqliteStores` (when configured) and return `ToolDecision::RequireApproval` if `allow_approval` is `true` (`lg serve`), or return `ToolDecision::Deny` if `allow_approval` is `false` (`lg pool`).
  Proof: tests `reflex_policy_red_requires_approval_in_serve_and_denies_in_pool`.
- **REQ-LG-REFLEX-04:** If `ReflexToolPolicy` receives `NEVER`, an unknown `tier`, a non-2xx HTTP response, a `5 s` timeout, a transport failure, or malformed JSON, then it shall fail closed with `ToolDecision::Deny`.
  Proof: tests `reflex_policy_fails_closed_on_never_unknown_http_error_and_timeout`.
- **REQ-LG-CUSTODY-01:** When `--blackwall-url` or `--blackwall-token-file` is passed to `lg pool`, startup shall require both flags together, validate `--blackwall-url` via `parse_clean_base_url`, load `--blackwall-token-file` via `SecretFile::load` (`O_NOFOLLOW`, regular file, `0600`, `geteuid()`, `<= 4 KiB`, redacted `Debug`), and validate `--blackwall-project` (and any delivery `"project"` override) against 1- or 2-segment `[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)?` rejecting `.` and `..`.
  Proof: tests `blackwall_project_validates_one_and_two_segment_slugs`, `cli_rejects_bad_reflex_url_and_mismatched_or_insecure_blackwall_flags`, `pool_delivery_project_override_routes_two_segments_and_invalid_project_terms`.
- **REQ-LG-CUSTODY-02:** When `BlackwallCustodyClient` imports a completed turn, it shall serialize `CustodyRunRecord` with `serde_json::to_string_pretty`, compute its 64-hex `blake3` `run_hash`, build a version `1` `CustodyBundle` (`parent_world = "littleguy-pool"`, `changeset = ""`, `settle_state = { "status": "unsettled" }`, `blobs = {}`), and `POST` to `{blackwall_url}/v1/projects/{project}/runs/import` with `Authorization: Bearer <token>` and a `10 s` timeout.
  Proof: tests `custody_bundle_blake3_matches_pretty_run_record_bytes_and_two_segment_url`.
- **REQ-LG-CUSTODY-03:** When `blackwall-serve` responds to `runs/import`, `BlackwallCustodyClient` shall require a `2xx` status and `response.run_hash == local_run_hash`, mapping `429`, `5xx`, timeouts, and transport failures to `TurnError::Transient` (`StepOutcome::Nacked`) and `4xx`, malformed JSON, or `run_hash` mismatches to `TurnError::Permanent` (`StepOutcome::Settled` with `outcome: "failed"`).
  Proof: tests `pool_custody_503_nacks_and_redelivery_settles_without_second_model_call`, `pool_custody_400_and_hash_mismatch_settle_failed_with_exit_err`.
- **REQ-LG-CUSTODY-04:** When `PoolWorker` builds task replies, `build_ok_reply` shall include `"exit": "ok"` in `body` (plus `"run_hash"`, `"custody": "blackwall-serve"` in `body` and `"run_hash"` in `refs` when `out.run_hash` is present) while preserving `"exit"`, `"run_hash"`, `"custody"`, `"shell_id"`, and `"transcript_sha256"` under `8_192`-byte answer truncation, and `build_failed_reply` shall include `"exit": "err"` in `body`.
  Proof: tests `pool_reply_truncates_oversized_answer_preserving_custody_and_exit_metadata`, `pool_custody_400_and_hash_mismatch_settle_failed_with_exit_err`.
