# Changelog

All notable changes to `cerebral-work/littleguy` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
