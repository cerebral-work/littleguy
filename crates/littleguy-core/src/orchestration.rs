//! 4-pillar whole-estate orchestration kernel (`specs/0002`).
//!
//! Unites Cortex subject topology, `wheelhorse` reversibility + canary tripwires,
//! `autumn-web` `AuthorityGrant` + AEO/`WebMCP` projections, `littleguy` `SeatSubstrate`,
//! and `escapement` typed pool dispatch. Pure and zero-I/O.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::mandate_bridge::{
    BiscuitMandateClaims, ConstitutionalSealedAction, MandateBridgeError, ReversibilityClass,
    SandboxClass,
};

// ─────────────────────────────────────────────────────────────────────────────
// 1. Cortex Subject Topology & Domain Gate Routing (REQ-ORCH-TOPO-01..02)
// ─────────────────────────────────────────────────────────────────────────────

/// Canonical enterprise mesh domains (`vespertin/specs/0033` & `cortex-domain`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeshDomainKind {
    /// Platform infrastructure, CI/CD, and cluster control plane (`RedGated`).
    Platform,
    /// Security, identity, containment, and supply-chain verification (`RedGated`).
    Security,
    /// Core engineering, agent fabric, memory, and runtime development (`LandOnGreen`).
    Development,
    /// Scientific and assurance research (`LandOnGreen` unless repo is `Security`).
    Research,
    /// Customer-facing web surfaces, brand, and games (`LandOnGreen`).
    Product,
}

impl MeshDomainKind {
    /// Canonical `snake_case` name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::Security => "security",
            Self::Development => "development",
            Self::Research => "research",
            Self::Product => "product",
        }
    }

    /// Default gate policy for this domain (`REQ-0033-05`, `REQ-0033-06`).
    #[must_use]
    pub const fn gate_policy(self) -> DomainGatePolicy {
        match self {
            Self::Platform | Self::Security => DomainGatePolicy::RedGated,
            Self::Development | Self::Research | Self::Product => DomainGatePolicy::LandOnGreen,
        }
    }
}

impl fmt::Display for MeshDomainKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Gate policy governing whether earned autonomy (`AutonomyTier::Autonomous`)
/// may auto-commit mutations without human sign-off (`REQ-ORCH-TOPO-02`, `REQ-ORCH-WH-03`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainGatePolicy {
    /// Requires explicit human approval (Soma Gate invariant); earned autonomy never auto-passes.
    RedGated,
    /// Permits autonomous merge-train queueing when Wheelhorse reversibility and CI checks pass.
    LandOnGreen,
}

impl DomainGatePolicy {
    /// Canonical `snake_case` identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RedGated => "red_gated",
            Self::LandOnGreen => "land_on_green",
        }
    }
}

impl fmt::Display for DomainGatePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The 9 canonical estate subjects (`CER-3494` + `comms` ruling `2026-11-10`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EstateSubject {
    /// Merge train, landing, rulesets, CI workflows, runners, and release/pin flows.
    Delivery,
    /// Cygnus Kubernetes cluster, Talos, Argo CD waves, charts, and observability.
    ClusterGitops,
    /// `OpenBao`, token minting, cosign/Kyverno, SPIFFE, and tailnet ACLs.
    IdentitySupplyChain,
    /// Cortex mesh, `herdr`, `saga` orchestration, lanes/wake, and `revenant`.
    AgentFabric,
    /// Org handbook, omnichannel adapters (`slack-bot`, `buzz`), and `cerebral-work/comms`.
    Comms,
    /// `reverie`, `xerj`, vault conventions, and corpora indexing.
    MemoryKnowledge,
    /// `vespertin` OS, domain crates, `reflexd`, `escapement`, and `scorcher`.
    RuntimeKernel,
    /// Security audits, threat models, `vilicus` jury, `autoresearcher`, and spec hygiene.
    AssuranceResearch,
    /// External web sites (`cerebral-site`, `inside.cerebral.work`), `rina`, and `wow-forever`.
    ProductGames,
}

impl EstateSubject {
    /// All 9 canonical estate subjects in topological order.
    pub const ALL: [Self; 9] = [
        Self::Delivery,
        Self::ClusterGitops,
        Self::IdentitySupplyChain,
        Self::AgentFabric,
        Self::Comms,
        Self::MemoryKnowledge,
        Self::RuntimeKernel,
        Self::AssuranceResearch,
        Self::ProductGames,
    ];

    /// Canonical kebab-case slug used by `cortex subject set <slug>`.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Delivery => "delivery",
            Self::ClusterGitops => "cluster-gitops",
            Self::IdentitySupplyChain => "identity-supply-chain",
            Self::AgentFabric => "agent-fabric",
            Self::Comms => "comms",
            Self::MemoryKnowledge => "memory-knowledge",
            Self::RuntimeKernel => "runtime-kernel",
            Self::AssuranceResearch => "assurance-research",
            Self::ProductGames => "product-games",
        }
    }

    /// Parses a subject slug (case-insensitive, trimmed).
    #[must_use]
    pub fn parse(input: &str) -> Option<Self> {
        let clean = input.trim();
        Self::ALL
            .into_iter()
            .find(|s| s.slug().eq_ignore_ascii_case(clean))
    }

    /// Canonical star callsign for this subject cell.
    #[must_use]
    pub const fn callsign(self) -> &'static str {
        match self {
            Self::Delivery => "mirfak",
            Self::ClusterGitops => "vega",
            Self::IdentitySupplyChain => "altair",
            Self::AgentFabric => "sadr",
            Self::Comms => "alpheratz",
            Self::MemoryKnowledge => "mira",
            Self::RuntimeKernel => "rigel",
            Self::AssuranceResearch => "thuban",
            Self::ProductGames => "alphard",
        }
    }

    /// Default [`MeshDomainKind`] for this subject (`cortex-domain::SUBJECT_DOMAINS`).
    #[must_use]
    pub const fn domain(self) -> MeshDomainKind {
        match self {
            Self::Delivery | Self::ClusterGitops => MeshDomainKind::Platform,
            Self::IdentitySupplyChain => MeshDomainKind::Security,
            Self::AgentFabric | Self::Comms | Self::MemoryKnowledge | Self::RuntimeKernel => {
                MeshDomainKind::Development
            }
            Self::AssuranceResearch => MeshDomainKind::Research,
            Self::ProductGames => MeshDomainKind::Product,
        }
    }

    /// Minimum [`SandboxClass`] floor required for pool workers executing in this subject.
    #[must_use]
    pub const fn default_sandbox_floor(self) -> SandboxClass {
        match self {
            Self::IdentitySupplyChain | Self::AssuranceResearch => SandboxClass::Firecracker,
            Self::Delivery | Self::ClusterGitops | Self::RuntimeKernel => SandboxClass::Gvisor,
            Self::AgentFabric | Self::Comms | Self::MemoryKnowledge | Self::ProductGames => {
                SandboxClass::Process
            }
        }
    }

    /// Default pool queue name on Cortex / `escapement`.
    #[must_use]
    pub const fn default_pool(self) -> &'static str {
        match self {
            Self::Delivery => "vilicus-pool",
            Self::ClusterGitops | Self::IdentitySupplyChain => "paas-pool",
            Self::AgentFabric => "cortex-pool",
            Self::Comms => "comms-pool",
            Self::MemoryKnowledge => "reverie-pool",
            Self::RuntimeKernel => "vespertin-pool",
            Self::AssuranceResearch => "research-pool",
            Self::ProductGames => "product-pool",
        }
    }
}

impl fmt::Display for EstateSubject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

/// Resolves the effective [`DomainGatePolicy`] for a `(subject, repo)` pair (`REQ-ORCH-TOPO-02`).
///
/// Per `vespertin/specs/0033` and the `C1c` ruling (`2026-11-07`), repositories under
/// `Platform` or `Security` (`unsigned-gg/unsigned-paas`, `cerebral-work/vilicus`,
/// `cerebral-work/revenant`, `unsigned-gg/autoresearcher`, `cerebral-work/krbrs`)
/// are always `RedGated`, even when touched by a `Development` or `Research` subject lane.
#[must_use]
pub fn resolve_domain_gate_policy(subject: EstateSubject, repo: Option<&str>) -> DomainGatePolicy {
    if subject.domain().gate_policy() == DomainGatePolicy::RedGated {
        return DomainGatePolicy::RedGated;
    }
    if let Some(slug) = repo {
        let clean = slug.trim();
        if is_red_gated_repo(clean) {
            return DomainGatePolicy::RedGated;
        }
    }
    DomainGatePolicy::LandOnGreen
}

/// Returns `true` when `repo` (`<owner>/<name>`) is classified as `Platform` or `Security`
/// under `vespertin/specs/0033` and `EstateRepoRegistry`.
#[must_use]
pub fn is_red_gated_repo(repo: &str) -> bool {
    matches!(
        repo,
        "unsigned-gg/unsigned-paas"
            | "cerebral-work/vilicus"
            | "cerebral-work/revenant"
            | "todie/revenant"
            | "unsigned-gg/revenant"
            | "unsigned-gg/autoresearcher"
            | "cerebral-work/krbrs"
            | "unsigned-gg/metal-foundry"
            | "unsigned-gg/dr"
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Wheelhorse Reversibility & Canary Tripwire Gate (REQ-ORCH-WH-01..04)
// ─────────────────────────────────────────────────────────────────────────────

/// Closed action kinds evaluated by the Wheelhorse reversibility pipeline
/// (`crm-core-reversibility` & `vespertin/specs/0135`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WheelhorseActionKind {
    /// Pure read-only probe, query, or static analysis check (`Pure`).
    ReadOnlyProbe,
    /// Idempotent branch push or draft PR update on `pool/<task_id>` (`Idempotent`).
    PoolBranchPush,
    /// Opening or updating a draft pull request (`Compensatable` by closing the draft PR).
    DraftPullRequest,
    /// Applying a reversible repository commit or config change (`Compensatable` by `git revert`).
    RepoCommit,
    /// Adding the `train` label to queue a PR on the `vilicus` merge train (`Compensatable` before merge).
    LabelMergeTrain,
    /// Syncing a Kubernetes / Argo CD application (`Compensatable` only with pinned rollback SHA).
    ClusterAppSync,
    /// Rotating a secret or cryptographic key (`Irreversible` one-way door).
    SecretRotation,
    /// Sending an external human notification or public email/message (`Irreversible`).
    ExternalHumanSend,
    /// Deleting durable state, PVCs, or retention-expired records (`Irreversible`).
    DestructiveErasure,
}

impl WheelhorseActionKind {
    /// All closed action variants.
    pub const ALL: [Self; 9] = [
        Self::ReadOnlyProbe,
        Self::PoolBranchPush,
        Self::DraftPullRequest,
        Self::RepoCommit,
        Self::LabelMergeTrain,
        Self::ClusterAppSync,
        Self::SecretRotation,
        Self::ExternalHumanSend,
        Self::DestructiveErasure,
    ];

    /// Canonical `snake_case` identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnlyProbe => "read_only_probe",
            Self::PoolBranchPush => "pool_branch_push",
            Self::DraftPullRequest => "draft_pull_request",
            Self::RepoCommit => "repo_commit",
            Self::LabelMergeTrain => "label_merge_train",
            Self::ClusterAppSync => "cluster_app_sync",
            Self::SecretRotation => "secret_rotation",
            Self::ExternalHumanSend => "external_human_send",
            Self::DestructiveErasure => "destructive_erasure",
        }
    }

    /// Stage 1 (`classify`): total mapping from [`WheelhorseActionKind`] to [`ReversibilityClass`].
    #[must_use]
    pub const fn classify(self) -> ReversibilityClass {
        match self {
            Self::ReadOnlyProbe => ReversibilityClass::Pure,
            Self::PoolBranchPush => ReversibilityClass::Idempotent,
            Self::DraftPullRequest
            | Self::RepoCommit
            | Self::LabelMergeTrain
            | Self::ClusterAppSync => ReversibilityClass::Compensatable,
            Self::SecretRotation | Self::ExternalHumanSend | Self::DestructiveErasure => {
                ReversibilityClass::Irreversible
            }
        }
    }
}

impl fmt::Display for WheelhorseActionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Machine-validated compensation plan attached to a `Compensatable` action (`REQ-ORCH-WH-04`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompensationPlan {
    /// Exact 40-character lowercase hex SHA-1 of the pre-image `HEAD` commit (`specs/0135` `REQ-0135-05`).
    pub head_sha: String,
    /// Exact 64-character lowercase hex BLAKE3 digest of the diff (`specs/0135` `REQ-0135-05`).
    pub diff_blake3: String,
    /// Size of the evaluated diff in bytes (`> 0`).
    pub diff_bytes: u64,
    /// Deterministic inverse command array (no shell interpolation, e.g. `["git", "revert", "--no-edit", "<sha>"]`).
    pub undo_argv: Vec<String>,
    /// Undo window duration in seconds during which automatic rollback is guaranteed.
    pub undo_window_secs: u64,
}

impl CompensationPlan {
    /// Validates structural invariants of the compensation plan (`Stage 3: validate_plan`).
    ///
    /// # Errors
    ///
    /// Returns a defect string when `head_sha` is not 40 lowercase hex chars, `diff_blake3`
    /// is not 64 lowercase hex chars, `diff_bytes == 0`, `undo_argv` is empty, or
    /// `undo_window_secs == 0`.
    pub fn validate(&self) -> Result<(), String> {
        if !is_lowercase_hex_len(&self.head_sha, 40) {
            return Err(format!(
                "invalid head_sha `{}`: expected 40 lowercase hex characters",
                self.head_sha
            ));
        }
        if !is_lowercase_hex_len(&self.diff_blake3, 64) {
            return Err(format!(
                "invalid diff_blake3 `{}`: expected 64 lowercase hex characters",
                self.diff_blake3
            ));
        }
        if self.diff_bytes == 0 {
            return Err("diff content is empty (diff_bytes == 0)".to_owned());
        }
        if self.undo_argv.is_empty() || self.undo_argv.iter().any(|arg| arg.trim().is_empty()) {
            return Err("undo_argv must be non-empty with non-blank arguments".to_owned());
        }
        if self.undo_window_secs == 0 {
            return Err("undo_window_secs must be > 0".to_owned());
        }
        Ok(())
    }
}

/// What an agent declares before executing an orchestration step (`REQ-ORCH-WH-01`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WheelhorseActionDescriptor {
    /// Actor name (e.g. `vilicus-lane`, `vespertin-pool`).
    pub actor: String,
    /// Capability name (e.g. `train-delivery`, `lane-package-v5`).
    pub capability: String,
    /// Subject cell routing this action.
    pub subject: EstateSubject,
    /// Target repository (`<owner>/<name>`), if any.
    pub target_repo: Option<String>,
    /// Closed action kind.
    pub kind: WheelhorseActionKind,
    /// Relative paths or entity keys touched by the action.
    pub touched_paths: Vec<String>,
    /// Optional constitutional sealed action check (`REQ-LG-MANDATE-05`).
    pub sealed_action: Option<ConstitutionalSealedAction>,
}

/// Canary tripwire registry (`crm-core-tripwire` & `REQ-ORCH-WH-02`).
///
/// Holds synthetic honeypot paths/entities and estate-protected credential paths
/// that no legitimate autonomous workflow may touch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripwireRegistry {
    canary_prefixes: BTreeSet<String>,
    canary_entities: BTreeSet<String>,
}

impl Default for TripwireRegistry {
    fn default() -> Self {
        Self::estate_default()
    }
}

impl TripwireRegistry {
    /// Default estate canary & credential tripwire registry (`REQ-ORCH-WH-02`).
    #[must_use]
    pub fn estate_default() -> Self {
        let canary_prefixes = [
            ".ssh/",
            ".gpg/",
            ".secrets",
            ".gitconfig",
            "login.keyring",
            "keyrings/login.keyring",
            "canary/honeypot/",
            "tripwire_canaries/",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();

        let canary_entities = [
            "canary:contact:00000000-0000-0000-0000-000000000001",
            "canary:secret:prod-master-token",
            "canary:repo:honeypot-vault",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();

        Self {
            canary_prefixes,
            canary_entities,
        }
    }

    /// Registers an additional canary path prefix.
    pub fn insert_prefix(&mut self, prefix: impl Into<String>) {
        self.canary_prefixes.insert(prefix.into());
    }

    /// Registers an additional synthetic canary entity key.
    pub fn insert_entity(&mut self, entity: impl Into<String>) {
        self.canary_entities.insert(entity.into());
    }

    /// Checks whether any path in `paths` triggers a tripwire. Returns the first matching tripwire.
    #[must_use]
    pub fn check_paths(&self, paths: &[String]) -> Option<String> {
        for raw_path in paths {
            let normalized = raw_path.trim().trim_start_matches("./");
            if self.canary_entities.contains(normalized) {
                return Some(format!("canary entity touched: `{normalized}`"));
            }
            for prefix in &self.canary_prefixes {
                if normalized == prefix.trim_end_matches('/')
                    || normalized.starts_with(prefix.as_str())
                    || normalized.contains(&format!("/{prefix}"))
                {
                    return Some(format!(
                        "tripwire path `{normalized}` matched protected canary `{prefix}`"
                    ));
                }
            }
        }
        None
    }
}

/// Outcome of the 4-stage Wheelhorse gate (`REQ-ORCH-WH-01..04`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", content = "reason", rename_all = "snake_case")]
pub enum WheelhorseGateDecision {
    /// Passes autonomously without human intervention (`Pure` or `Idempotent`, or `Compensatable` at `Autonomous`).
    AutoCommit(String),
    /// Passes autonomously and emits an operator notice (`specs/0135` `auto_commit_with_notice`).
    AutoCommitWithNotice(String),
    /// Falls back to `heimdal:approval` / human operator queue (`specs/0135` `REQ-0135-03`).
    RouteToApproval(String),
    /// Blocked immediately because a canary tripwire or protected path was touched (`REQ-ORCH-WH-02`).
    BlockedByTripwire(String),
    /// Unconditionally refused by constitutional or mandate policy (`ApprovalGate::Never`).
    BlockedByPolicy(String),
}

impl WheelhorseGateDecision {
    /// Returns `true` only when the gate permits immediate autonomous execution.
    #[must_use]
    pub const fn permits_autonomous_execution(&self) -> bool {
        matches!(self, Self::AutoCommit(_) | Self::AutoCommitWithNotice(_))
    }

    /// Canonical `pass` or `fallback` or `blocked` summary matching `specs/0135` `REQ-0135-05`.
    #[must_use]
    pub const fn decision_label(&self) -> &'static str {
        match self {
            Self::AutoCommit(_) | Self::AutoCommitWithNotice(_) => "pass",
            Self::RouteToApproval(_) => "fallback",
            Self::BlockedByTripwire(_) | Self::BlockedByPolicy(_) => "blocked",
        }
    }

    /// Human-readable reason string.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::AutoCommit(r)
            | Self::AutoCommitWithNotice(r)
            | Self::RouteToApproval(r)
            | Self::BlockedByTripwire(r)
            | Self::BlockedByPolicy(r) => r,
        }
    }
}

/// Immutable audit record emitted by [`WheelhorseGate::evaluate`] (`specs/0135` `REQ-0135-05`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WheelhorseGateRecord {
    /// `"pass"`, `"fallback"`, or `"blocked"`.
    pub decision: String,
    /// Full structured gate verdict.
    pub verdict: WheelhorseGateDecision,
    /// Actor that requested the action.
    pub actor: String,
    /// Capability evaluated by the gate.
    pub capability: String,
    /// Subject cell of the action.
    pub subject: EstateSubject,
    /// Effective domain gate policy (`RedGated` vs `LandOnGreen`).
    pub domain_gate_policy: DomainGatePolicy,
    /// Stage 1 reversibility classification.
    pub reversibility_class: ReversibilityClass,
    /// Pre-image `HEAD` SHA-1 if a compensation plan was supplied.
    pub head_sha: Option<String>,
    /// Diff BLAKE3 digest if a compensation plan was supplied.
    pub diff_blake3: Option<String>,
    /// Diff size in bytes if a compensation plan was supplied.
    pub diff_bytes: Option<u64>,
    /// Whether the compensation plan passed Stage 3 validation.
    pub plan_validated: bool,
    /// Defect string if Stage 3 validation failed.
    pub plan_defect: Option<String>,
    /// Evaluation timestamp in milliseconds since Unix epoch.
    pub evaluated_at_ms: u64,
}

/// Pure 4-stage Wheelhorse reversibility and canary tripwire gate (`REQ-ORCH-WH-01..04`).
///
/// Authority in the estate is never accumulated via streak-based earned autonomy.
/// Admission depends strictly on:
/// 1. Absence of Constitutional Sealed Actions (`REQ-LG-MANDATE-05`)
/// 2. Canary Tripwire clearance (`REQ-ORCH-WH-02`)
/// 3. Soma Domain Gate policy (`DomainGatePolicy::{LandOnGreen, RedGated}`, `REQ-ORCH-WH-03`)
/// 4. Deterministic `ReversibilityClass` + validated `CompensationPlan` (`REQ-ORCH-WH-04`)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WheelhorseGate {
    /// Active canary tripwire registry.
    tripwires: TripwireRegistry,
}

impl WheelhorseGate {
    /// Creates a new `WheelhorseGate` with the default estate canary tripwires.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tripwires: TripwireRegistry::estate_default(),
        }
    }

    /// Mutable access to the [`TripwireRegistry`].
    pub const fn tripwires_mut(&mut self) -> &mut TripwireRegistry {
        &mut self.tripwires
    }

    /// Executes the 4-stage Wheelhorse decision pipeline (`classify -> capture_plan -> validate -> decide`).
    ///
    /// If `human_override` is `Some(false)`, human denial wins unconditionally (`REQ-0135-04`).
    /// If `human_override` is `Some(true)`, human approval wins unless a Constitutional Sealed
    /// Action or Canary Tripwire is violated (`REQ-0135-04`, `REQ-LG-MANDATE-05`).
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn evaluate(
        &self,
        descriptor: &WheelhorseActionDescriptor,
        plan: Option<&CompensationPlan>,
        human_override: Option<bool>,
        evaluated_at_ms: u64,
    ) -> WheelhorseGateRecord {
        let class = descriptor.kind.classify();
        let domain_gate_policy =
            resolve_domain_gate_policy(descriptor.subject, descriptor.target_repo.as_deref());

        let (head_sha, diff_blake3, diff_bytes, plan_validated, plan_defect) = match (class, plan) {
            (ReversibilityClass::Irreversible, _) | (_, None) => (None, None, None, false, None),
            (_, Some(p)) => match p.validate() {
                Ok(()) => (
                    Some(p.head_sha.clone()),
                    Some(p.diff_blake3.clone()),
                    Some(p.diff_bytes),
                    true,
                    None,
                ),
                Err(defect) => (
                    Some(p.head_sha.clone()),
                    Some(p.diff_blake3.clone()),
                    Some(p.diff_bytes),
                    false,
                    Some(defect),
                ),
            },
        };

        // 0. Constitutional Sealed Actions are unconditionally blocked (`REQ-LG-MANDATE-05`).
        if let Some(sealed) = descriptor.sealed_action {
            let verdict = WheelhorseGateDecision::BlockedByPolicy(format!(
                "constitutional sealed action `{sealed}` is unconditionally forbidden"
            ));
            return Self::build_record(
                verdict,
                descriptor,
                domain_gate_policy,
                class,
                head_sha,
                diff_blake3,
                diff_bytes,
                plan_validated,
                plan_defect,
                evaluated_at_ms,
            );
        }

        // 1. Canary Tripwire check (`REQ-ORCH-WH-02`).
        if let Some(hit) = self.tripwires.check_paths(&descriptor.touched_paths) {
            let verdict = WheelhorseGateDecision::BlockedByTripwire(hit);
            return Self::build_record(
                verdict,
                descriptor,
                domain_gate_policy,
                class,
                head_sha,
                diff_blake3,
                diff_bytes,
                plan_validated,
                plan_defect,
                evaluated_at_ms,
            );
        }

        // 2. Human heimdal signal precedence (`REQ-0135-04`).
        if let Some(approved) = human_override {
            let verdict = if approved {
                WheelhorseGateDecision::AutoCommit("human heimdal signal approved".to_owned())
            } else {
                WheelhorseGateDecision::BlockedByPolicy("human heimdal signal denied".to_owned())
            };
            return Self::build_record(
                verdict,
                descriptor,
                domain_gate_policy,
                class,
                head_sha,
                diff_blake3,
                diff_bytes,
                plan_validated,
                plan_defect,
                evaluated_at_ms,
            );
        }

        // 3. Pure read-only probes always pass.
        if class == ReversibilityClass::Pure {
            let verdict = WheelhorseGateDecision::AutoCommit("pure read-only probe".to_owned());
            return Self::build_record(
                verdict,
                descriptor,
                domain_gate_policy,
                class,
                head_sha,
                diff_blake3,
                diff_bytes,
                plan_validated,
                plan_defect,
                evaluated_at_ms,
            );
        }

        // 4. Soma Gate Invariant (`REQ-ORCH-WH-03`):
        //    Irreversible actions (one-way doors) and RedGated domains ALWAYS route to explicit operator approval.
        if class == ReversibilityClass::Irreversible {
            let verdict = WheelhorseGateDecision::RouteToApproval(format!(
                "wheelhorse fallback: action `{}` is irreversible (one-way door)",
                descriptor.kind
            ));
            return Self::build_record(
                verdict,
                descriptor,
                domain_gate_policy,
                class,
                head_sha,
                diff_blake3,
                diff_bytes,
                plan_validated,
                plan_defect,
                evaluated_at_ms,
            );
        }

        if domain_gate_policy == DomainGatePolicy::RedGated {
            let verdict = WheelhorseGateDecision::RouteToApproval(format!(
                "wheelhorse fallback:soma_gate: subject `{}` / repo `{}` is red_gated",
                descriptor.subject,
                descriptor.target_repo.as_deref().unwrap_or("-")
            ));
            return Self::build_record(
                verdict,
                descriptor,
                domain_gate_policy,
                class,
                head_sha,
                diff_blake3,
                diff_bytes,
                plan_validated,
                plan_defect,
                evaluated_at_ms,
            );
        }

        // 5. Stage 3 & 4 (`validate` & `decide` for LandOnGreen Idempotent / Compensatable).
        let verdict = match class {
            ReversibilityClass::Idempotent => WheelhorseGateDecision::AutoCommit(format!(
                "land_on_green idempotent action `{}` for {}/{}",
                descriptor.kind, descriptor.actor, descriptor.capability
            )),
            ReversibilityClass::Compensatable => {
                if plan_validated {
                    WheelhorseGateDecision::AutoCommitWithNotice(format!(
                        "land_on_green compensatable action for {}/{}, verdict auto_commit_with_notice (head {})",
                        descriptor.actor,
                        descriptor.capability,
                        head_sha.as_deref().unwrap_or("-")
                    ))
                } else {
                    let why = plan_defect
                        .clone()
                        .unwrap_or_else(|| "missing compensation plan".to_owned());
                    WheelhorseGateDecision::RouteToApproval(format!(
                        "wheelhorse fallback: invalid compensation plan: {why}"
                    ))
                }
            }
            ReversibilityClass::Pure | ReversibilityClass::Irreversible => unreachable!(),
        };

        Self::build_record(
            verdict,
            descriptor,
            domain_gate_policy,
            class,
            head_sha,
            diff_blake3,
            diff_bytes,
            plan_validated,
            plan_defect,
            evaluated_at_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_record(
        verdict: WheelhorseGateDecision,
        descriptor: &WheelhorseActionDescriptor,
        domain_gate_policy: DomainGatePolicy,
        reversibility_class: ReversibilityClass,
        head_sha: Option<String>,
        diff_blake3: Option<String>,
        diff_bytes: Option<u64>,
        plan_validated: bool,
        plan_defect: Option<String>,
        evaluated_at_ms: u64,
    ) -> WheelhorseGateRecord {
        WheelhorseGateRecord {
            decision: verdict.decision_label().to_owned(),
            verdict,
            actor: descriptor.actor.clone(),
            capability: descriptor.capability.clone(),
            subject: descriptor.subject,
            domain_gate_policy,
            reversibility_class,
            head_sha,
            diff_blake3,
            diff_bytes,
            plan_validated,
            plan_defect,
            evaluated_at_ms,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Autumn-Web AgentAuthority, AEO Markdown & WebMCP (REQ-ORCH-AUTUMN-01..02)
// ─────────────────────────────────────────────────────────────────────────────

/// Side-effect vocabulary from `autumn_web::agent_authority::EffectKind` (`REQ-ORCH-AUTUMN-01`).
///
/// Ordered so the unsafe side of a branch join is greater (`UnboundedWrite > Write`, `CrossTenant > Write`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    /// Row-bounded or branch-bounded write (`save`, `insert`, `pool/<task_id>` commit).
    Write,
    /// Write with no proven row bound (`delete_all`, unfiltered update, force-push).
    UnboundedWrite,
    /// Query or mutation that crosses tenant or subject boundaries.
    CrossTenant,
    /// Outbound HTTP/gRPC call to an external host.
    Outbound,
    /// Outbound webhook dispatch.
    Webhook,
    /// Background workflow or pool job enqueue.
    Job,
}

impl EffectKind {
    /// Canonical `snake_case` spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::UnboundedWrite => "unbounded_write",
            Self::CrossTenant => "cross_tenant",
            Self::Outbound => "outbound",
            Self::Webhook => "webhook",
            Self::Job => "job",
        }
    }
}

impl fmt::Display for EffectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the claim behind an [`AgentEffect`] was established (`autumn_web::agent_authority::EffectProvenance`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectProvenance {
    /// Recovered from a static Rust type or protobuf descriptor (`TypeResolved`).
    TypeResolved,
    /// Recovered from syntactic analysis of a command or route (`Syntactic`).
    Syntactic,
    /// Explicitly declared by an operator or `#[agent_effect(...)]` attribute (`Declared`).
    Declared,
}

/// A single side effect proved or declared for a seat action (`autumn-web` bridge).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AgentEffect {
    /// Effect classification.
    pub kind: EffectKind,
    /// Target model, repository slug (`<owner>/<name>`), host, or job type.
    pub target: String,
    /// Provenance of the effect claim.
    pub provenance: EffectProvenance,
}

/// Declarative authority envelope modeled on `autumn_web::authority_grant!` (`REQ-ORCH-AUTUMN-01`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorityGrant {
    /// Grant identifier (e.g. `PoolWorkerDraftGrant`).
    pub name: String,
    /// Allowed `Write` targets (repositories or models).
    pub allowed_writes: BTreeSet<String>,
    /// Whether `UnboundedWrite` is ever permitted (always `false` for pool workers).
    pub allow_unbounded_write: bool,
    /// Whether `CrossTenant` access is permitted.
    pub allow_cross_tenant: bool,
    /// Allowed outbound hosts or URI prefixes.
    pub allowed_outbound: BTreeSet<String>,
    /// Allowed background job / pool names.
    pub allowed_jobs: BTreeSet<String>,
    /// Maximum invocations per minute (`rate` governor).
    pub max_rate_per_min: u32,
    /// Maximum permitted [`ReversibilityClass`] (`Compensatable` for autonomous workers).
    pub max_reversibility: ReversibilityClass,
}

/// Errors returned when an action violates an [`AuthorityGrant`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthorityViolation {
    /// Handler attempted an `UnboundedWrite` when the grant forbids it.
    #[error("grant `{grant}` forbids unbounded_write on `{target}`")]
    UnboundedWriteForbidden {
        /// Grant name.
        grant: String,
        /// Target identifier.
        target: String,
    },
    /// Handler attempted a `CrossTenant` operation when the grant is tenant-scoped.
    #[error("grant `{grant}` forbids cross_tenant access on `{target}`")]
    CrossTenantForbidden {
        /// Grant name.
        grant: String,
        /// Target identifier.
        target: String,
    },
    /// Target is not listed in the grant's allowlist for that [`EffectKind`].
    #[error("grant `{grant}` does not allow `{kind}` on `{target}`")]
    TargetNotAllowed {
        /// Grant name.
        grant: String,
        /// Effect kind.
        kind: EffectKind,
        /// Target identifier.
        target: String,
    },
    /// Call rate exceeded `max_rate_per_min`.
    #[error("grant `{grant}` rate limit exceeded: {observed_per_min}/min > {max_per_min}/min")]
    RateLimitExceeded {
        /// Grant name.
        grant: String,
        /// Observed calls in the current 1-minute window.
        observed_per_min: u32,
        /// Configured ceiling.
        max_per_min: u32,
    },
    /// Action reversibility class is less reversible (higher impact) than `max_reversibility`.
    #[error("grant `{grant}` allows at most `{allowed}`, but action is `{actual}`")]
    ReversibilityExceeded {
        /// Grant name.
        grant: String,
        /// Maximum allowed class.
        allowed: ReversibilityClass,
        /// Actual action class.
        actual: ReversibilityClass,
    },
}

impl AuthorityGrant {
    /// Standard bounded authority grant for a seated pool worker (`REQ-ORCH-AUTUMN-01`).
    #[must_use]
    pub fn pool_worker_default(
        name: impl Into<String>,
        allowed_repos: impl IntoIterator<Item = String>,
        allowed_pools: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            name: name.into(),
            allowed_writes: allowed_repos.into_iter().collect(),
            allow_unbounded_write: false,
            allow_cross_tenant: false,
            allowed_outbound: [
                "https://api.github.com".to_owned(),
                "https://llm.unsigned.gg".to_owned(),
            ]
            .into_iter()
            .collect(),
            allowed_jobs: allowed_pools.into_iter().collect(),
            max_rate_per_min: 30,
            max_reversibility: ReversibilityClass::Compensatable,
        }
    }

    /// Verifies a slice of [`AgentEffect`]s, the action's [`ReversibilityClass`], and the current
    /// 1-minute call count against this grant (`REQ-ORCH-AUTUMN-01`).
    ///
    /// # Errors
    ///
    /// Returns [`AuthorityViolation`] on the first effect, rate, or reversibility violation.
    pub fn check_effects(
        &self,
        effects: &[AgentEffect],
        reversibility: ReversibilityClass,
        calls_in_current_minute: u32,
    ) -> Result<(), AuthorityViolation> {
        if calls_in_current_minute > self.max_rate_per_min {
            return Err(AuthorityViolation::RateLimitExceeded {
                grant: self.name.clone(),
                observed_per_min: calls_in_current_minute,
                max_per_min: self.max_rate_per_min,
            });
        }
        if reversibility > self.max_reversibility {
            return Err(AuthorityViolation::ReversibilityExceeded {
                grant: self.name.clone(),
                allowed: self.max_reversibility,
                actual: reversibility,
            });
        }
        for effect in effects {
            match effect.kind {
                EffectKind::Write => {
                    if !matches_prefix_or_exact(&self.allowed_writes, &effect.target) {
                        return Err(AuthorityViolation::TargetNotAllowed {
                            grant: self.name.clone(),
                            kind: EffectKind::Write,
                            target: effect.target.clone(),
                        });
                    }
                }
                EffectKind::UnboundedWrite => {
                    if !self.allow_unbounded_write {
                        return Err(AuthorityViolation::UnboundedWriteForbidden {
                            grant: self.name.clone(),
                            target: effect.target.clone(),
                        });
                    }
                }
                EffectKind::CrossTenant => {
                    if !self.allow_cross_tenant {
                        return Err(AuthorityViolation::CrossTenantForbidden {
                            grant: self.name.clone(),
                            target: effect.target.clone(),
                        });
                    }
                }
                EffectKind::Outbound | EffectKind::Webhook => {
                    if !matches_prefix_or_exact(&self.allowed_outbound, &effect.target) {
                        return Err(AuthorityViolation::TargetNotAllowed {
                            grant: self.name.clone(),
                            kind: effect.kind,
                            target: effect.target.clone(),
                        });
                    }
                }
                EffectKind::Job => {
                    if !matches_prefix_or_exact(&self.allowed_jobs, &effect.target) {
                        return Err(AuthorityViolation::TargetNotAllowed {
                            grant: self.name.clone(),
                            kind: EffectKind::Job,
                            target: effect.target.clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

/// Streamable HTTP `WebMCP` tool descriptor (`autumn_web::aeo::webmcp`, `REQ-ORCH-AUTUMN-02`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebMcpToolDescriptor {
    /// Tool name exposed on `/mcp`.
    pub name: String,
    /// Concise description of the tool.
    pub description: String,
    /// HTTP method + route snapshot target (`GET /status`, `POST /tasks/submit`, etc.).
    pub route_target: String,
    /// Whether the tool is read-only / idempotent.
    pub read_only: bool,
}

/// Deterministic `WebMcpManifest` exposed by an Autumn-Web / Littleguy orchestration node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebMcpManifest {
    /// Service title (`site_name` in `AeoConfig`).
    pub site_name: String,
    /// Protocol revision (`2026-07-28`).
    pub protocol_revision: String,
    /// Whether the router snapshot is acyclic (mounted prior to `/mcp`).
    pub acyclic_router_snapshot: bool,
    /// Exported tools.
    pub tools: Vec<WebMcpToolDescriptor>,
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Littleguy SeatSubstrate & Escapement Claim Loop (REQ-ORCH-SEAT-01)
// ─────────────────────────────────────────────────────────────────────────────

/// Identifier of a long-lived agent seat (`vespertin/specs/uas/0027` `SeatId`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SeatId(pub String);

impl fmt::Display for SeatId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether a seat is alive and ready to accept work (`vespertin/specs/uas/0027` `Liveness`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeatLiveness {
    /// The seat actor is healthy and ready.
    Alive,
    /// The seat actor is unavailable or stopped, with reason.
    Dead(String),
}

/// Cryptographic proof left when a seat finishes a job (`vespertin/specs/uas/0027` `SeatReceipt`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatReceipt {
    /// Retry-idempotent job identifier (`delivery_token` / reflex action id).
    pub job_id: String,
    /// Queue item identifier (`job_id` or `qitem-uas-<24 hex>`).
    pub queue_item_id: String,
    /// Head branch pushed by the seat (`pool/<job_id>`, `REQ-0136-14`).
    pub head_branch: String,
    /// How the substrate closed the job.
    pub closure: String,
    /// Optional 64-hex SHA-256 of the seat's execution transcript.
    pub transcript_sha256: Option<String>,
    /// Attached Wheelhorse gate record that authorized the job.
    pub wheelhorse_record: WheelhorseGateRecord,
    /// The validated Ghost binding the seat embodied, when the delivery carried one (spec 0004).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ghost_binding: Option<crate::ghost_binding::GhostBinding>,
}

/// Job specification submitted to a seat (`vespertin/specs/uas/0027` `JobSpec`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatJobSpec {
    /// Retry-idempotent job identifier (`delivery_token` or reflex action id).
    pub job_id: String,
    /// Rendered deterministic brief (`escapement_core::pool::render_brief`).
    pub body: String,
    /// Target repository (`<owner>/<name>`).
    pub target_repo: Option<String>,
    /// Subject cell routing this task.
    pub subject: EstateSubject,
    /// Touched path allowlist from `TaskSpec.allowed_paths`.
    pub allowed_paths: Vec<String>,
    /// Required CI check identifiers (`<project>:<task>`, e.g. `escapement:test`).
    pub check_ids: Vec<String>,
    /// Optional compensation plan (pre-image `head_sha` + `diff_blake3`).
    pub compensation_plan: Option<CompensationPlan>,
}

impl SeatJobSpec {
    /// Computes the canonical OpenRig/Harvest queue item id (`vespertin/specs/uas/0027` D4, `REQ-0027-04`).
    ///
    /// Uses `job_id` directly when it matches `[A-Za-z0-9._-]{1,128}`, and falls back to
    /// `qitem-uas-<24 lowercase hex>` otherwise.
    #[must_use]
    pub fn queue_item_id(&self) -> String {
        derive_queue_item_id(&self.job_id)
    }

    /// Computes the canonical head branch `pool/<job_id>` (`REQ-0136-14`, `REQ-0027-14`).
    #[must_use]
    pub fn head_branch(&self) -> String {
        format!("pool/{}", self.queue_item_id())
    }

    /// Synthesizes the host-throttled (`-j 2`) cargo verification command for this job (`REQ-LG-SANDBOX-02`).
    #[must_use]
    pub fn throttled_cargo_check_argv(&self) -> Vec<String> {
        vec![
            "cargo".to_owned(),
            "test".to_owned(),
            "-j".to_owned(),
            "2".to_owned(),
            "--workspace".to_owned(),
        ]
    }
}

/// Execution state of a submitted seat job (`vespertin/specs/uas/0027` `JobState`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum SeatJobState {
    /// Queued or running inside the seat's `autumn-harvest` actor loop.
    Running {
        /// Queue item identifier.
        queue_item_id: String,
        /// Head branch (`pool/<id>`).
        head_branch: String,
        /// Throttled cargo command argv (`-j 2`).
        cargo_argv: Vec<String>,
    },
    /// Completed with a [`SeatReceipt`].
    Done(SeatReceipt),
    /// Failed or cancelled, with reason.
    Failed(String),
}

/// Errors returned by [`LittleguySeatSubstrate`] (`vespertin/specs/uas/0027` `SubstrateError`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SeatSubstrateError {
    /// Permanent error that a retry cannot fix (`[permanent] ...`, mapped to `TaskError::Payload`).
    #[error("[permanent] {0}")]
    Permanent(String),
    /// Recoverable fault (`TaskError::Recoverable` / `Nack`).
    #[error("recoverable: {0}")]
    Recoverable(String),
}

impl SeatSubstrateError {
    /// Returns `true` when a retry cannot fix this error (`REQ-0027-03`, `REQ-0027-10`).
    #[must_use]
    pub const fn is_permanent(&self) -> bool {
        matches!(self, Self::Permanent(_))
    }
}

/// Registration of a single `littleguy` durable seat slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LittleguySeatSlot {
    /// Seat identifier (`<subject>/<slot>`).
    pub seat_id: SeatId,
    /// Slot name (`slot-0`, `vilicus-pool`, etc.).
    pub slot_name: String,
    /// Working directory bound to the seat.
    pub cwd: PathBuf,
    /// Subject cell this seat belongs to.
    pub subject: EstateSubject,
    /// Active sandbox isolation class (`Process` .. `Ouroboros`).
    pub sandbox_class: SandboxClass,
    /// Verified Biscuit + `spiffe://unsigned.gg` mandate claims for this seat.
    pub mandate_claims: BiscuitMandateClaims,
    /// Autumn-Web `AuthorityGrant` governing this seat's side effects.
    pub authority_grant: AuthorityGrant,
    /// Current liveness state.
    pub liveness: SeatLiveness,
    /// Calls executed in the current 1-minute window.
    pub calls_in_current_minute: u32,
}

/// Durable `LittleguySeatSubstrate` uniting `escapement` (`uas/0027`), `wheelhorse` (`0135`),
/// `autumn-web` (`AuthorityGrant` + AEO/WebMCP), and `littleguy` (`Mandate` + `SandboxClass`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LittleguySeatSubstrate {
    /// Registered slots keyed by `slot_name`.
    slots: BTreeMap<String, LittleguySeatSlot>,
    /// Active and finished jobs keyed by `( SeatId, job_id )`.
    jobs: BTreeMap<(SeatId, String), SeatJobState>,
    /// Wheelhorse gate records keyed by `( SeatId, job_id )`.
    gate_records: BTreeMap<(SeatId, String), WheelhorseGateRecord>,
    /// Embedded Wheelhorse reversibility + tripwire gate.
    wheelhorse: WheelhorseGate,
}

impl LittleguySeatSubstrate {
    /// Creates an empty [`LittleguySeatSubstrate`] with default estate Wheelhorse rules.
    #[must_use]
    pub const fn new(wheelhorse: WheelhorseGate) -> Self {
        Self {
            slots: BTreeMap::new(),
            jobs: BTreeMap::new(),
            gate_records: BTreeMap::new(),
            wheelhorse,
        }
    }

    /// Immutable reference to the embedded [`WheelhorseGate`].
    #[must_use]
    pub const fn wheelhorse(&self) -> &WheelhorseGate {
        &self.wheelhorse
    }

    /// Mutable reference to the embedded [`WheelhorseGate`].
    pub const fn wheelhorse_mut(&mut self) -> &mut WheelhorseGate {
        &mut self.wheelhorse
    }

    /// Pre-configures a slot with its subject, [`SandboxClass`], [`BiscuitMandateClaims`], and [`AuthorityGrant`].
    ///
    /// Validates both the Biscuit mandate compilation and the subject's minimum `SandboxClass` floor
    /// (`REQ-LG-SANDBOX-01`, `REQ-ORCH-SEAT-01`).
    ///
    /// # Errors
    ///
    /// Returns [`SeatSubstrateError::Permanent`] if the Biscuit mandate is invalid/expired or if
    /// `sandbox_class` violates either the subject floor or the Biscuit mandate floor.
    #[allow(clippy::too_many_arguments)]
    pub fn register_slot(
        &mut self,
        slot_name: impl Into<String>,
        cwd: impl Into<PathBuf>,
        subject: EstateSubject,
        sandbox_class: SandboxClass,
        mandate_claims: BiscuitMandateClaims,
        authority_grant: AuthorityGrant,
        now_ms: u64,
    ) -> Result<SeatId, SeatSubstrateError> {
        let slot_name = slot_name.into();
        let cwd = cwd.into();

        // Enforce subject sandbox floor (`REQ-ORCH-TOPO-01`).
        let subject_floor = subject.default_sandbox_floor();
        if !sandbox_class.satisfies_floor(subject_floor) {
            return Err(SeatSubstrateError::Permanent(format!(
                "slot `{slot_name}` in subject `{subject}` requires at least sandbox `{subject_floor}`, got `{sandbox_class}`"
            )));
        }

        // Compile and verify `spiffe://unsigned.gg` + Biscuit mandate (`REQ-LG-MANDATE-01..05`).
        mandate_claims
            .compile_to_harvest_mandate(sandbox_class, now_ms)
            .map_err(|e: MandateBridgeError| SeatSubstrateError::Permanent(e.to_string()))?;

        let seat_id = SeatId(format!("{subject}/{slot_name}"));
        self.slots.insert(
            slot_name.clone(),
            LittleguySeatSlot {
                seat_id: seat_id.clone(),
                slot_name,
                cwd,
                subject,
                sandbox_class,
                mandate_claims,
                authority_grant,
                liveness: SeatLiveness::Alive,
                calls_in_current_minute: 0,
            },
        );
        Ok(seat_id)
    }

    /// Ensures a seat exists and is running for `slot` with `cwd` (`vespertin/specs/uas/0027` `ensure_seat`).
    ///
    /// # Errors
    ///
    /// Returns [`SeatSubstrateError::Recoverable`] if the slot is not registered.
    pub fn ensure_seat(&mut self, slot: &str, cwd: &Path) -> Result<SeatId, SeatSubstrateError> {
        let entry = self.slots.get_mut(slot).ok_or_else(|| {
            SeatSubstrateError::Recoverable(format!("slot `{slot}` is not registered"))
        })?;
        entry.cwd = cwd.to_path_buf();
        Ok(entry.seat_id.clone())
    }

    /// Checks whether `seat` can accept work (`vespertin/specs/uas/0027` `liveness`).
    ///
    /// # Errors
    ///
    /// Returns [`SeatSubstrateError::Recoverable`] if `seat` is unknown.
    pub fn liveness(&self, seat: &SeatId) -> Result<SeatLiveness, SeatSubstrateError> {
        let slot = self.find_slot_by_seat(seat)?;
        Ok(slot.liveness.clone())
    }

    /// Marks a seat dead or alive (for fault-injection and watchdog recovery).
    ///
    /// # Errors
    ///
    /// Returns [`SeatSubstrateError::Recoverable`] if `seat` is unknown.
    pub fn set_liveness(
        &mut self,
        seat: &SeatId,
        liveness: SeatLiveness,
    ) -> Result<(), SeatSubstrateError> {
        let slot = self.find_slot_by_seat_mut(seat)?;
        slot.liveness = liveness;
        Ok(())
    }

    /// Submits a [`SeatJobSpec`] to `seat` (`vespertin/specs/uas/0027` `submit`, `REQ-ORCH-SEAT-01`).
    ///
    /// - **Idempotent on `job_id`:** repeating the same `(seat, job_id)` is a no-op (`REQ-0027-04`).
    /// - **Workspace/Grant Repo Validation:** permanent error if `target_repo` is not allowed by
    ///   the seat's Biscuit mandate or `AuthorityGrant` (`REQ-0027-03`, `REQ-0027-10`, `REQ-ORCH-AUTUMN-01`).
    /// - **Wheelhorse Gate:** evaluates `WheelhorseGate::evaluate` over the job's touched paths
    ///   and `CompensationPlan`; refuses execution on tripwire hits, policy blocks, or unapproved
    ///   fallbacks (`REQ-ORCH-WH-01..04`).
    ///
    /// # Errors
    ///
    /// Returns [`SeatSubstrateError::Permanent`] on payload/repo/mandate/tripwire violations, or
    /// [`SeatSubstrateError::Recoverable`] if the seat is dead or waiting on human approval.
    pub fn submit(
        &mut self,
        seat: &SeatId,
        job: &SeatJobSpec,
        now_ms: u64,
    ) -> Result<(), SeatSubstrateError> {
        if job.body.trim().is_empty() {
            return Err(SeatSubstrateError::Permanent(
                "job body is empty".to_owned(),
            ));
        }
        let key = (seat.clone(), job.job_id.clone());
        if self.jobs.contains_key(&key) {
            return Ok(());
        }

        let slot = self.find_slot_by_seat(seat)?.clone();
        if let SeatLiveness::Dead(why) = &slot.liveness {
            return Err(SeatSubstrateError::Recoverable(format!(
                "seat `{seat}` is dead: {why}"
            )));
        }

        // Re-verify Biscuit mandate expiration and sandbox floor at submission time.
        let compiled_mandate = slot
            .mandate_claims
            .compile_to_harvest_mandate(slot.sandbox_class, now_ms)
            .map_err(|e| SeatSubstrateError::Permanent(e.to_string()))?;

        // Check target_repo against Biscuit `allowed_repo_prefixes`.
        if let Some(repo) = &job.target_repo {
            let repo_permitted = slot
                .mandate_claims
                .allowed_repo_prefixes
                .iter()
                .any(|prefix| repo == prefix || repo.starts_with(prefix.as_str()));
            if !repo_permitted {
                return Err(SeatSubstrateError::Permanent(format!(
                    "unknown_target_repo: `{repo}` is not permitted by Biscuit mandate for seat `{seat}`"
                )));
            }
        }

        // Check Autumn-Web `AuthorityGrant` (`EffectKind::Write` on `target_repo`).
        let mut effects = Vec::new();
        if let Some(repo) = &job.target_repo {
            effects.push(AgentEffect {
                kind: EffectKind::Write,
                target: repo.clone(),
                provenance: EffectProvenance::TypeResolved,
            });
        }
        let action_kind = if job.compensation_plan.is_some() {
            WheelhorseActionKind::DraftPullRequest
        } else {
            WheelhorseActionKind::PoolBranchPush
        };
        let reversibility = action_kind.classify();
        slot.authority_grant
            .check_effects(&effects, reversibility, slot.calls_in_current_minute + 1)
            .map_err(|e| SeatSubstrateError::Permanent(e.to_string()))?;

        // Evaluate Wheelhorse 4-stage gate (`classify -> capture_plan -> validate -> decide`).
        let descriptor = WheelhorseActionDescriptor {
            actor: compiled_mandate.holder().to_owned(),
            capability: format!("pool:{}", job.subject.default_pool()),
            subject: job.subject,
            target_repo: job.target_repo.clone(),
            kind: action_kind,
            touched_paths: job.allowed_paths.clone(),
            sealed_action: None,
        };
        let gate_record =
            self.wheelhorse
                .evaluate(&descriptor, job.compensation_plan.as_ref(), None, now_ms);

        match &gate_record.verdict {
            WheelhorseGateDecision::AutoCommit(_)
            | WheelhorseGateDecision::AutoCommitWithNotice(_) => {}
            WheelhorseGateDecision::BlockedByTripwire(why)
            | WheelhorseGateDecision::BlockedByPolicy(why) => {
                let err_msg = why.clone();
                self.gate_records.insert(key, gate_record);
                return Err(SeatSubstrateError::Permanent(err_msg));
            }
            WheelhorseGateDecision::RouteToApproval(why) => {
                let err_msg = why.clone();
                self.gate_records.insert(key, gate_record);
                return Err(SeatSubstrateError::Recoverable(err_msg));
            }
        }

        let slot_mut = self.find_slot_by_seat_mut(seat)?;
        slot_mut.calls_in_current_minute += 1;

        let queue_item_id = job.queue_item_id();
        let head_branch = job.head_branch();
        let cargo_argv = job.throttled_cargo_check_argv();

        self.gate_records.insert(key.clone(), gate_record);
        self.jobs.insert(
            key,
            SeatJobState::Running {
                queue_item_id,
                head_branch,
                cargo_argv,
            },
        );
        Ok(())
    }

    /// Polls the current [`SeatJobState`] of `(seat, job_id)` (`vespertin/specs/uas/0027` `poll`).
    ///
    /// # Errors
    ///
    /// Returns [`SeatSubstrateError::Recoverable`] if `seat` or `job_id` is unknown.
    pub fn poll(&self, seat: &SeatId, job_id: &str) -> Result<SeatJobState, SeatSubstrateError> {
        let slot = self.find_slot_by_seat(seat)?;
        if let SeatLiveness::Dead(why) = &slot.liveness {
            return Ok(SeatJobState::Failed(format!("seat died: {why}")));
        }
        let key = (seat.clone(), job_id.to_owned());
        self.jobs.get(&key).cloned().ok_or_else(|| {
            SeatSubstrateError::Recoverable(format!("job `{job_id}` not found on seat `{seat}`"))
        })
    }

    /// Marks a running job on `seat` as completed and records its cryptographic [`SeatReceipt`].
    ///
    /// # Errors
    ///
    /// Returns [`SeatSubstrateError::Recoverable`] if the job is not found.
    pub fn complete_job(
        &mut self,
        seat: &SeatId,
        job_id: &str,
        closure: impl Into<String>,
        transcript_sha256: Option<String>,
    ) -> Result<SeatReceipt, SeatSubstrateError> {
        let key = (seat.clone(), job_id.to_owned());
        let gate_record = self.gate_records.get(&key).cloned().ok_or_else(|| {
            SeatSubstrateError::Recoverable(format!(
                "missing wheelhorse gate record for job `{job_id}` on seat `{seat}`"
            ))
        })?;
        let queue_item_id = derive_queue_item_id(job_id);
        let head_branch = format!("pool/{queue_item_id}");
        let receipt = SeatReceipt {
            job_id: job_id.to_owned(),
            queue_item_id,
            head_branch,
            closure: closure.into(),
            transcript_sha256,
            wheelhorse_record: gate_record,
            ghost_binding: None,
        };
        self.jobs.insert(key, SeatJobState::Done(receipt.clone()));
        Ok(receipt)
    }

    /// Cancels a running job on `seat` (`vespertin/specs/uas/0027` `cancel`).
    ///
    /// # Errors
    ///
    /// Returns [`SeatSubstrateError::Recoverable`] if `seat` is unknown.
    pub fn cancel(&mut self, seat: &SeatId, job_id: &str) -> Result<(), SeatSubstrateError> {
        let _ = self.find_slot_by_seat(seat)?;
        let key = (seat.clone(), job_id.to_owned());
        self.jobs.insert(
            key,
            SeatJobState::Failed("cancelled by escapement-runner".to_owned()),
        );
        Ok(())
    }

    /// Releases a seat back to the pool (`vespertin/specs/uas/0027` `release`).
    ///
    /// # Errors
    ///
    /// Returns [`SeatSubstrateError::Recoverable`] if `seat` is unknown.
    pub fn release(&mut self, seat: &SeatId) -> Result<(), SeatSubstrateError> {
        let slot = self.find_slot_by_seat_mut(seat)?;
        slot.calls_in_current_minute = 0;
        Ok(())
    }

    /// Renders an RFC 7231 `Accept: text/markdown` AEO status document (`autumn-web` bridge, `REQ-ORCH-AUTUMN-02`).
    #[must_use]
    pub fn render_aeo_markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "# Estate Orchestration Status (Wheelhorse + Autumn-Web + Littleguy + Escapement)\n"
        );
        let _ = writeln!(out, "## 1. Canonical Subject Topology (9 Cells)\n");
        let _ = writeln!(
            out,
            "| Subject | Callsign | Domain | Gate Policy | Sandbox Floor | Default Pool |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|");
        for subject in EstateSubject::ALL {
            let domain = subject.domain();
            let _ = writeln!(
                out,
                "| `{}` | **{}** | `{}` | `{}` | `{}` | `{}` |",
                subject.slug(),
                subject.callsign(),
                domain.as_str(),
                domain.gate_policy().as_str(),
                subject.default_sandbox_floor().as_str(),
                subject.default_pool(),
            );
        }

        let _ = writeln!(
            out,
            "\n## 2. Active Littleguy Seat Slots ({})\n",
            self.slots.len()
        );
        for slot in self.slots.values() {
            let _ = writeln!(
                out,
                "- **`{}`** (`subject={}`, `sandbox={}`, `grant={}`, `liveness={:?}`)",
                slot.seat_id,
                slot.subject,
                slot.sandbox_class,
                slot.authority_grant.name,
                slot.liveness
            );
        }

        let _ = writeln!(
            out,
            "\n## 3. Wheelhorse Gate Records ({})\n",
            self.gate_records.len()
        );
        for ((seat, job_id), rec) in &self.gate_records {
            let _ = writeln!(
                out,
                "- **`{seat}` / `{job_id}`**: `decision={}` (`class={}`, `policy={}`) — {}",
                rec.decision,
                rec.reversibility_class,
                rec.domain_gate_policy,
                rec.verdict.reason()
            );
        }
        out
    }

    /// Builds the deterministic `autumn-web` `WebMcpManifest` (`/mcp` tool catalog, `REQ-ORCH-AUTUMN-02`).
    #[must_use]
    pub fn webmcp_manifest(&self) -> WebMcpManifest {
        WebMcpManifest {
            site_name: "cerebral-estate-orchestrator".to_owned(),
            protocol_revision: "2026-07-28".to_owned(),
            acyclic_router_snapshot: true,
            tools: vec![
                WebMcpToolDescriptor {
                    name: "orchestration_status_markdown".to_owned(),
                    description: "Render RFC 7231 Accept: text/markdown snapshot of all 9 subject cells, Littleguy seats, and Wheelhorse gate records".to_owned(),
                    route_target: "GET /status".to_owned(),
                    read_only: true,
                },
                WebMcpToolDescriptor {
                    name: "wheelhorse_gate_evaluate".to_owned(),
                    description: "Evaluate the 4-stage Wheelhorse reversibility and canary tripwire pipeline on an ActionDescriptor".to_owned(),
                    route_target: "POST /v1/wheelhorse/evaluate".to_owned(),
                    read_only: true,
                },
                WebMcpToolDescriptor {
                    name: "seat_submit_pool_job".to_owned(),
                    description: "Submit an idempotent PoolWork / TaskSpec job to a Littleguy seat under Biscuit + AuthorityGrant governance".to_owned(),
                    route_target: "POST /v1/seats/submit".to_owned(),
                    read_only: false,
                },
                WebMcpToolDescriptor {
                    name: "escapement_pool_claim_and_ack".to_owned(),
                    description: "Claim and confirm a typed EscapementTaskSpec lease with two-phase lease_epoch fencing".to_owned(),
                    route_target: "POST /v1/escapement/claim".to_owned(),
                    read_only: false,
                },
            ],
        }
    }

    fn find_slot_by_seat(&self, seat: &SeatId) -> Result<&LittleguySeatSlot, SeatSubstrateError> {
        self.slots
            .values()
            .find(|s| &s.seat_id == seat)
            .ok_or_else(|| SeatSubstrateError::Recoverable(format!("unknown seat `{seat}`")))
    }

    fn find_slot_by_seat_mut(
        &mut self,
        seat: &SeatId,
    ) -> Result<&mut LittleguySeatSlot, SeatSubstrateError> {
        self.slots
            .values_mut()
            .find(|s| &s.seat_id == seat)
            .ok_or_else(|| SeatSubstrateError::Recoverable(format!("unknown seat `{seat}`")))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. Pillar 4: Escapement Typed Pool Dispatcher & Two-Phase Lease Loop
//    (REQ-ORCH-ESC-01..03, vespertin/specs/0136 & specs/uas/0027)
// ─────────────────────────────────────────────────────────────────────────────

/// Execution pool class separating build, formal verification, and GPU runners (`REQ-ORCH-ESC-01`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscapementPoolClass {
    /// Standard compilation and test runners (`service-pool-build-1..12` on Cygnus or `ceres` slots).
    Build,
    /// Formal verification, Lean/Kani/TLA+, and security containment runners.
    Verify,
    /// GPU training and evaluation runners (`scorcher` / `burn` / `autoresearcher`).
    Gpu,
}

impl EscapementPoolClass {
    /// Canonical `snake_case` name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Verify => "verify",
            Self::Gpu => "gpu",
        }
    }
}

impl fmt::Display for EscapementPoolClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Closed typed task contract replacing untyped free-form prompt strings (`vespertin/specs/0136`, `REQ-ORCH-ESC-01`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscapementTaskKind {
    /// Compile and run workspace tests (`cargo test -j 2 --workspace`).
    BuildAndTest,
    /// Run formal verification or `LeanSpec` checks (`make spec-check` / `cargo kani`).
    FormalVerify,
    /// Validate documentation and site structure (`python3 scripts/check_site.py`).
    DocCheck,
    /// Execute a bounded custom command within `allowed_paths`.
    CustomCommand,
}

impl EscapementTaskKind {
    /// Canonical `snake_case` name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BuildAndTest => "build_and_test",
            Self::FormalVerify => "formal_verify",
            Self::DocCheck => "doc_check",
            Self::CustomCommand => "custom_command",
        }
    }
}

impl fmt::Display for EscapementTaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed pool task specification (`vespertin/specs/0136` `TaskSpec`, `REQ-ORCH-ESC-01`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscapementTaskSpec {
    /// Unique task identifier (`act_<hex>` or UUID).
    pub task_id: String,
    /// Owning subject cell (`delivery`, `runtime-kernel`, etc.).
    pub subject: EstateSubject,
    /// Target runner pool class (`Build`, `Verify`, `Gpu`).
    pub pool_class: EscapementPoolClass,
    /// Target repository (`<owner>/<name>`).
    pub target_repo: String,
    /// 40-hex base commit SHA (`origin/main` head at dispatch time).
    pub base_sha: String,
    /// Typed task contract kind.
    pub kind: EscapementTaskKind,
    /// Rendered task brief (`escapement_core::pool::render_brief`).
    pub brief: String,
    /// Allowed file paths/globs the task may touch.
    pub allowed_paths: Vec<String>,
    /// Required check IDs (`<project>:<check>`) before Judge approval.
    pub check_ids: Vec<String>,
    /// Pre-computed compensation plan (`head_sha` + `diff_blake3`).
    pub compensation_plan: Option<CompensationPlan>,
}

impl EscapementTaskSpec {
    /// Converts this [`EscapementTaskSpec`] into a [`SeatJobSpec`] for [`LittleguySeatSubstrate::submit`].
    #[must_use]
    pub fn to_seat_job_spec(&self) -> SeatJobSpec {
        SeatJobSpec {
            job_id: self.task_id.clone(),
            body: self.brief.clone(),
            target_repo: Some(self.target_repo.clone()),
            subject: self.subject,
            allowed_paths: self.allowed_paths.clone(),
            check_ids: self.check_ids.clone(),
            compensation_plan: self.compensation_plan.clone(),
        }
    }
}

/// Two-phase lease and execution state for an Escapement pool task (`REQ-ORCH-ESC-02..03`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscapementLeaseState {
    /// Waiting in the pool queue (`FOR UPDATE SKIP LOCKED` candidate).
    Queued,
    /// Claimed by a runner (`claim_next`), awaiting explicit `confirm_claim` ack before execution.
    ClaimPendingAck,
    /// Runner acknowledged the lease (`ClaimConfirmed`); eligible for `dispatch_claimed`.
    ClaimConfirmed,
    /// Dispatched to a [`LittleguySeatSubstrate`] seat and actively running.
    Running,
    /// Completed and verified by the Escapement Judge.
    Succeeded,
    /// Failed execution or rejected by gate/judge.
    Failed,
    /// Cancelled by operator or runner.
    Cancelled,
}

/// Tracked state of a task inside [`EscapementPoolDispatcher`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscapementPoolTask {
    /// Immutable task specification.
    pub spec: EscapementTaskSpec,
    /// Current lease state.
    pub state: EscapementLeaseState,
    /// Runner identifier holding the current lease (`service-pool-build-7`, `openrig-ceres-0`, etc.).
    pub claimed_by_runner: Option<String>,
    /// Monotonically increasing fencing token incremented on every claim (`REQ-ORCH-ESC-02`).
    pub lease_epoch: u64,
    /// Seat currently executing the task, once `dispatch_claimed` succeeds.
    pub assigned_seat: Option<SeatId>,
    /// Final cryptographic receipt once settled.
    pub receipt: Option<SeatReceipt>,
}

/// Pillar 4 `EscapementPoolDispatcher` implementing typed pool queueing and the
/// two-phase claim-and-ack protocol (`REQ-ORCH-ESC-01..03`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscapementPoolDispatcher {
    tasks: BTreeMap<String, EscapementPoolTask>,
    next_epoch: u64,
}

impl EscapementPoolDispatcher {
    /// Creates an empty [`EscapementPoolDispatcher`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tasks: BTreeMap::new(),
            next_epoch: 0,
        }
    }

    /// Enqueues a typed [`EscapementTaskSpec`] in `EscapementLeaseState::Queued` (idempotent on `task_id`).
    pub fn enqueue(&mut self, spec: EscapementTaskSpec) {
        let task_id = spec.task_id.clone();
        self.tasks.entry(task_id).or_insert(EscapementPoolTask {
            spec,
            state: EscapementLeaseState::Queued,
            claimed_by_runner: None,
            lease_epoch: 0,
            assigned_seat: None,
            receipt: None,
        });
    }

    /// Looks up a task by `task_id`.
    #[must_use]
    pub fn get_task(&self, task_id: &str) -> Option<&EscapementPoolTask> {
        self.tasks.get(task_id)
    }

    /// Counts how many tasks are currently active (`ClaimPendingAck`, `ClaimConfirmed`, or `Running`)
    /// for `runner_id`. When this returns `0`, the runner heartbeat transitions peer status to `ready`.
    #[must_use]
    pub fn active_tasks_for_runner(&self, runner_id: &str) -> usize {
        self.tasks
            .values()
            .filter(|t| {
                t.claimed_by_runner.as_deref() == Some(runner_id)
                    && matches!(
                        t.state,
                        EscapementLeaseState::ClaimPendingAck
                            | EscapementLeaseState::ClaimConfirmed
                            | EscapementLeaseState::Running
                    )
            })
            .count()
    }

    /// Phase 1 of two-phase claim (`FOR UPDATE SKIP LOCKED` equivalent, `REQ-ORCH-ESC-02`):
    /// claims the next `Queued` task matching `pool_class` (and optional `subject_filter`),
    /// transitioning it to `ClaimPendingAck` with a fresh `lease_epoch`.
    pub fn claim_next(
        &mut self,
        pool_class: EscapementPoolClass,
        subject_filter: Option<EstateSubject>,
        runner_id: &str,
    ) -> Option<(String, u64)> {
        let next_id = self
            .tasks
            .values()
            .find(|t| {
                t.state == EscapementLeaseState::Queued
                    && t.spec.pool_class == pool_class
                    && subject_filter.is_none_or(|s| t.spec.subject == s)
            })
            .map(|t| t.spec.task_id.clone())?;

        self.next_epoch += 1;
        let epoch = self.next_epoch;
        if let Some(task) = self.tasks.get_mut(&next_id) {
            task.state = EscapementLeaseState::ClaimPendingAck;
            task.claimed_by_runner = Some(runner_id.to_owned());
            task.lease_epoch = epoch;
        }
        Some((next_id, epoch))
    }

    /// Phase 2 of two-phase claim (`REQ-ORCH-ESC-02`):
    /// runner confirms receipt of the lease (`ClaimPendingAck -> ClaimConfirmed`) before any command executes.
    ///
    /// # Errors
    ///
    /// Returns [`SeatSubstrateError::Recoverable`] if `task_id` is unknown, owned by another runner,
    /// has a mismatched `lease_epoch`, or is not in `ClaimPendingAck`.
    pub fn confirm_claim(
        &mut self,
        task_id: &str,
        runner_id: &str,
        lease_epoch: u64,
    ) -> Result<(), SeatSubstrateError> {
        let task = self.tasks.get_mut(task_id).ok_or_else(|| {
            SeatSubstrateError::Recoverable(format!("unknown escapement task `{task_id}`"))
        })?;
        if task.claimed_by_runner.as_deref() != Some(runner_id) || task.lease_epoch != lease_epoch {
            return Err(SeatSubstrateError::Recoverable(format!(
                "stale or mismatched lease on `{task_id}` (runner={runner_id}, epoch={lease_epoch})"
            )));
        }
        if task.state != EscapementLeaseState::ClaimPendingAck {
            return Err(SeatSubstrateError::Recoverable(format!(
                "task `{task_id}` cannot confirm_claim from state `{:?}`",
                task.state
            )));
        }
        task.state = EscapementLeaseState::ClaimConfirmed;
        Ok(())
    }

    /// Requeues a `ClaimPendingAck` task whose runner failed to send `confirm_claim` before timeout (`REQ-ORCH-ESC-03`).
    ///
    /// # Errors
    ///
    /// Returns [`SeatSubstrateError::Recoverable`] if `task_id` is unknown or not in `ClaimPendingAck`.
    pub fn requeue_unconfirmed(&mut self, task_id: &str) -> Result<(), SeatSubstrateError> {
        let task = self.tasks.get_mut(task_id).ok_or_else(|| {
            SeatSubstrateError::Recoverable(format!("unknown escapement task `{task_id}`"))
        })?;
        if task.state != EscapementLeaseState::ClaimPendingAck {
            return Err(SeatSubstrateError::Recoverable(format!(
                "only ClaimPendingAck tasks can be requeued without side effects, got `{:?}`",
                task.state
            )));
        }
        task.state = EscapementLeaseState::Queued;
        task.claimed_by_runner = None;
        Ok(())
    }

    /// Dispatches a `ClaimConfirmed` task to `seat` on `substrate` (`REQ-ORCH-ESC-02`).
    ///
    /// Refuses execution if the lease is still `ClaimPendingAck` (unconfirmed) or if `lease_epoch` is stale.
    ///
    /// # Errors
    ///
    /// Returns [`SeatSubstrateError`] if the lease is not confirmed or if `substrate.submit` rejects the job.
    pub fn dispatch_claimed(
        &mut self,
        task_id: &str,
        runner_id: &str,
        lease_epoch: u64,
        seat: &SeatId,
        substrate: &mut LittleguySeatSubstrate,
        now_ms: u64,
    ) -> Result<(), SeatSubstrateError> {
        let job_spec = {
            let task = self.tasks.get(task_id).ok_or_else(|| {
                SeatSubstrateError::Recoverable(format!("unknown escapement task `{task_id}`"))
            })?;
            if task.claimed_by_runner.as_deref() != Some(runner_id)
                || task.lease_epoch != lease_epoch
            {
                return Err(SeatSubstrateError::Recoverable(format!(
                    "fenced lease mismatch on `{task_id}` (runner={runner_id}, epoch={lease_epoch})"
                )));
            }
            if task.state != EscapementLeaseState::ClaimConfirmed {
                return Err(SeatSubstrateError::Recoverable(format!(
                    "two-phase lease violation: task `{task_id}` is `{:?}`, must be ClaimConfirmed before execution",
                    task.state
                )));
            }
            task.spec.to_seat_job_spec()
        };

        substrate.submit(seat, &job_spec, now_ms)?;
        if let Some(task) = self.tasks.get_mut(task_id) {
            task.state = EscapementLeaseState::Running;
            task.assigned_seat = Some(seat.clone());
        }
        Ok(())
    }

    /// Settles a `Running` task on `substrate` and records its [`SeatReceipt`].
    ///
    /// # Errors
    ///
    /// Returns [`SeatSubstrateError::Recoverable`] if the task is not running or `complete_job` fails.
    pub fn settle_task(
        &mut self,
        task_id: &str,
        substrate: &mut LittleguySeatSubstrate,
        succeeded: bool,
        closure: impl Into<String>,
        transcript_sha256: Option<String>,
    ) -> Result<SeatReceipt, SeatSubstrateError> {
        let seat = {
            let task = self.tasks.get(task_id).ok_or_else(|| {
                SeatSubstrateError::Recoverable(format!("unknown escapement task `{task_id}`"))
            })?;
            if task.state != EscapementLeaseState::Running {
                return Err(SeatSubstrateError::Recoverable(format!(
                    "task `{task_id}` is not Running (`{:?}`)",
                    task.state
                )));
            }
            task.assigned_seat.clone().ok_or_else(|| {
                SeatSubstrateError::Recoverable(format!("task `{task_id}` has no assigned seat"))
            })?
        };

        let receipt = substrate.complete_job(&seat, task_id, closure, transcript_sha256)?;
        if let Some(task) = self.tasks.get_mut(task_id) {
            task.state = if succeeded {
                EscapementLeaseState::Succeeded
            } else {
                EscapementLeaseState::Failed
            };
            task.receipt = Some(receipt.clone());
        }
        Ok(receipt)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn is_lowercase_hex_len(s: &str, expected_len: usize) -> bool {
    s.len() == expected_len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn matches_prefix_or_exact(allowlist: &BTreeSet<String>, target: &str) -> bool {
    allowlist
        .iter()
        .any(|entry| target == entry || target.starts_with(entry.as_str()))
}

/// Derives a queue item id following `vespertin/specs/uas/0027` D4 (`REQ-0027-04`).
///
/// Uses `job_id` directly when it matches `[A-Za-z0-9._-]{1,128}`; otherwise returns
/// `qitem-uas-<24 lowercase hex>` using three 64-bit FNV-1a lanes over `job_id`.
#[must_use]
pub fn derive_queue_item_id(job_id: &str) -> String {
    if !job_id.is_empty()
        && job_id.len() <= 128
        && job_id
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return job_id.to_owned();
    }
    let h1 = fnv1a64(job_id.as_bytes(), 0xcbf2_9ce4_8422_2325);
    let h2 = fnv1a64(job_id.as_bytes(), 0x8422_2325_cbf2_9ce4);
    let h3 = fnv1a64(job_id.as_bytes(), 0x1000_0000_01b3_9ce4);
    format!("qitem-uas-{h1:016x}{:08x}", (h2 ^ h3) & 0xffff_ffff)
}

fn fnv1a64(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = seed;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests (Unit + Mutation Controls for REQ-ORCH-TOPO, WH, AUTUMN, SEAT, ESC)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_claims(repo_prefix: &str, floor: SandboxClass) -> BiscuitMandateClaims {
        BiscuitMandateClaims {
            spiffe_id: "spiffe://unsigned.gg/conclave/cygnus/elf/worker-01".to_owned(),
            acting_for_principal: "ctodie".to_owned(),
            min_sandbox_class: floor,
            allowed_repo_prefixes: vec![repo_prefix.to_owned()],
            max_reversibility: ReversibilityClass::Compensatable,
            expires_at_ms: 2_000_000_000_000,
        }
    }

    fn valid_compensation_plan() -> CompensationPlan {
        CompensationPlan {
            head_sha: "a".repeat(40),
            diff_blake3: "b".repeat(64),
            diff_bytes: 512,
            undo_argv: vec![
                "git".to_owned(),
                "revert".to_owned(),
                "--no-edit".to_owned(),
                "a".repeat(40),
            ],
            undo_window_secs: 3600,
        }
    }

    #[test]
    fn req_orch_topo_01_02_all_nine_subjects_and_red_gated_repos_resolve() {
        assert_eq!(EstateSubject::ALL.len(), 9);
        assert_eq!(EstateSubject::parse("  Comms "), Some(EstateSubject::Comms));
        assert_eq!(EstateSubject::Comms.callsign(), "alpheratz");
        assert_eq!(
            EstateSubject::Delivery.domain().gate_policy(),
            DomainGatePolicy::RedGated
        );
        assert_eq!(
            EstateSubject::RuntimeKernel.domain().gate_policy(),
            DomainGatePolicy::LandOnGreen
        );

        // Spec 0033 / C1c: autoresearcher and vilicus are RedGated even when touched by a Research/Development subject.
        assert_eq!(
            resolve_domain_gate_policy(
                EstateSubject::AssuranceResearch,
                Some("unsigned-gg/autoresearcher")
            ),
            DomainGatePolicy::RedGated
        );
        assert_eq!(
            resolve_domain_gate_policy(
                EstateSubject::RuntimeKernel,
                Some("cerebral-work/vespertin")
            ),
            DomainGatePolicy::LandOnGreen
        );
    }

    #[test]
    fn req_orch_wh_01_04_wheelhorse_pipeline_and_soma_invariant() {
        let gate = WheelhorseGate::new();

        let desc = WheelhorseActionDescriptor {
            actor: "elf-cygnus-worker-01".to_owned(),
            capability: "pool:vespertin-pool".to_owned(),
            subject: EstateSubject::RuntimeKernel,
            target_repo: Some("cerebral-work/vespertin".to_owned()),
            kind: WheelhorseActionKind::DraftPullRequest,
            touched_paths: vec!["escapement/crates/escapement-runner/src/main.rs".to_owned()],
            sealed_action: None,
        };

        // 1. Valid compensation plan + LandOnGreen -> AutoCommitWithNotice (`pass`).
        let plan = valid_compensation_plan();
        let rec = gate.evaluate(&desc, Some(&plan), None, 1_700_000_000_000);
        assert_eq!(rec.decision, "pass");
        assert!(rec.verdict.permits_autonomous_execution());
        assert!(rec.plan_validated);

        // 2. Missing compensation plan on Compensatable action -> falls back to approval (`fallback`).
        let rec_no_plan = gate.evaluate(&desc, None, None, 1_700_000_000_000);
        assert_eq!(rec_no_plan.decision, "fallback");
        assert!(!rec_no_plan.verdict.permits_autonomous_execution());

        // 3. Soma Gate Invariant (`REQ-ORCH-WH-03`): RedGated repo (`unsigned-gg/unsigned-paas`)
        //    never auto-commits even with a valid compensation plan.
        let mut red_desc = desc.clone();
        red_desc.target_repo = Some("unsigned-gg/unsigned-paas".to_owned());
        let rec_soma = gate.evaluate(&red_desc, Some(&plan), None, 1_700_000_000_000);
        assert_eq!(rec_soma.decision, "fallback");
        assert!(rec_soma.verdict.reason().contains("soma_gate"));

        // 4. Heimdal human deny overrides autonomous pass (`REQ-0135-04`).
        let rec_denied = gate.evaluate(&desc, Some(&plan), Some(false), 1_700_000_000_000);
        assert_eq!(rec_denied.decision, "blocked");
    }

    #[test]
    fn req_orch_wh_02_canary_tripwire_blocks_immediately() {
        let gate = WheelhorseGate::new();

        let desc = WheelhorseActionDescriptor {
            actor: "elf-cygnus-worker-01".to_owned(),
            capability: "pool:vespertin-pool".to_owned(),
            subject: EstateSubject::RuntimeKernel,
            target_repo: Some("cerebral-work/vespertin".to_owned()),
            kind: WheelhorseActionKind::PoolBranchPush,
            touched_paths: vec![".ssh/authorized_keys".to_owned()],
            sealed_action: None,
        };

        let rec = gate.evaluate(&desc, None, None, 1_700_000_000_000);
        assert_eq!(rec.decision, "blocked");
        assert!(matches!(
            rec.verdict,
            WheelhorseGateDecision::BlockedByTripwire(_)
        ));
    }

    #[test]
    fn req_orch_autumn_01_authority_grant_checks_effects_rate_and_reversibility() {
        let grant = AuthorityGrant::pool_worker_default(
            "VespertinPoolGrant",
            vec!["cerebral-work/vespertin".to_owned()],
            vec!["vespertin-pool".to_owned()],
        );

        let ok_effects = vec![AgentEffect {
            kind: EffectKind::Write,
            target: "cerebral-work/vespertin".to_owned(),
            provenance: EffectProvenance::TypeResolved,
        }];
        assert!(
            grant
                .check_effects(&ok_effects, ReversibilityClass::Compensatable, 5)
                .is_ok()
        );

        // UnboundedWrite is rejected.
        let unbounded = vec![AgentEffect {
            kind: EffectKind::UnboundedWrite,
            target: "cerebral-work/vespertin".to_owned(),
            provenance: EffectProvenance::Syntactic,
        }];
        assert!(matches!(
            grant.check_effects(&unbounded, ReversibilityClass::Compensatable, 1),
            Err(AuthorityViolation::UnboundedWriteForbidden { .. })
        ));

        // Rate limit exceeded.
        assert!(matches!(
            grant.check_effects(&ok_effects, ReversibilityClass::Compensatable, 31),
            Err(AuthorityViolation::RateLimitExceeded { .. })
        ));

        // Irreversible exceeds max_reversibility (Compensatable).
        assert!(matches!(
            grant.check_effects(&ok_effects, ReversibilityClass::Irreversible, 1),
            Err(AuthorityViolation::ReversibilityExceeded { .. })
        ));
    }

    #[test]
    fn req_orch_seat_01_littleguy_seat_substrate_end_to_end_lifecycle() {
        let wh = WheelhorseGate::new();
        let mut substrate = LittleguySeatSubstrate::new(wh);

        let claims = sample_claims("cerebral-work/vespertin", SandboxClass::Firecracker);
        let grant = AuthorityGrant::pool_worker_default(
            "VespertinPoolGrant",
            vec!["cerebral-work/vespertin".to_owned()],
            vec!["vespertin-pool".to_owned()],
        );

        // Register slot in RuntimeKernel (floor = Gvisor, actual = Firecracker >= Gvisor).
        let seat = substrate
            .register_slot(
                "slot-0",
                "/scratch/slots/vespertin-0",
                EstateSubject::RuntimeKernel,
                SandboxClass::Firecracker,
                claims,
                grant,
                1_700_000_000_000,
            )
            .unwrap();

        assert_eq!(
            substrate
                .ensure_seat("slot-0", Path::new("/scratch/slots/vespertin-0"))
                .unwrap(),
            seat
        );
        assert_eq!(substrate.liveness(&seat).unwrap(), SeatLiveness::Alive);

        let job = SeatJobSpec {
            job_id: "act_18cf144b5df69451006".to_owned(),
            body: "Implement REQ-0136-10 in escapement-runner".to_owned(),
            target_repo: Some("cerebral-work/vespertin".to_owned()),
            subject: EstateSubject::RuntimeKernel,
            allowed_paths: vec!["escapement/crates/escapement-runner/src/main.rs".to_owned()],
            check_ids: vec!["escapement:test".to_owned()],
            compensation_plan: Some(valid_compensation_plan()),
        };

        // Submit is idempotent on job_id.
        substrate.submit(&seat, &job, 1_700_000_000_000).unwrap();
        substrate.submit(&seat, &job, 1_700_000_000_000).unwrap();

        match substrate.poll(&seat, &job.job_id).unwrap() {
            SeatJobState::Running {
                queue_item_id,
                head_branch,
                cargo_argv,
            } => {
                assert_eq!(queue_item_id, "act_18cf144b5df69451006");
                assert_eq!(head_branch, "pool/act_18cf144b5df69451006");
                assert_eq!(cargo_argv, vec!["cargo", "test", "-j", "2", "--workspace"]);
            }
            other => panic!("expected Running state, got {other:?}"),
        }

        // Complete job and verify receipt + AEO markdown + WebMCP manifest.
        let receipt = substrate
            .complete_job(&seat, &job.job_id, "judge_succeeded", Some("c".repeat(64)))
            .unwrap();
        assert_eq!(receipt.head_branch, "pool/act_18cf144b5df69451006");
        assert_eq!(receipt.wheelhorse_record.decision, "pass");

        // REQ-LG-GHOST-09: the receipt carries a Ghost binding and round-trips it.
        assert!(receipt.ghost_binding.is_none());
        let binding: crate::ghost_binding::GhostBinding = serde_json::from_str(
            r#"{"ghost_id":"sadr","manifestation":"agent","mitc_principal":"ctodie","spiffe_seat":"spiffe://unsigned.gg/agent/platform/sadr","molt_epoch":3}"#,
        )
        .unwrap();
        let mut ghosted = receipt.clone();
        ghosted.ghost_binding = Some(binding);
        let json = serde_json::to_string(&ghosted).unwrap();
        let back: SeatReceipt = serde_json::from_str(&json).unwrap();
        let carried = back.ghost_binding.as_ref().unwrap();
        assert_eq!((carried.ghost_id(), carried.molt_epoch()), ("sadr", 3));
        assert!(
            !serde_json::to_string(&receipt)
                .unwrap()
                .contains("ghost_binding")
        );

        let md = substrate.render_aeo_markdown();
        assert!(md.contains("alpheratz"));
        assert!(md.contains("act_18cf144b5df69451006"));

        let mcp = substrate.webmcp_manifest();
        assert!(mcp.acyclic_router_snapshot);
        assert_eq!(mcp.tools.len(), 4);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn req_orch_esc_01_03_escapement_two_phase_claim_and_ack_lifecycle() {
        let wh = WheelhorseGate::new();
        let mut substrate = LittleguySeatSubstrate::new(wh);
        let claims = sample_claims("cerebral-work/vespertin", SandboxClass::Kata);
        let grant = AuthorityGrant::pool_worker_default(
            "CygnusBuildPoolGrant",
            vec!["cerebral-work/vespertin".to_owned()],
            vec!["vespertin-pool".to_owned()],
        );
        let seat = substrate
            .register_slot(
                "build-slot-7",
                "/scratch/slots/build-7",
                EstateSubject::RuntimeKernel,
                SandboxClass::Kata,
                claims,
                grant,
                1_700_000_000_000,
            )
            .unwrap();

        let mut dispatcher = EscapementPoolDispatcher::new();
        dispatcher.enqueue(EscapementTaskSpec {
            task_id: "act_escapement_0001".to_owned(),
            subject: EstateSubject::RuntimeKernel,
            pool_class: EscapementPoolClass::Build,
            target_repo: "cerebral-work/vespertin".to_owned(),
            base_sha: "a".repeat(40),
            kind: EscapementTaskKind::BuildAndTest,
            brief: "Verify escapement two-phase lease loop".to_owned(),
            allowed_paths: vec!["escapement/crates/uas-engine/src/lib.rs".to_owned()],
            check_ids: vec!["escapement:test".to_owned()],
            compensation_plan: Some(valid_compensation_plan()),
        });

        // 1. Runner-1 claims the task (`Queued -> ClaimPendingAck`), but crashes before `confirm_claim`.
        let (task_id, epoch1) = dispatcher
            .claim_next(
                EscapementPoolClass::Build,
                Some(EstateSubject::RuntimeKernel),
                "service-pool-build-7",
            )
            .unwrap();
        assert_eq!(task_id, "act_escapement_0001");
        assert_eq!(epoch1, 1);

        // Attempting to dispatch before `confirm_claim` MUST fail (`REQ-ORCH-ESC-02`).
        let unconfirmed_err = dispatcher
            .dispatch_claimed(
                &task_id,
                "service-pool-build-7",
                epoch1,
                &seat,
                &mut substrate,
                1_700_000_000_000,
            )
            .unwrap_err();
        assert!(
            unconfirmed_err
                .to_string()
                .contains("must be ClaimConfirmed")
        );

        // 2. Requeue unconfirmed task (`REQ-ORCH-ESC-03`) and re-claim with new lease_epoch.
        dispatcher.requeue_unconfirmed(&task_id).unwrap();
        assert_eq!(
            dispatcher.active_tasks_for_runner("service-pool-build-7"),
            0
        );

        let (task_id_2, epoch2) = dispatcher
            .claim_next(
                EscapementPoolClass::Build,
                Some(EstateSubject::RuntimeKernel),
                "service-pool-build-8",
            )
            .unwrap();
        assert_eq!(task_id_2, "act_escapement_0001");
        assert_eq!(epoch2, 2);

        // Stale epoch1 from runner 7 is rejected.
        assert!(
            dispatcher
                .confirm_claim(&task_id_2, "service-pool-build-7", epoch1)
                .is_err()
        );

        // Runner 8 confirms epoch2 -> dispatches -> settles -> active_tasks drops to 0.
        dispatcher
            .confirm_claim(&task_id_2, "service-pool-build-8", epoch2)
            .unwrap();
        dispatcher
            .dispatch_claimed(
                &task_id_2,
                "service-pool-build-8",
                epoch2,
                &seat,
                &mut substrate,
                1_700_000_000_000,
            )
            .unwrap();
        assert_eq!(
            dispatcher.active_tasks_for_runner("service-pool-build-8"),
            1
        );

        let receipt = dispatcher
            .settle_task(
                &task_id_2,
                &mut substrate,
                true,
                "judge_passed",
                Some("d".repeat(64)),
            )
            .unwrap();
        assert_eq!(receipt.head_branch, "pool/act_escapement_0001");
        assert_eq!(
            dispatcher.active_tasks_for_runner("service-pool-build-8"),
            0
        );
        assert_eq!(
            dispatcher.get_task(&task_id_2).unwrap().state,
            EscapementLeaseState::Succeeded
        );
    }

    #[test]
    fn req_orch_seat_01_queue_item_id_hashes_freeform_ids_deterministically() {
        assert_eq!(
            derive_queue_item_id("act_18cf144b5df69451006"),
            "act_18cf144b5df69451006"
        );
        let hashed = derive_queue_item_id("job with spaces / slashes");
        assert!(hashed.starts_with("qitem-uas-"));
        assert_eq!(hashed.len(), "qitem-uas-".len() + 24);
        assert_eq!(hashed, derive_queue_item_id("job with spaces / slashes"));
    }
}
