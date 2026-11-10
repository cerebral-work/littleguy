---
status: in-progress
created: 2026-11-10
tags: [littleguy, wheelhorse, autumn-web, autumn-harvest, escapement, cortex, orchestration, reversibility, aeo]
priority: high
depends_on: [0001-substrate-seat-and-biscuit-mandate-integration]
---

# 0002 — 4-Pillar Whole-Estate Orchestration: Wheelhorse, Autumn-Web, Littleguy, and Escapement

## 1. Overview & Motivation

Whole-estate orchestration across `ceres` and the Cygnus Kubernetes cluster unites the 9 Cortex Subject Cells with four execution and governance pillars:
1. **Pillar 1 — Wheelhorse Reversibility & Canary Tripwires (`madmax983` / `wheelhorsedev/nexus` & `vespertin/specs/0135`):** Pure 4-stage `classify -> capture_plan -> validate -> decide` mutation gate with `CompensationPlan`, `diff_blake3` + `head` SHA pinning, static capability + reversibility evaluation (**no streak-based earned autonomy**), the Soma Gate invariant (`RedGated` and `Irreversible` mutations always require explicit operator approval), and Canary Tripwire membranes (`TripwireRegistry`).
2. **Pillar 2 — Autumn-Web Agent Authority & AEO/WebMCP Surface (`autumn-foundation/autumn`):** Declarative `AuthorityGrant` envelopes over ordered `EffectKind`s (`Write`, `UnboundedWrite`, `CrossTenant`, `Outbound`, `Webhook`, `Job`), RFC 7231 `Accept: text/markdown` content negotiation, `/llms.txt`, and streamable `/mcp` tool descriptors.
3. **Pillar 3 — Littleguy Durable Seat Substrate (`cerebral-work/littleguy` on `autumn-harvest 0.10.0`):** Event-sourced actor seats (`<subject>/<slot>`) executing under `spiffe://unsigned.gg` SVIDs, `oath-core` Ed25519 Biscuit mandates (`Mandate::from_biscuit_and_svid`), Meta's Rule of Two (`FlowRules::rule_of_two()`), `-j 2` cargo build throttling, and the 5-tier `SandboxClass` isolation hierarchy.
4. **Pillar 4 — Escapement Typed Pool Runtime (`vespertin/escapement`: `specs/0136` & `specs/uas/0027`):** `EscapementPoolDispatcher` claiming typed `EscapementTaskSpec` units (`BuildAndTest`, `FormalVerify`, `DocCheck`, `CustomCommand`) via a two-phase lease loop (`Queued -> ClaimPendingAck -> ClaimConfirmed -> Running -> Succeeded/Failed/Cancelled` with fenced `lease_epoch`) and driving seated workers through `LittleguySeatSubstrate`.

---

## 2. Normative Requirements (EARS)

### 2.1 Subject Topology & Domain Gate Routing (`REQ-ORCH-TOPO-*`)

- **REQ-ORCH-TOPO-01:** THE `SubjectTopologyRegistry` SHALL map all 9 canonical estate subjects (`delivery`, `cluster-gitops`, `identity-supply-chain`, `agent-fabric`, `comms`, `memory-knowledge`, `runtime-kernel`, `assurance-research`, `product-games`) to their canonical callsign, `MeshDomainKind` (`Platform`, `Security`, `Development`, `Research`, `Product`), `DomainGatePolicy` (`RedGated` vs `LandOnGreen`), minimum `SandboxClass` floor, and default pool queue.
- **REQ-ORCH-TOPO-02:** WHEN a repository slug (`<owner>/<name>`) sits in `Platform` or `Security` (`unsigned-gg/unsigned-paas`, `cerebral-work/vilicus`, `cerebral-work/revenant`, `unsigned-gg/autoresearcher`), THE domain gate resolver SHALL classify it as `DomainGatePolicy::RedGated` regardless of which subject lane initiated the task (`vespertin/specs/0033` & `C1c` ruling).

### 2.2 Pillar 1 — Wheelhorse Reversibility & Canary Tripwire Gate (`REQ-ORCH-WH-*`)

- **REQ-ORCH-WH-01:** WHEN an agent seat or workflow proposes an external action (`WheelhorseActionDescriptor`), THE `WheelhorseGate` SHALL execute the pure 4-stage pipeline (`classify -> capture_plan -> validate -> decide`) without streak-based earned-autonomy counters and produce an immutable `WheelhorseGateRecord` containing `decision`, `reason`, `actor`, `capability`, `domain_gate`, `reversibility_class`, `head_sha`, `diff_blake3`, and `evaluated_at_ms`.
- **REQ-ORCH-WH-02:** WHEN an action touches any path or entity registered in `TripwireRegistry` (canary honeypot records or forbidden paths such as `.ssh/`, `.gpg/`, `.secrets`, `login.keyring`), THE `WheelhorseGate` SHALL immediately return `WheelhorseGateDecision::BlockedByTripwire`.
- **REQ-ORCH-WH-03 (Soma Gate Invariant):** WHEN an action targets a `DomainGatePolicy::RedGated` repository/domain OR classifies as `ReversibilityClass::Irreversible` (one-way door), THE `WheelhorseGate` SHALL unconditionally return `WheelhorseGateDecision::RouteToApproval` and SHALL NEVER auto-commit.
- **REQ-ORCH-WH-04:** WHEN an action classifies as `ReversibilityClass::Compensatable` in a `DomainGatePolicy::LandOnGreen` domain, THE `WheelhorseGate` SHALL return `AutoCommitWithNotice` ONLY IF a `CompensationPlan` with a valid 40-hex `head_sha` and 64-hex `diff_blake3` is attached and `diff_bytes > 0`; otherwise it SHALL return `BlockedByMissingCompensation`.

### 2.3 Pillar 2 — Autumn-Web `AuthorityGrant`, AEO Markdown & WebMCP (`REQ-ORCH-AUTUMN-*`)

- **REQ-ORCH-AUTUMN-01:** THE `AuthorityGrant` envelope SHALL order `EffectKind` as `Write < UnboundedWrite < CrossTenant < Outbound < Webhook < Job` and SHALL refuse any handler or tool invocation whose proved/declared effect, target repository, rate (`calls_per_minute`), or reversibility class exceeds the grant.
- **REQ-ORCH-AUTUMN-02:** THE orchestration engine SHALL render an RFC 7231 `Accept: text/markdown` AEO status document (`render_aeo_markdown`) and a deterministic `WebMcpManifest` (`/mcp` tool + resource catalog) summarizing all active subject cells, seat slots, Wheelhorse gate records, and Escapement pool tasks.

### 2.4 Pillar 3 — Littleguy `SeatSubstrate` (`REQ-ORCH-SEAT-*`)

- **REQ-ORCH-SEAT-01:** THE `LittleguySeatSubstrate` SHALL implement `ensure_seat`, `liveness`, `submit`, `poll`, `cancel`, and `release` matching `vespertin/specs/uas/0027`:
  - `submit` SHALL be idempotent on `job_id` (`[A-Za-z0-9._-]{1,128}` used directly as queue item id; other strings hashed as `qitem-uas-<24 hex of sha256(job_id)>`).
  - `submit` SHALL verify that the seat's `SandboxClass` satisfies both the subject floor and the Biscuit mandate floor (`REQ-LG-SANDBOX-01`), that `target_repo` is permitted by the Biscuit mandate and `AuthorityGrant`, and that the `WheelhorseGate` permits execution.
  - Every cargo build/test command synthesized for a seat SHALL enforce `-j 2` host throttling (`REQ-LG-SANDBOX-02`).

### 2.5 Pillar 4 — Escapement Typed Pool Dispatcher & Two-Phase Lease Loop (`REQ-ORCH-ESC-*`)

- **REQ-ORCH-ESC-01:** THE `EscapementPoolDispatcher` SHALL accept typed `EscapementTaskSpec` records (`BuildAndTest`, `FormalVerify`, `DocCheck`, `CustomCommand`) scoped to `EscapementPoolClass` (`Build`, `Verify`, `Gpu`) and `EstateSubject`.
- **REQ-ORCH-ESC-02 (Two-Phase Lease Claim-and-Ack):** WHEN a runner calls `claim_next`, THE dispatcher SHALL transition the next matching `Queued` task to `EscapementLeaseState::ClaimPendingAck` with a monotonically incremented `lease_epoch`. THE dispatcher SHALL refuse to execute (`dispatch_claimed`) any task that has not first transitioned to `EscapementLeaseState::ClaimConfirmed` with matching `runner_id` and `lease_epoch`.
- **REQ-ORCH-ESC-03:** WHEN a runner fails to confirm a `ClaimPendingAck` lease before timeout, `requeue_unconfirmed` SHALL return the task to `EscapementLeaseState::Queued` with zero execution side effects.
