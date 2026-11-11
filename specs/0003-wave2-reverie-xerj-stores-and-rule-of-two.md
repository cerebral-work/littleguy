---
status: in-progress
created: 2026-11-11
tags: [littleguy, reverie, xerj, autumn-harvest, stores, flow-rules, rule-of-two, signet, security]
priority: high
depends_on:
  - 0001-substrate-seat-and-biscuit-mandate-integration
  - 0002-whole-estate-wheelhorse-autumn-littleguy-orchestration
---

# 0003 — Wave 2: Reverie + XERJ Durable Stores and Adversarial `FlowRules::rule_of_two()` Gate

## 1. Overview & Motivation

Wave 1 (`specs/0001` and `specs/0002`) aligned `cerebral-work/littleguy` on `autumn-harvest 0.10.0`, established the single estate trust domain (`spiffe://unsigned.gg`), compiled `oath-core` Biscuit delegation claims into `autumn_harvest::Mandate` and `FlowRules`, and delivered the 4-pillar `Wheelhorse` / `Autumn-Web` / `LittleguySeatSubstrate` / `Escapement` orchestration kernel.

Wave 2 connects `littleguy` seats to the estate's durable memory and retrieval planes while closing the pre-tool lethal-trifecta gap:

1. **Reverie Durable Observation & Smart-Context Adapter (`crates/littleguy-stores::reverie`):** Bridges `autumn_harvest_agent::memory::MemoryStore` and `autumn_harvest_agent::episode::EpisodeStore` to the Reverie daemon (`:7437`) via typed `/search?q=...`, `/context/smart?project=...`, and `/observations` requests (`topic_key` + `project` + taint-preserving flow labels), backed by local SQLite `EditKey` idempotency.
2. **XERJ Hybrid Search & `_meta.commit_sha` Watermark Adapter (`crates/littleguy-stores::xerj`):** Bridges `MemoryStore` / `EpisodeStore` and corpus retrieval to the Cygnus `xerj` cluster service (`http://xerj.tail769bd2.ts.net:9200`, `XERJ_EMBED_MODE=proxy`) with a zero-interpolation BM25 + `semantic` hybrid query builder (`rrf` / `linear` fusion), deterministic SHA-256 corpus content hashing, and `_meta.commit_sha` index watermark tracking.
3. **Pluggable HTTP Transport (`crates/littleguy-stores::transport`):** Isolates wire I/O behind `HttpTransport` (`ReqwestHttpTransport` on `reqwest 0.13` + `rustls` ring in production; `MockHttpTransport` in unit tests) so every store test runs 100% deterministically offline with zero live network dependency.
4. **Pre-Tool `RuleOfTwoGate` & Adversarial Falsification Suite (`crates/littleguy-core::rule_of_two`):** Bridges `autumn_harvest_agent::FlowRules::rule_of_two()` and `BiscuitMandateClaims` so any tool call completing `[UntrustedInput, SensitiveCapability, ExternalEgress]` fails closed (`Err(RuleOfTwoViolation)`) unless accompanied by a fresh (`age <= 60s`), un-replayed, argument-bound `SignetPresenceReceipt` carrying a valid 32-byte keyed BLAKE3 MAC from a non-agent human principal.

---

## 2. Normative Requirements (EARS)

### 2.1 Pluggable Transport & Zero-Interpolation Wire Builders (`REQ-LG-STORE-01..02`)

- **REQ-LG-STORE-01:** THE `littleguy-stores` crate SHALL define a `Send + Sync` `HttpTransport` trait returning `BoxFuture` over typed `StoreHttpRequest` and `StoreHttpResponse` values, providing `ReqwestHttpTransport` for production and deterministic `MockHttpTransport` for offline unit tests.
- **REQ-LG-STORE-02:** WHEN `ReverieObservationStore` or `XerjHybridSearchStore` constructs an HTTP URL or JSON request body, THE builder SHALL use `url::Url` path-segment and query-pair encoders and typed `serde` serialization with zero ad-hoc string interpolation.

### 2.2 Reverie Durable `MemoryStore` / `EpisodeStore` Adapter (`REQ-LG-REVERIE-01..04`)

- **REQ-LG-REVERIE-01:** THE `ReverieClientConfig` SHALL validate its base URL (`http` or `https`, no embedded credentials), non-empty `default_project`, `ReverieScope` (`project` | `personal`), and bounded `smart_context_limit` (`1..=50`).
- **REQ-LG-REVERIE-02:** THE `ReverieObservationBuilder` SHALL require non-empty `title`, `content`, `project`, and normalized hierarchical `topic_key` (`<stem>/<slug>`, rejecting empty segments, `..`, leading/trailing `/`, whitespace, or control characters).
- **REQ-LG-REVERIE-03:** WHEN `ReverieObservationStore` executes `MemoryStore::apply`, `MemoryStore::apply_labelled`, `EpisodeStore::append`, or `EpisodeStore::mark`, THE store SHALL enforce `EditKey` idempotency so retried keys return the cached outcome without emitting duplicate HTTP writes, and SHALL propagate entry flow labels as `FlowLabel` tags so untrusted taint survives round-trips.
- **REQ-LG-REVERIE-04:** THE `ReverieObservationStore` SHALL expose typed `search` (`GET /search?q=...`) and `smart_context` (`GET /context/smart?project=...`) methods over `HttpTransport`.

### 2.3 XERJ Hybrid Search & `_meta.commit_sha` Watermarks (`REQ-LG-XERJ-01..04`)

- **REQ-LG-XERJ-01:** THE `XerjClientConfig` SHALL default to the Cygnus cluster endpoint `http://xerj.tail769bd2.ts.net:9200` with `XerjEmbedMode::Proxy` and SHALL reject loopback `:9200` endpoints unless `allow_loopback_for_tests` is explicitly enabled, preventing local neural-embedder RSS exhaustion on shared hosts (`ceres`).
- **REQ-LG-XERJ-02:** THE `XerjHybridQueryBuilder` SHALL construct `POST /{index}/_search` requests fusing a BM25 `match` clause and a server-side `semantic` clause under `rrf` or `linear` fusion, enforcing validated index identifiers and `max_tokens >= 32`.
- **REQ-LG-XERJ-03:** THE `XerjHybridSearchStore` SHALL track index synchronization state via `CommitWatermark` (`commit_sha` + canonical SHA-256 `content_hash`), comparing target digests against both local SQLite watermarks and remote `GET /{index}/_mapping` `_meta.commit_sha` / `_meta.content_hash` fields before issuing `PUT /{index}/_mapping` updates.
- **REQ-LG-XERJ-04:** THE `XerjHybridSearchStore` SHALL implement `MemoryStore` and `EpisodeStore` backed by local SQLite `EditKey` durability and XERJ `/_memory/{namespace}` write-through / `/_memory/{namespace}/_recall` hybrid recall.

### 2.4 Pre-Tool `RuleOfTwoGate` & Signet Hardware-Presence Elevation (`REQ-LG-ROT-01..05`)

- **REQ-LG-ROT-01:** THE `RuleOfTwoGate` SHALL bridge `BiscuitMandateClaims::from_biscuit_and_svid` and `autumn_harvest_agent::FlowRules::rule_of_two()`, monotonically accumulating exercised taint legs (`UntrustedInput`, `SensitiveCapability`, `ExternalEgress`) across tool calls and untrusted tool outputs within a session.
- **REQ-LG-ROT-02:** WHEN a tool invocation exercises at most 2 of the 3 legs within a valid, un-expired Biscuit `Mandate` and does not violate `ConstitutionalSealedAction` or `max_reversibility`, THE `RuleOfTwoGate` SHALL allow execution (`Ok(RuleOfTwoOutcome::Allowed)`).
- **REQ-LG-ROT-03:** WHEN a tool invocation (either directly or combined with prior session taint such as prompt-injected untrusted tool output) completes all 3 legs `[UntrustedInput, SensitiveCapability, ExternalEgress]`, THE `RuleOfTwoGate` SHALL fail closed with `Err(RuleOfTwoViolation::TrifectaBlocked)` unless accompanied by a valid `SignetPresenceReceipt`.
- **REQ-LG-ROT-04:** THE `RuleOfTwoGate` SHALL reject any `SignetPresenceReceipt` that:
  1. Was issued more than `60_000` ms (`60s`) before `now_ms` (`ReceiptExpired`) or lies in the future (`ReceiptFromFuture`),
  2. Names an agent or workload principal (`spiffe://...`, `agent:*`, `elf:*`, `service:*`, `sa:*`, `worker:*`, `bot:*`, or the seat's holder name) instead of a human principal (`AgentPrincipalForbidden`),
  3. Fails constant-time 32-byte keyed BLAKE3 MAC verification over `(spiffe_id, principal, tool_name, arguments_digest, issued_at_ms, nonce)` (`InvalidReceiptMac`),
  4. Mismatches the target `tool_name` or canonical `arguments_digest` (`ReceiptScopeMismatch`), or
  5. Reuses a previously consumed 16-byte `nonce` (`ReceiptReplayDetected`).
- **REQ-LG-ROT-05:** WHEN a valid, fresh human `SignetPresenceReceipt` accompanies a 3-of-3 invocation (and the action is not a `ConstitutionalSealedAction`), THE `RuleOfTwoGate` SHALL consume the receipt nonce, record an immutable `SignetElevationRecord`, upgrade session taint cleanly, and return `Ok(RuleOfTwoOutcome::AllowedWithSignetElevation)`. A self-falsifying `verify_rule_of_two_guard` harness SHALL prove that simulated guard bypasses are caught deterministically.
