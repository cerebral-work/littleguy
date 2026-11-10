---
status: planned
created: 2026-11-10
tags: [littleguy, autumn-harvest, escapement, substrate, signet, oath, biscuit, sandbox, xerj]
priority: high
---

# 0001 — Substrate Seat, 5-Tier SandboxClass, and Biscuit Mandate Integration

## 1. Overview & Motivation

`cerebral-work/littleguy` (forked from `autumn-foundation/littleguy` `v0.1.0`) is the estate's durable, self-scheduling autonomous coding and operations agent runtime built on `autumn-harvest`.

To run as a first-class citizen inside the `unsigned-gg` / `cerebral-work` agentic substrate (`escapement` / `uas`, `blackwall`, `cortex` mesh, `signet`, and `oath`), `littleguy` must:
1. Upgrade its workflow dependency from `autumn-harvest 0.4` to **`autumn-harvest 0.10.0`** (`rusqlite 0.41` bundled) to match `cerebral-work/vespertin` (`EST-1500`).
2. Implement the **`SeatSubstrate` (`SeatBackend`)** contract (`vespertin/specs/uas/0027`) so `escapement-runner` can claim, wake, heartbeat, and reap `littleguy` seats across the 5-tier **`SandboxClass`** isolation matrix (`Process` $\to$ `Gvisor` $\to$ `Kata` $\to$ `Firecracker` $\to$ `Ouroboros`).
3. Compile **`oath-core` Ed25519 Biscuit (`biscuit-auth`)** delegation tokens and **`spiffe://unsigned.gg`** workload SVIDs directly into `autumn-harvest::Mandate` and `FlowRules` (`REQ-LG-MANDATE-01..05`).
4. Enforce the **Second Sovereign Root**, **4-hour renewal cooldown**, and **5 Constitutional Sealed Actions** at the tool-call and subagent-delegation boundary.

---

## 2. Normative Requirements (EARS)

### 2.1 `autumn-harvest 0.10.0` & Durable SQLite WAL Alignment (`REQ-LG-HARVEST-*`)

- **REQ-LG-HARVEST-01:** WHEN `littleguy` is compiled in the estate workspace, THE system SHALL depend on `autumn-harvest = "0.10.0"` and `rusqlite = { version = "0.41", features = ["bundled"] }` with zero `0.4` duplicate versions in `Cargo.lock`.
- **REQ-LG-HARVEST-02:** WHEN a `littleguy` workflow executes an external side effect (shell command, file write, git operation, MCP call, or subagent spawn), THE `WorkflowContext` SHALL journal the step in the durable SQLite WAL with an explicit `ActionKind`, `ReversibilityClass` (`Pure`, `Idempotent`, `Compensatable`, `Irreversible`), and `CompensationPlan` before executing I/O.
- **REQ-LG-HARVEST-03:** WHEN a `littleguy` seat is reaped, migrated, or restarted after node preemption, THE runtime SHALL replay the SQLite WAL journal to resume from the last committed activity watermark without re-executing completed non-idempotent steps.

### 2.2 Single Trust Domain (`spiffe://unsigned.gg`) & Biscuit-to-Mandate Compilation (`REQ-LG-MANDATE-*`)

- **REQ-LG-MANDATE-01:** WHEN a `littleguy` seat starts, THE runtime SHALL require a valid X.509/JWT-SVID under the single estate trust domain `spiffe://unsigned.gg` matching one of the canonical agent path shapes (`/agent/<team>/<agent_id>`, `/conclave/<mesh>/elf/<elf_id>`, `/actor/<atespace>/<actor>`, or `/host/<host>/lane/<role>`) and SHALL reject split-horizon trust domains (`cerebral.internal`, `uas.unsigned.gg`, `substrate-actor.local`).
- **REQ-LG-MANDATE-02:** WHEN `littleguy` receives an `oath-core` Ed25519 `Biscuit` token from `escapement-runner` or a parent coordinator, THE runtime SHALL verify the token offline against the `Oath` root public key and compile its Datalog facts/checks into an `autumn_harvest::Mandate` (`Mandate::from_biscuit_and_svid`).
- **REQ-LG-MANDATE-03:** WHEN `littleguy` spawns a child subagent or delegates a tool invocation, THE runtime SHALL append an offline Biscuit attenuation block restricting `sandbox_class`, `allowed_repo_prefixes`, `max_ActionKind`, and `expires_at_ms` before passing the attenuated token to the child.
- **REQ-LG-MANDATE-04:** WHEN `littleguy` requires an external API credential or secret, THE runtime SHALL exchange its `spiffe://unsigned.gg` JWT-SVID with OpenBao or the `reflex` Egress Credential Broker (`san-mateo` for OAuth) and SHALL NEVER read per-agent 1Password service-account tokens (`REQ-0012-CRED-04`).
- **REQ-LG-MANDATE-05:** WHEN a tool call targets any of the **5 Constitutional Sealed Actions** (`ModifySovereignRootQuorum`, `RotateSpireOrOpenBaoRootCa`, `DisableSignetAuditLedger`, `ShredActivePrincipalPiiKey`, `GrantUnboundedSuperadmin`), THE `littleguy` `Mandate` gate SHALL unconditionally refuse execution (`ApprovalGate::Never`) and emit a security violation receipt to `signet-store`.

### 2.3 `SeatSubstrate` & 5-Tier `SandboxClass` Isolation (`REQ-LG-SANDBOX-*`)

| Tier | `SandboxClass` Variant | Runtime / Hypervisor | Target Workloads |
|---|---|---|---|
| **Tier 0** | `Process` | `blackwall` (`sbx-core` Landlock v3 + seccomp-BPF + cgroup v2 + DeltaDB CoW) | Trusted internal repo builders (`-j 2` cargo throttled) |
| **Tier 1** | `Gvisor` | `runsc` (`gVisor` user-space application kernel) | Networked MCP / web-scraping / external API agents |
| **Tier 2** | `Kata` | `kata-runtime` (QEMU/Cloud-Hypervisor lightweight hardware VM) | Multi-tenant container builds & untrusted C/C++/CUDA compilation |
| **Tier 3** | `Firecracker` | `firecracker` / `jailer` microVM (`<125ms` boot, virtio-vsock) | Ephemeral `escapement` `Elf` pool workers & customer-scoped seats |
| **Tier 4** | `Ouroboros` | `unsigned-gg/ouroborOS` (`pgray` capability microkernel + KV page transfer) + `hermit` `detcore` | High-assurance deterministic replay & zero-copy KV-cache handoff between seats |

- **REQ-LG-SANDBOX-01:** WHEN `escapement-runner` claims a `PoolWork` item (`vespertin/specs/0142`), THE `LittleguySeatBackend` (`SeatSubstrate`) SHALL launch the `littleguy` worker inside the `SandboxClass` required by the item's sensitivity and `Biscuit` `check if sandbox_class in [...]` predicate.
- **REQ-LG-SANDBOX-02:** WHEN `littleguy` executes any `cargo` build or test activity on shared hosts (`ceres` or `pool=build`), THE tool runner SHALL inject `-j 2` (or run through `dagrun` under a 2-core resource box) to prevent host memory exhaustion.
- **REQ-LG-SANDBOX-03:** WHEN a `littleguy` run completes a step or requests an operator/Two-Root gate, THE runtime SHALL emit both a live `dagr` JSON projection (`dagr-producer`) and a `Signet` hash-chained audit entry covered by the daily Ed25519 + ML-DSA-65 hybrid checkpoint (`REQ-0011-HIST-05`).

---

## 3. Implementation Phases

1. **Phase 1 (Crate & Dependency Alignment):** Bump `Cargo.toml` to `autumn-harvest 0.10.0` + `rusqlite 0.41`, add `biscuit-auth`, and implement `src/mandate_bridge.rs` (`Mandate::from_biscuit_and_svid`).
2. **Phase 2 (`SeatSubstrate` & 5-Tier `SandboxClass`):** Implement `LittleguySeatBackend` for `vespertin/escapement-runner` (`specs/uas/0027`) supporting `Process`, `Gvisor`, `Kata`, `Firecracker`, and `Ouroboros`.
3. **Phase 3 (Durable Queue, XERJ & Dagr Wiring):** Wire `escapement` `PoolWork` intake, durable XERJ `corpusSync` watermark activities, `dagr-producer` run-file emission, and `Signet` checkpoint receipts.
4. **Phase 4 (`hermit` `detcore` & `ouroborOS` KV Handoff):** Integrate `hermit` deterministic thread/RNG/clock scheduling for chaos replay of `littleguy` SQLite WAL journals and zero-copy KV-cache page handoff on `ouroborOS`.
