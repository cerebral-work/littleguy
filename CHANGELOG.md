# Changelog

All notable changes to `cerebral-work/littleguy` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Security

- **Camel & `zip` Dependency Upgrade (`EST-1604`, `littleguy`):** Upgraded `camel-*` from `0.57` to `0.59` (with an unmodified `harvest-camel 0.4.0` `[patch.crates-io]` compatibility shim on `camel-* = "0.59"` until upstream publishes `harvest-camel > 0.4.0`), moving `Cargo.lock` from `zip 8.6.0` to `zip >= 9.0.5` and closing all 5 `zip` Dependabot alerts (`GHSA-m665-v7pm-7grj`, `GHSA-9hj8-73fh-9963`, `GHSA-hjqx-j4rw-g7xv`, `GHSA-q7v9-x93c-xqv3`, `GHSA-8hxf-qr8p-62x4`).

### Fixed

- **Littleman Pool Consumer (`littleguy`, `specs/0005`):** Accept `0`-based explicit `refs.attempt` (`0` and `"0"`) in `lg pool` so first-send deliveries from `escapement-serve` (`escapement-serve/src/pool.rs:596`, `:1193`) run, cache by `(task_id, 0)`, and reply with `refs.attempt = 0` while still refusing non-integers, negatives, signed strings, and values above `u32::MAX` (absent or `null` `refs.attempt` continues to fall back to `delivery_attempt.max(1)`).

### Added

- **Non-Root Container Image, Harbor Workflow, and `--api-key-file` (`littleguy`, `xtask`, `specs/0006`):**
  - Added `--api-key-file` (`LG_API_KEY_FILE`) on `ServeArgs` (shared by `lg serve` and `lg pool`) backed by `littleguy::secret_file::SecretFile` (`O_NOFOLLOW | O_NONBLOCK`, regular file, mode `0600`, `geteuid()`, `<= 4 KiB`, graphic ASCII, redacted `Debug` and error formatting) so hosted model API keys (`claude`, `openai`, `openrouter`) load from mounted secret files without entering Pod `env:` blocks, `argv`, or wrapper scripts.
  - Added multi-stage `Dockerfile` and `.dockerignore` (`rust:1.99.1-slim-trixie` builder + `debian:trixie-slim` runtime pinned by `sha256` digest, HTTPS apt mirrors, `-j 2` Cargo throttling, `ca-certificates` + `tini`, writable `0700` `/data`, and `USER 65532:65532`).
  - Added `cargo xtask container-check` (wired into `cargo xtask ci`), a PR `container` smoke job in `.github/workflows/ci.yml` (`--read-only --cap-drop=ALL --security-opt=no-new-privileges`), and `.github/workflows/build-image.yml` pushing Trivy-scanned, Cosign-signed, CycloneDX-attested digests to `harbor.unsigned.gg/platform/littleguy` while skipping `publish` cleanly when repository secrets are absent.
- **Littleman Pool Consumer (`littleguy`, `littleguy-core`, `specs/0005`):**
  - Added `lg pool` (`--cortex-url`, `--token-file`, `--peer-id`, `--allowed-sender`, `--wait-ms`, `--shell-id`) with `O_NOFOLLOW | O_NONBLOCK`, `0600` regular-file effective-UID (`geteuid`), `4 KiB`-capped `PoolToken` loading and redacted `Debug` and error formatting.
  - Added `PoolClient`, `PoolWorker`, `TurnRunner`, and `RuntimeTurnRunner` (`LittleguySeatSubstrate` plus single-writer `SqliteRuntime`), enforcing sender and payload caps (`12 KiB` body, `16 KiB` `refs`, `8 KiB` reply, `1024`-char `refs.error`), `GhostBinding` validation with `"ghost_binding": null` absent parity, background lease `extend`, bounded `(TaskId, attempt)` replay cache retained until `term` confirmation, `escapement-serve` reply settlement (`ack`, `nack`, `term`), `250 ms` empty-`recv` spin floor, `recv` non-array decode backoff, and worker survival across poison deliveries and per-delivery `401`/`403`/`410`/`4xx` settle or send responses.
  - Added boundary id newtypes `TaskId`, `ShellId`, `RoleName`, `PoolPeerId` (`8-4-4-4-12` UUID validated), and `DeliveryToken` in `littleguy-core::ids`.
- **GhostBinding Validator (`littleguy-core`, `specs/0004`):**
  - Added the zero-I/O `GhostBinding` type, `GhostBindingWire`, `GhostManifestation`, `GhostLifecycleState` and `validate`, which refuses a binding with a typed `GhostBindingError`. Deserializing a `GhostBinding` runs the validator and refuses unknown fields.
  - Enforced the cortex REQ-0125-03/-04 rules: a persistent manifestation needs a `spiffe://unsigned.gg/agent/TEAM/SLUG` seat, and `Crew` holds no seat and never enters `dormant` or `dreaming`.
  - Enforced the caps: `ghost_id` 64 bytes, `mitc_principal` 128, `spiffe_seat` 256, seat segment 63.
  - Added three fail-closed rules that are stricter than cortex: `ghost_id` is `[a-z0-9-]`, `mitc_principal` refuses control characters and surrounding whitespace, and seat segments refuse `_`. A shape test pins the serialized names to cortex.
  - Added an optional `ghost_binding` field on `SeatReceipt`, omitted when absent.
  - Added `BiscuitMandateClaims` tests for the agent and elf SPIFFE forms, with no `mandate_bridge` code change.
- **Wave 2 Durable Reverie & XERJ Store Adapters (`littleguy-stores`, `specs/0003`):**
  - Added pluggable `HttpTransport` trait with `ReqwestHttpTransport` (`reqwest 0.13` + `rustls` ring) for production and deterministic offline `MockHttpTransport` for tests.
  - Added `ReverieClientConfig`, `ReverieObservationBuilder`, and `ReverieObservationStore` implementing `autumn_harvest_agent` `MemoryStore` and `EpisodeStore` with zero-interpolation `/search?q=...`, `/context/smart?project=...`, and `/observations` requests (`topic_key` + `project` + taint-preserving flow labels).
  - Added `XerjClientConfig`, `XerjHybridQueryBuilder`, and `XerjHybridSearchStore` implementing `MemoryStore` and `EpisodeStore`, Cygnus `:9200` BM25 + `semantic` hybrid search (`rrf` / `linear` fusion), canonical SHA-256 corpus content hashing, and `_meta.commit_sha` watermark tracking.
- **Wave 2 Adversarial `FlowRules::rule_of_two()` Gate & Falsification Suite (`littleguy-core`, `specs/0003`):**
  - Added `RuleOfTwoGate`, `RuleOfTwoLeg`, `SignetPresenceReceipt`, `SignetVerifierKey`, and `verify_rule_of_two_guard` bridging `autumn-harvest` `FlowRules::rule_of_two()` and `mandate_bridge`.
  - Enforced fail-closed `RuleOfTwoViolation` on `[UntrustedInput, SensitiveCapability, ExternalEgress]` lethal-trifecta tool calls unless accompanied by a fresh (`<= 60s`), un-replayed, argument-bound human Signet receipt with a valid 32-byte keyed BLAKE3 MAC, covered by self-falsifying adversarial tests and negative-control bypass detection.
- **Wave 1 Orchestration & Mandate Foundation (`littleguy-core`, `specs/0001`, `specs/0002`):**
  - Added 4-pillar whole-estate orchestration kernel (`WheelhorseGate`, `TripwireRegistry`, `AuthorityGrant`, `LittleguySeatSubstrate`, `EscapementPoolDispatcher`).
  - Added `spiffe://unsigned.gg` single-trust-domain `SpiffeAgentIdentity`, 5-tier `SandboxClass`, 5 `ConstitutionalSealedAction` gates, and `BiscuitMandateClaims` compilation to `autumn-harvest 0.10.0` `Mandate` and `FlowRules`.
