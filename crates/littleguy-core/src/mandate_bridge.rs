//! SPIFFE agent identity, 5-tier [`SandboxClass`], and Biscuit-to-[`Mandate`] bridge.
//!
//! Enforces the single estate trust domain (`spiffe://unsigned.gg`), 5-tier
//! [`SandboxClass`] ordering, the 5 [`ConstitutionalSealedAction`] gates, and
//! `oath-core` Biscuit-to-`autumn-harvest` [`Mandate`] / [`FlowRules`] compilation
//! (`specs/0001-substrate-seat-and-biscuit-mandate-integration.md`,
//! `REQ-LG-MANDATE-01..05`, `REQ-LG-SANDBOX-01..03`).

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use autumn_harvest_agent::Rule;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Re-export [`autumn_harvest_agent::Mandate`] and [`autumn_harvest_agent::FlowRules`]
/// under both module root and `autumn_harvest` alias so callers referencing
/// `autumn_harvest::Mandate` or `autumn_harvest_agent::Mandate` resolve identically.
pub use autumn_harvest_agent::{FlowRules, Mandate};

/// Type alias module for `autumn_harvest::Mandate` compatibility.
pub mod autumn_harvest {
    pub use autumn_harvest_agent::{FlowRules, Mandate};
}

/// The single canonical SPIFFE trust domain for the estate (`REQ-LG-MANDATE-01`).
pub const ESTATE_TRUST_DOMAIN: &str = "unsigned.gg";

/// The required URI scheme prefix for all SPIFFE IDs.
pub const SPIFFE_SCHEME: &str = "spiffe://";

/// Split-horizon trust domains that are unconditionally rejected (`REQ-LG-MANDATE-01`).
pub const REJECTED_SPLIT_HORIZON_DOMAINS: [&str; 3] = [
    "cerebral.internal",
    "uas.unsigned.gg",
    "substrate-actor.local",
];

/// Maximum length of a compiled `autumn-harvest` agent name.
const HARVEST_MAX_AGENT_NAME_LEN: usize = autumn_harvest_agent::mesh::MAX_NAME_LEN;

/// Errors returned during SPIFFE SVID parsing, Biscuit claim validation,
/// sandbox floor enforcement, or Constitutional Sealed Action checks.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MandateBridgeError {
    /// The URI does not start with `spiffe://` or lacks a trust-domain separator.
    #[error("malformed SPIFFE ID `{spiffe_id}`: expected `spiffe://unsigned.gg/<path>`")]
    MalformedSpiffeUri {
        /// The rejected raw URI string.
        spiffe_id: String,
    },

    /// The SPIFFE ID uses a deprecated split-horizon trust domain (`REQ-LG-MANDATE-01`).
    #[error(
        "split-horizon SPIFFE trust domain `{domain}` is forbidden; single estate trust domain is `spiffe://unsigned.gg`"
    )]
    SplitHorizonTrustDomain {
        /// The rejected split-horizon domain.
        domain: String,
    },

    /// The SPIFFE ID uses an unknown external trust domain.
    #[error("untrusted SPIFFE domain `{domain}`: expected `{ESTATE_TRUST_DOMAIN}`")]
    UntrustedDomain {
        /// The rejected trust domain.
        domain: String,
    },

    /// The SPIFFE path names a human principal (`Rule 1: No path names a person`).
    #[error(
        "SPIFFE ID `{spiffe_id}` violates Rule 1: No path names a person (`/human/` paths are forbidden; use `acting_for` in Biscuit claims)"
    )]
    HumanPathForbidden {
        /// The rejected SPIFFE ID.
        spiffe_id: String,
    },

    /// The SPIFFE path does not match any of the 6 canonical workload/agent families.
    #[error("unrecognized SPIFFE workload path `{path}` under `spiffe://unsigned.gg`")]
    UnrecognizedSpiffePath {
        /// The rejected path portion.
        path: String,
    },

    /// A path segment is empty or contains forbidden characters.
    #[error("invalid SPIFFE path segment `{segment}` in `{spiffe_id}`")]
    InvalidPathSegment {
        /// The offending segment.
        segment: String,
        /// The full SPIFFE ID.
        spiffe_id: String,
    },

    /// The Biscuit delegation token has expired (`now_ms >= expires_at_ms`).
    #[error("Biscuit mandate expired at {expires_at_ms}ms (current time: {now_ms}ms)")]
    MandateExpired {
        /// Expiration timestamp in milliseconds since Unix epoch.
        expires_at_ms: u64,
        /// Current timestamp in milliseconds since Unix epoch.
        now_ms: u64,
    },

    /// The seat's current [`SandboxClass`] is below the mandate's required floor (`REQ-LG-SANDBOX-01`).
    #[error(
        "sandbox floor violation: mandate requires at least `{required}` (tier {}), but seat is running in `{actual}` (tier {})",
        required.tier(),
        actual.tier()
    )]
    SandboxFloorViolation {
        /// Minimum sandbox isolation tier demanded by the Biscuit token.
        required: SandboxClass,
        /// Actual sandbox isolation tier of the seat.
        actual: SandboxClass,
    },

    /// A tool call or delegation attempted one of the 5 Constitutional Sealed Actions (`REQ-LG-MANDATE-05`).
    #[error(
        "constitutional sealed action `{action}` unconditionally refused (ApprovalGate::Never, REQ-LG-MANDATE-05)"
    )]
    ConstitutionalSealedActionRefused {
        /// The sealed action that was refused.
        action: ConstitutionalSealedAction,
    },

    /// A Biscuit claim field is empty or malformed.
    #[error("invalid Biscuit mandate claim `{field}`: {reason}")]
    InvalidClaim {
        /// Name of the invalid claim field.
        field: &'static str,
        /// Why the claim was rejected.
        reason: String,
    },

    /// Child attenuation attempted to widen authority beyond the parent mandate (`REQ-LG-MANDATE-03`).
    #[error("invalid Biscuit attenuation: {reason}")]
    AttenuationWidening {
        /// Explanation of the attempted privilege escalation.
        reason: String,
    },

    /// The compiled `autumn-harvest` [`Mandate`] failed structural validation.
    #[error("compiled autumn-harvest mandate failed validation: {reason}")]
    InvalidHarvestMandate {
        /// Validation error from `autumn_harvest_agent::Mandate::validate`.
        reason: String,
    },
}

/// Canonical SPIFFE path families under `spiffe://unsigned.gg` (`REQ-LG-MANDATE-01`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "family", rename_all = "snake_case")]
pub enum SpiffePathFamily {
    /// `/agent/<team>/<agent_id>`
    Agent {
        /// Owning team slug.
        team: String,
        /// Agent identifier.
        agent_id: String,
    },
    /// `/conclave/<mesh>/elf/<elf_id>`
    ConclaveElf {
        /// Conclave mesh name.
        mesh: String,
        /// Ephemeral elf worker identifier.
        elf_id: String,
    },
    /// `/actor/<atespace>/<actor>`
    Actor {
        /// Actor namespace (`atespace`).
        atespace: String,
        /// Actor name.
        actor: String,
    },
    /// `/worker-for-actor/<atespace>/<actor>`
    WorkerForActor {
        /// Actor namespace (`atespace`).
        atespace: String,
        /// Target actor name.
        actor: String,
    },
    /// `/host/<host>/lane/<role>`
    HostLane {
        /// Physical or virtual host name (e.g. `ceres`).
        host: String,
        /// Lane role callsign.
        role: String,
    },
    /// `/cluster/<cluster>/ns/<ns>/sa/<sa>`
    ClusterServiceAccount {
        /// Kubernetes cluster name (e.g. `cygnus`).
        cluster: String,
        /// Kubernetes namespace.
        namespace: String,
        /// Kubernetes service account name.
        service_account: String,
    },
}

/// Validated single-trust-domain workload/agent identity under `spiffe://unsigned.gg/`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SpiffeAgentIdentity {
    /// Full canonical SPIFFE ID string (`spiffe://unsigned.gg/...`).
    pub spiffe_id: String,
    /// Validated trust domain (always `"unsigned.gg"`).
    pub trust_domain: String,
    /// Parsed canonical workload/agent path family.
    pub family: SpiffePathFamily,
}

impl SpiffeAgentIdentity {
    /// Parse and validate a SPIFFE ID under the single estate trust domain
    /// `spiffe://unsigned.gg/` (`REQ-LG-MANDATE-01`).
    ///
    /// - Explicitly rejects `cerebral.internal`, `uas.unsigned.gg`, and
    ///   `substrate-actor.local`.
    /// - Explicitly rejects `/human/` paths (`Rule 1: No path names a person`).
    /// - Parses the 6 canonical workload/agent path families.
    ///
    /// # Errors
    ///
    /// Returns [`MandateBridgeError`] if the scheme, trust domain, or path
    /// shape violates estate SPIFFE invariants.
    pub fn parse(spiffe_id: &str) -> Result<Self, MandateBridgeError> {
        let rest = spiffe_id.strip_prefix(SPIFFE_SCHEME).ok_or_else(|| {
            MandateBridgeError::MalformedSpiffeUri {
                spiffe_id: spiffe_id.to_owned(),
            }
        })?;

        let (domain, path_without_leading_slash) =
            rest.split_once('/')
                .ok_or_else(|| MandateBridgeError::MalformedSpiffeUri {
                    spiffe_id: spiffe_id.to_owned(),
                })?;

        if domain.is_empty() {
            return Err(MandateBridgeError::MalformedSpiffeUri {
                spiffe_id: spiffe_id.to_owned(),
            });
        }

        if REJECTED_SPLIT_HORIZON_DOMAINS.contains(&domain) {
            return Err(MandateBridgeError::SplitHorizonTrustDomain {
                domain: domain.to_owned(),
            });
        }

        if domain != ESTATE_TRUST_DOMAIN {
            return Err(MandateBridgeError::UntrustedDomain {
                domain: domain.to_owned(),
            });
        }

        let path = format!("/{path_without_leading_slash}");

        // Rule 1: No path names a person (`/human/` paths are forbidden).
        let segments: Vec<&str> = path_without_leading_slash.split('/').collect();
        if segments.contains(&"human") {
            return Err(MandateBridgeError::HumanPathForbidden {
                spiffe_id: spiffe_id.to_owned(),
            });
        }

        for seg in &segments {
            if !is_valid_spiffe_segment(seg) {
                return Err(MandateBridgeError::InvalidPathSegment {
                    segment: (*seg).to_owned(),
                    spiffe_id: spiffe_id.to_owned(),
                });
            }
        }

        let family = match segments.as_slice() {
            ["agent", team, agent_id] => SpiffePathFamily::Agent {
                team: (*team).to_owned(),
                agent_id: (*agent_id).to_owned(),
            },
            ["conclave", mesh, "elf", elf_id] => SpiffePathFamily::ConclaveElf {
                mesh: (*mesh).to_owned(),
                elf_id: (*elf_id).to_owned(),
            },
            ["actor", atespace, actor] => SpiffePathFamily::Actor {
                atespace: (*atespace).to_owned(),
                actor: (*actor).to_owned(),
            },
            ["worker-for-actor", atespace, actor] => SpiffePathFamily::WorkerForActor {
                atespace: (*atespace).to_owned(),
                actor: (*actor).to_owned(),
            },
            ["host", host, "lane", role] => SpiffePathFamily::HostLane {
                host: (*host).to_owned(),
                role: (*role).to_owned(),
            },
            ["cluster", cluster, "ns", namespace, "sa", service_account] => {
                SpiffePathFamily::ClusterServiceAccount {
                    cluster: (*cluster).to_owned(),
                    namespace: (*namespace).to_owned(),
                    service_account: (*service_account).to_owned(),
                }
            }
            _ => {
                return Err(MandateBridgeError::UnrecognizedSpiffePath { path });
            }
        };

        Ok(Self {
            spiffe_id: spiffe_id.to_owned(),
            trust_domain: ESTATE_TRUST_DOMAIN.to_owned(),
            family,
        })
    }

    /// Returns the full validated SPIFFE URI (`spiffe://unsigned.gg/...`).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.spiffe_id
    }

    /// Returns the parsed [`SpiffePathFamily`].
    #[must_use]
    pub const fn family(&self) -> &SpiffePathFamily {
        &self.family
    }

    /// Derives a normalized `autumn-harvest` holder name (`[a-z0-9_-]{1,64}`)
    /// satisfying `autumn_harvest_agent::mesh::valid_name`.
    #[must_use]
    pub fn harvest_holder_name(&self) -> String {
        let raw = match &self.family {
            SpiffePathFamily::Agent { team, agent_id } => format!("agent-{team}-{agent_id}"),
            SpiffePathFamily::ConclaveElf { mesh, elf_id } => format!("elf-{mesh}-{elf_id}"),
            SpiffePathFamily::Actor { atespace, actor } => format!("actor-{atespace}-{actor}"),
            SpiffePathFamily::WorkerForActor { atespace, actor } => {
                format!("worker-{atespace}-{actor}")
            }
            SpiffePathFamily::HostLane { host, role } => format!("lane-{host}-{role}"),
            SpiffePathFamily::ClusterServiceAccount {
                cluster,
                namespace,
                service_account,
            } => format!("sa-{cluster}-{namespace}-{service_account}"),
        };

        let mut normalized: String = raw
            .chars()
            .map(|ch| {
                let lower = ch.to_ascii_lowercase();
                if matches!(lower, 'a'..='z' | '0'..='9' | '_' | '-') {
                    lower
                } else {
                    '-'
                }
            })
            .take(HARVEST_MAX_AGENT_NAME_LEN)
            .collect();

        if normalized.is_empty() {
            normalized.push_str("lg-agent");
        }
        normalized
    }
}

impl fmt::Display for SpiffeAgentIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.spiffe_id)
    }
}

impl FromStr for SpiffeAgentIdentity {
    type Err = MandateBridgeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Checks whether a SPIFFE path segment is non-empty, not `.` or `..`, and
/// consists solely of ASCII alphanumeric characters, `-`, `_`, or `.`.
fn is_valid_spiffe_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg != "."
        && seg != ".."
        && seg
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.'))
}

/// 5-tier sandbox isolation classification (`REQ-LG-SANDBOX-01`).
///
/// Strictly ordered from weakest (`Process`) to strongest (`Ouroboros`):
/// `Process < Gvisor < Kata < Firecracker < Ouroboros`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxClass {
    /// Tier 0: `blackwall` (`sbx-core` Landlock v3 + seccomp-BPF + cgroup v2 + `DeltaDB` `CoW`).
    Process = 0,
    /// Tier 1: `runsc` (`gVisor` user-space application kernel).
    Gvisor = 1,
    /// Tier 2: `kata-runtime` (QEMU/Cloud-Hypervisor lightweight hardware VM).
    Kata = 2,
    /// Tier 3: `firecracker` / `jailer` microVM (`<125ms` boot, virtio-vsock).
    Firecracker = 3,
    /// Tier 4: `unsigned-gg/ouroborOS` (`pgray` capability microkernel + KV page transfer) + `hermit` `detcore`.
    Ouroboros = 4,
}

impl SandboxClass {
    /// All 5 tiers in ascending isolation order.
    pub const ALL: [Self; 5] = [
        Self::Process,
        Self::Gvisor,
        Self::Kata,
        Self::Firecracker,
        Self::Ouroboros,
    ];

    /// Numeric tier index (`0..=4`).
    #[must_use]
    pub const fn tier(self) -> u8 {
        self as u8
    }

    /// Canonical `snake_case` identifier used in Biscuit Datalog facts and flow labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Gvisor => "gvisor",
            Self::Kata => "kata",
            Self::Firecracker => "firecracker",
            Self::Ouroboros => "ouroboros",
        }
    }

    /// Returns `true` when `self` meets or exceeds the `minimum` required sandbox tier.
    #[must_use]
    pub const fn satisfies_floor(self, minimum: Self) -> bool {
        (self as u8) >= (minimum as u8)
    }
}

impl fmt::Display for SandboxClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Reversibility classification of an external side effect (`REQ-LG-HARVEST-02`).
///
/// Ordered from least impactful (`Pure`) to most impactful (`Irreversible`):
/// `Pure < Idempotent < Compensatable < Irreversible`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReversibilityClass {
    /// Deterministic computation with zero external state mutation.
    Pure = 0,
    /// Safe to re-execute on WAL replay without duplicating side effects.
    Idempotent = 1,
    /// Mutates external state but carries a paired `CompensationPlan` rollback step.
    Compensatable = 2,
    /// Cannot be undone once executed; requires highest mandate authority or human gate.
    Irreversible = 3,
}

impl ReversibilityClass {
    /// All 4 reversibility classes in ascending impact order.
    pub const ALL: [Self; 4] = [
        Self::Pure,
        Self::Idempotent,
        Self::Compensatable,
        Self::Irreversible,
    ];

    /// Canonical `snake_case` name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pure => "pure",
            Self::Idempotent => "idempotent",
            Self::Compensatable => "compensatable",
            Self::Irreversible => "irreversible",
        }
    }

    /// Flow label attached to tool calls of this reversibility class.
    #[must_use]
    pub const fn flow_label(self) -> &'static str {
        match self {
            Self::Pure => "reversibility:pure",
            Self::Idempotent => "reversibility:idempotent",
            Self::Compensatable => "reversibility:compensatable",
            Self::Irreversible => "reversibility:irreversible",
        }
    }
}

impl fmt::Display for ReversibilityClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The 5 Constitutional Sealed Actions (`REQ-LG-MANDATE-05`) that unconditionally
/// refuse autonomous or delegated agent execution (`ApprovalGate::Never`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstitutionalSealedAction {
    /// Modifying the Sovereign Root or Second Sovereign Root quorum configuration.
    ModifySovereignRootQuorum,
    /// Rotating the SPIRE or `OpenBao` root CA outside the physical ceremony.
    RotateSpireOrOpenBaoRootCa,
    /// Disabling or truncating the Signet hash-chained audit ledger.
    DisableSignetAuditLedger,
    /// Shredding an active principal's PII encryption key without a closed erasure ticket.
    ShredActivePrincipalPiiKey,
    /// Granting unbounded superadmin capabilities to any workload or principal.
    GrantUnboundedSuperadmin,
}

impl ConstitutionalSealedAction {
    /// All 5 Constitutional Sealed Actions.
    pub const ALL: [Self; 5] = [
        Self::ModifySovereignRootQuorum,
        Self::RotateSpireOrOpenBaoRootCa,
        Self::DisableSignetAuditLedger,
        Self::ShredActivePrincipalPiiKey,
        Self::GrantUnboundedSuperadmin,
    ];

    /// Canonical `snake_case` identifier for this sealed action.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModifySovereignRootQuorum => "modify_sovereign_root_quorum",
            Self::RotateSpireOrOpenBaoRootCa => "rotate_spire_or_openbao_root_ca",
            Self::DisableSignetAuditLedger => "disable_signet_audit_ledger",
            Self::ShredActivePrincipalPiiKey => "shred_active_principal_pii_key",
            Self::GrantUnboundedSuperadmin => "grant_unbounded_superadmin",
        }
    }

    /// Canonical flow label used to block this sealed action in `FlowRules`.
    #[must_use]
    pub const fn flow_label(self) -> &'static str {
        match self {
            Self::ModifySovereignRootQuorum => "sealed:modify_sovereign_root_quorum",
            Self::RotateSpireOrOpenBaoRootCa => "sealed:rotate_spire_or_openbao_root_ca",
            Self::DisableSignetAuditLedger => "sealed:disable_signet_audit_ledger",
            Self::ShredActivePrincipalPiiKey => "sealed:shred_active_principal_pii_key",
            Self::GrantUnboundedSuperadmin => "sealed:grant_unbounded_superadmin",
        }
    }

    /// Matches a tool name, action name, or flow label against the 5 sealed actions.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let trimmed = name.strip_prefix("sealed:").unwrap_or(name);
        match trimmed {
            "modify_sovereign_root_quorum" | "ModifySovereignRootQuorum" => {
                Some(Self::ModifySovereignRootQuorum)
            }
            "rotate_spire_or_openbao_root_ca"
            | "rotate_spire_or_open_bao_root_ca"
            | "RotateSpireOrOpenBaoRootCa" => Some(Self::RotateSpireOrOpenBaoRootCa),
            "disable_signet_audit_ledger" | "DisableSignetAuditLedger" => {
                Some(Self::DisableSignetAuditLedger)
            }
            "shred_active_principal_pii_key" | "ShredActivePrincipalPiiKey" => {
                Some(Self::ShredActivePrincipalPiiKey)
            }
            "grant_unbounded_superadmin" | "GrantUnboundedSuperadmin" => {
                Some(Self::GrantUnboundedSuperadmin)
            }
            _ => None,
        }
    }

    /// Unconditionally refuses execution of `self` (`ApprovalGate::Never`, `REQ-LG-MANDATE-05`).
    ///
    /// # Errors
    ///
    /// Always returns [`MandateBridgeError::ConstitutionalSealedActionRefused`].
    pub const fn enforce_never(self) -> Result<(), MandateBridgeError> {
        Err(MandateBridgeError::ConstitutionalSealedActionRefused { action: self })
    }

    /// Checks an arbitrary action or tool name and refuses if it targets a
    /// Constitutional Sealed Action (`REQ-LG-MANDATE-05`).
    ///
    /// # Errors
    ///
    /// Returns [`MandateBridgeError::ConstitutionalSealedActionRefused`] when
    /// `name` matches any of the 5 sealed actions.
    pub fn check_action_name(name: &str) -> Result<(), MandateBridgeError> {
        if let Some(action) = Self::from_name(name) {
            return action.enforce_never();
        }
        Ok(())
    }
}

impl fmt::Display for ConstitutionalSealedAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Verified Datalog facts extracted from an `oath-core` Ed25519 Biscuit
/// delegation token and paired `spiffe://unsigned.gg` workload SVID
/// (`REQ-LG-MANDATE-02`, `REQ-LG-MANDATE-03`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BiscuitMandateClaims {
    /// Workload/agent SPIFFE ID (`spiffe://unsigned.gg/...`).
    pub spiffe_id: String,
    /// Human or service principal on whose behalf the agent acts (`acting_for`).
    pub acting_for_principal: String,
    /// Minimum [`SandboxClass`] isolation tier required by the Biscuit check.
    pub min_sandbox_class: SandboxClass,
    /// Repository prefixes the mandate holder is permitted to mutate.
    pub allowed_repo_prefixes: Vec<String>,
    /// Maximum [`ReversibilityClass`] permitted without escalation.
    pub max_reversibility: ReversibilityClass,
    /// Token expiration timestamp in milliseconds since Unix epoch.
    pub expires_at_ms: u64,
}

impl BiscuitMandateClaims {
    /// Validates the Biscuit claims against `current_sandbox` and `now_ms`,
    /// then compiles them into an [`autumn_harvest::Mandate`]
    /// (`autumn_harvest_agent::Mandate`, `REQ-LG-MANDATE-02`).
    ///
    /// Validation order:
    /// 1. Parse and validate `spiffe_id` under `spiffe://unsigned.gg/`.
    /// 2. Validate `acting_for_principal` (must be non-empty and must not smuggle a `/human/` SPIFFE URI).
    /// 3. Validate token expiry (`now_ms < expires_at_ms`).
    /// 4. Validate sandbox floor (`current_sandbox >= min_sandbox_class`).
    /// 5. Validate `allowed_repo_prefixes` entries and reject any sealed action name.
    /// 6. Compile to [`Mandate`] with `read_only` and structured `flow_labels`.
    ///
    /// # Errors
    ///
    /// Returns [`MandateBridgeError`] if any check fails or the resulting
    /// [`Mandate`] fails `Mandate::validate`.
    pub fn compile_to_harvest_mandate(
        &self,
        current_sandbox: SandboxClass,
        now_ms: u64,
    ) -> Result<Mandate, MandateBridgeError> {
        let identity = self.validate_context(current_sandbox, now_ms)?;

        let mut flow_labels = BTreeSet::new();
        flow_labels.insert(format!("spiffe:{}", identity.as_str()));
        flow_labels.insert(format!("acting_for:{}", self.acting_for_principal.trim()));
        flow_labels.insert(format!("sandbox:{}", current_sandbox.as_str()));
        flow_labels.insert(format!("min_sandbox:{}", self.min_sandbox_class.as_str()));
        flow_labels.insert(format!(
            "max_reversibility:{}",
            self.max_reversibility.as_str()
        ));

        for prefix in &self.allowed_repo_prefixes {
            flow_labels.insert(format!("repo:{prefix}"));
        }

        let mut mandate = Mandate::new(identity.harvest_holder_name());
        if self.max_reversibility == ReversibilityClass::Pure {
            mandate = mandate.read_only();
        }
        mandate.flow_labels = flow_labels;
        mandate
            .validate()
            .map_err(|reason| MandateBridgeError::InvalidHarvestMandate { reason })?;

        Ok(mandate)
    }

    /// Compiles the Biscuit claims into `autumn-harvest` [`FlowRules`] enforcing:
    /// - Unconditional `Rule::Deny` on all 5 [`ConstitutionalSealedAction`] labels (`REQ-LG-MANDATE-05`).
    /// - `Rule::Deny` on any [`ReversibilityClass`] exceeding `self.max_reversibility`.
    /// - The Agents Rule of Two for untrusted input + private data + write/external effects.
    ///
    /// # Errors
    ///
    /// Returns [`MandateBridgeError`] if the claims fail expiry, SPIFFE, or sandbox floor validation.
    pub fn compile_flow_rules(
        &self,
        current_sandbox: SandboxClass,
        now_ms: u64,
    ) -> Result<FlowRules, MandateBridgeError> {
        let _identity = self.validate_context(current_sandbox, now_ms)?;

        let mut rules = FlowRules::rule_of_two();

        // REQ-LG-MANDATE-05: Unconditionally refuse all 5 Constitutional Sealed Actions.
        for sealed in ConstitutionalSealedAction::ALL {
            rules = rules.forbid(
                [sealed.flow_label()],
                Rule::Deny(format!(
                    "constitutional sealed action `{sealed}` is unconditionally forbidden (ApprovalGate::Never)"
                )),
            );
        }

        // Deny any reversibility class strictly above `max_reversibility`.
        for class in ReversibilityClass::ALL {
            if class > self.max_reversibility {
                rules = rules.forbid(
                    [class.flow_label()],
                    Rule::Deny(format!(
                        "action reversibility `{class}` exceeds mandate max_reversibility `{}`",
                        self.max_reversibility
                    )),
                );
            }
        }

        Ok(rules)
    }

    /// Compiles both the [`Mandate`] and [`FlowRules`] from this Biscuit claim set and SVID
    /// (`Mandate::from_biscuit_and_svid`, `REQ-LG-MANDATE-02`).
    ///
    /// # Errors
    ///
    /// Returns [`MandateBridgeError`] if validation or compilation fails.
    pub fn from_biscuit_and_svid(
        &self,
        current_sandbox: SandboxClass,
        now_ms: u64,
    ) -> Result<(Mandate, FlowRules), MandateBridgeError> {
        let mandate = self.compile_to_harvest_mandate(current_sandbox, now_ms)?;
        let flow_rules = self.compile_flow_rules(current_sandbox, now_ms)?;
        Ok((mandate, flow_rules))
    }

    /// Checks whether a tool/action invocation is permitted under this mandate,
    /// unconditionally refusing all 5 [`ConstitutionalSealedAction`] variants (`REQ-LG-MANDATE-05`).
    ///
    /// # Errors
    ///
    /// Returns [`MandateBridgeError`] if `action_name` is a sealed action or if
    /// the mandate context is invalid/expired/under-sandboxed.
    pub fn authorize_action(
        &self,
        action_name: &str,
        current_sandbox: SandboxClass,
        now_ms: u64,
    ) -> Result<(), MandateBridgeError> {
        ConstitutionalSealedAction::check_action_name(action_name)?;
        let _identity = self.validate_context(current_sandbox, now_ms)?;
        Ok(())
    }

    /// Appends an offline Biscuit attenuation block for a child subagent (`REQ-LG-MANDATE-03`).
    ///
    /// Authority can only narrow down the delegation tree:
    /// - `child_min_sandbox` must be `>= self.min_sandbox_class`.
    /// - Every prefix in `child_repo_prefixes` must be covered by `self.allowed_repo_prefixes`.
    /// - `child_max_reversibility` must be `<= self.max_reversibility`.
    /// - `child_expires_at_ms` must be `<= self.expires_at_ms`.
    ///
    /// # Errors
    ///
    /// Returns [`MandateBridgeError`] if the child SPIFFE ID is invalid or any
    /// parameter attempts to widen the parent's authority.
    pub fn attenuate_for_child(
        &self,
        child_spiffe_id: &str,
        child_min_sandbox: SandboxClass,
        child_repo_prefixes: Vec<String>,
        child_max_reversibility: ReversibilityClass,
        child_expires_at_ms: u64,
    ) -> Result<Self, MandateBridgeError> {
        let _parent_id = SpiffeAgentIdentity::parse(&self.spiffe_id)?;
        let _child_id = SpiffeAgentIdentity::parse(child_spiffe_id)?;

        if child_min_sandbox < self.min_sandbox_class {
            return Err(MandateBridgeError::AttenuationWidening {
                reason: format!(
                    "child min_sandbox_class `{child_min_sandbox}` is weaker than parent `{}`",
                    self.min_sandbox_class
                ),
            });
        }

        if child_max_reversibility > self.max_reversibility {
            return Err(MandateBridgeError::AttenuationWidening {
                reason: format!(
                    "child max_reversibility `{child_max_reversibility}` exceeds parent `{}`",
                    self.max_reversibility
                ),
            });
        }

        if child_expires_at_ms > self.expires_at_ms {
            return Err(MandateBridgeError::AttenuationWidening {
                reason: format!(
                    "child expires_at_ms ({child_expires_at_ms}) exceeds parent ({})",
                    self.expires_at_ms
                ),
            });
        }

        for child_prefix in &child_repo_prefixes {
            let covered = self
                .allowed_repo_prefixes
                .iter()
                .any(|parent_prefix| child_prefix.starts_with(parent_prefix));
            if !covered {
                return Err(MandateBridgeError::AttenuationWidening {
                    reason: format!(
                        "child repo prefix `{child_prefix}` is not within parent allowed_repo_prefixes"
                    ),
                });
            }
        }

        Ok(Self {
            spiffe_id: child_spiffe_id.to_owned(),
            acting_for_principal: self.acting_for_principal.clone(),
            min_sandbox_class: child_min_sandbox,
            allowed_repo_prefixes: child_repo_prefixes,
            max_reversibility: child_max_reversibility,
            expires_at_ms: child_expires_at_ms,
        })
    }

    fn validate_context(
        &self,
        current_sandbox: SandboxClass,
        now_ms: u64,
    ) -> Result<SpiffeAgentIdentity, MandateBridgeError> {
        let identity = SpiffeAgentIdentity::parse(&self.spiffe_id)?;

        let principal = self.acting_for_principal.trim();
        if principal.is_empty() || principal.chars().any(char::is_control) {
            return Err(MandateBridgeError::InvalidClaim {
                field: "acting_for_principal",
                reason: "acting_for_principal must be non-empty with no control characters"
                    .to_owned(),
            });
        }
        if principal.starts_with(SPIFFE_SCHEME) && principal.contains("/human/") {
            return Err(MandateBridgeError::HumanPathForbidden {
                spiffe_id: principal.to_owned(),
            });
        }

        if now_ms >= self.expires_at_ms {
            return Err(MandateBridgeError::MandateExpired {
                expires_at_ms: self.expires_at_ms,
                now_ms,
            });
        }

        if !current_sandbox.satisfies_floor(self.min_sandbox_class) {
            return Err(MandateBridgeError::SandboxFloorViolation {
                required: self.min_sandbox_class,
                actual: current_sandbox,
            });
        }

        for prefix in &self.allowed_repo_prefixes {
            if prefix.trim().is_empty() || prefix.chars().any(char::is_control) {
                return Err(MandateBridgeError::InvalidClaim {
                    field: "allowed_repo_prefixes",
                    reason: "repo prefix must be non-empty with no control characters".to_owned(),
                });
            }
            ConstitutionalSealedAction::check_action_name(prefix)?;
        }

        Ok(identity)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use autumn_harvest_agent::ToolDecision;
    use autumn_harvest_agent::flow::Released;

    use super::*;

    fn sample_claims() -> BiscuitMandateClaims {
        BiscuitMandateClaims {
            spiffe_id: "spiffe://unsigned.gg/agent/platform/littleguy-01".to_owned(),
            acting_for_principal: "principal:ctodie".to_owned(),
            min_sandbox_class: SandboxClass::Kata,
            allowed_repo_prefixes: vec![
                "cerebral-work/littleguy".to_owned(),
                "cerebral-work/vespertin".to_owned(),
            ],
            max_reversibility: ReversibilityClass::Compensatable,
            expires_at_ms: 1_800_000_000_000,
        }
    }

    #[test]
    fn parses_all_six_canonical_spiffe_path_families() {
        let agent =
            SpiffeAgentIdentity::parse("spiffe://unsigned.gg/agent/platform/littleguy-01").unwrap();
        assert_eq!(agent.trust_domain, "unsigned.gg");
        assert_eq!(
            agent.family,
            SpiffePathFamily::Agent {
                team: "platform".to_owned(),
                agent_id: "littleguy-01".to_owned(),
            }
        );
        assert_eq!(agent.harvest_holder_name(), "agent-platform-littleguy-01");

        let elf =
            SpiffeAgentIdentity::parse("spiffe://unsigned.gg/conclave/ceres/elf/elf-42").unwrap();
        assert_eq!(
            elf.family,
            SpiffePathFamily::ConclaveElf {
                mesh: "ceres".to_owned(),
                elf_id: "elf-42".to_owned(),
            }
        );

        let actor =
            SpiffeAgentIdentity::parse("spiffe://unsigned.gg/actor/substrate/scheduler").unwrap();
        assert_eq!(
            actor.family,
            SpiffePathFamily::Actor {
                atespace: "substrate".to_owned(),
                actor: "scheduler".to_owned(),
            }
        );

        let worker =
            SpiffeAgentIdentity::parse("spiffe://unsigned.gg/worker-for-actor/substrate/scheduler")
                .unwrap();
        assert_eq!(
            worker.family,
            SpiffePathFamily::WorkerForActor {
                atespace: "substrate".to_owned(),
                actor: "scheduler".to_owned(),
            }
        );

        let lane =
            SpiffeAgentIdentity::parse("spiffe://unsigned.gg/host/ceres/lane/t15-builder").unwrap();
        assert_eq!(
            lane.family,
            SpiffePathFamily::HostLane {
                host: "ceres".to_owned(),
                role: "t15-builder".to_owned(),
            }
        );

        let sa = SpiffeAgentIdentity::parse(
            "spiffe://unsigned.gg/cluster/cygnus/ns/escapement/sa/littleguy-runner",
        )
        .unwrap();
        assert_eq!(
            sa.family,
            SpiffePathFamily::ClusterServiceAccount {
                cluster: "cygnus".to_owned(),
                namespace: "escapement".to_owned(),
                service_account: "littleguy-runner".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_split_horizon_trust_domains_and_human_paths() {
        for bad_domain in [
            "spiffe://cerebral.internal/agent/platform/lg-01",
            "spiffe://uas.unsigned.gg/agent/platform/lg-01",
            "spiffe://substrate-actor.local/actor/core/runner",
        ] {
            let err = SpiffeAgentIdentity::parse(bad_domain).unwrap_err();
            assert!(
                matches!(err, MandateBridgeError::SplitHorizonTrustDomain { .. }),
                "expected SplitHorizonTrustDomain for {bad_domain}, got {err:?}"
            );
        }

        for human_uri in [
            "spiffe://unsigned.gg/human/ctodie",
            "spiffe://unsigned.gg/agent/human/ctodie",
        ] {
            let err = SpiffeAgentIdentity::parse(human_uri).unwrap_err();
            assert!(
                matches!(err, MandateBridgeError::HumanPathForbidden { .. }),
                "expected HumanPathForbidden for {human_uri}, got {err:?}"
            );
            assert!(
                err.to_string().contains("Rule 1: No path names a person"),
                "error message must cite Rule 1: {err}"
            );
        }
    }

    #[test]
    fn enforces_five_tier_sandbox_class_strict_ordering_and_floor() {
        assert!(SandboxClass::Process < SandboxClass::Gvisor);
        assert!(SandboxClass::Gvisor < SandboxClass::Kata);
        assert!(SandboxClass::Kata < SandboxClass::Firecracker);
        assert!(SandboxClass::Firecracker < SandboxClass::Ouroboros);

        let claims = sample_claims(); // min_sandbox_class = Kata
        let now_ms = 1_750_000_000_000;

        for weak in [SandboxClass::Process, SandboxClass::Gvisor] {
            let err = claims.compile_to_harvest_mandate(weak, now_ms).unwrap_err();
            assert_eq!(
                err,
                MandateBridgeError::SandboxFloorViolation {
                    required: SandboxClass::Kata,
                    actual: weak,
                }
            );
        }

        for strong in [
            SandboxClass::Kata,
            SandboxClass::Firecracker,
            SandboxClass::Ouroboros,
        ] {
            let mandate = claims.compile_to_harvest_mandate(strong, now_ms).unwrap();
            assert_eq!(mandate.holder(), "agent-platform-littleguy-01");
            assert!(
                mandate
                    .flow_labels
                    .contains(&format!("sandbox:{}", strong.as_str()))
            );
            assert!(
                mandate
                    .flow_labels
                    .contains("spiffe:spiffe://unsigned.gg/agent/platform/littleguy-01")
            );
            assert!(mandate.flow_labels.contains("repo:cerebral-work/littleguy"));
        }
    }

    #[test]
    fn rejects_expired_biscuit_mandates() {
        let claims = sample_claims();
        let exact_expiry = claims.expires_at_ms;
        let past_expiry = claims.expires_at_ms + 5_000;

        for expired_now in [exact_expiry, past_expiry] {
            let err = claims
                .compile_to_harvest_mandate(SandboxClass::Firecracker, expired_now)
                .unwrap_err();
            assert_eq!(
                err,
                MandateBridgeError::MandateExpired {
                    expires_at_ms: claims.expires_at_ms,
                    now_ms: expired_now,
                }
            );
        }
    }

    #[test]
    fn refuses_all_five_constitutional_sealed_actions_unconditionally() {
        let claims = sample_claims();
        let now_ms = 1_750_000_000_000;
        let (_, flow_rules) = claims
            .from_biscuit_and_svid(SandboxClass::Ouroboros, now_ms)
            .unwrap();

        assert_eq!(ConstitutionalSealedAction::ALL.len(), 5);

        for action in ConstitutionalSealedAction::ALL {
            assert_eq!(
                action.enforce_never(),
                Err(MandateBridgeError::ConstitutionalSealedActionRefused { action })
            );
            assert_eq!(
                claims.authorize_action(action.as_str(), SandboxClass::Ouroboros, now_ms),
                Err(MandateBridgeError::ConstitutionalSealedActionRefused { action })
            );

            let call_labels = BTreeSet::from([action.flow_label().to_owned()]);
            let decision = flow_rules.check(&BTreeSet::new(), &Released::new(), &call_labels);
            assert!(
                matches!(decision, ToolDecision::Deny { ref reason } if reason.contains("ApprovalGate::Never")),
                "expected ToolDecision::Deny for sealed action {action}, got {decision:?}"
            );
        }
    }

    #[test]
    fn child_attenuation_only_narrows_authority() {
        let parent = sample_claims();
        let child = parent
            .attenuate_for_child(
                "spiffe://unsigned.gg/conclave/ceres/elf/worker-1",
                SandboxClass::Firecracker,
                vec!["cerebral-work/littleguy".to_owned()],
                ReversibilityClass::Pure,
                parent.expires_at_ms - 60_000,
            )
            .unwrap();

        let child_mandate = child
            .compile_to_harvest_mandate(SandboxClass::Firecracker, 1_750_000_000_000)
            .unwrap();
        assert!(child_mandate.read_only);
        assert_eq!(child_mandate.holder(), "elf-ceres-worker-1");

        // Attempting to lower sandbox floor below parent (Kata -> Process) must fail.
        assert!(matches!(
            parent.attenuate_for_child(
                "spiffe://unsigned.gg/conclave/ceres/elf/worker-1",
                SandboxClass::Process,
                vec!["cerebral-work/littleguy".to_owned()],
                ReversibilityClass::Pure,
                parent.expires_at_ms - 60_000,
            ),
            Err(MandateBridgeError::AttenuationWidening { .. })
        ));
    }

    // REQ-LG-GHOST-08: the claims accept the Ghost seat and elf forms.
    #[test]
    fn claims_accept_agent_and_elf_spiffe_forms() {
        for id in [
            "spiffe://unsigned.gg/agent/platform/sadr",
            "spiffe://unsigned.gg/conclave/ceres/elf/w1",
        ] {
            let mut claims = sample_claims();
            claims.spiffe_id = id.to_owned();
            assert!(
                claims
                    .compile_to_harvest_mandate(SandboxClass::Kata, 1_750_000_000_000)
                    .is_ok(),
                "claims must accept {id}"
            );
        }
    }

    // REQ-LG-GHOST-08: the claims refuse foreign domains, `/human/` and unknown shapes.
    #[test]
    fn claims_refuse_foreign_human_and_unknown_spiffe() {
        for id in [
            "spiffe://cerebral.internal/agent/platform/sadr",
            "spiffe://unsigned.gg/human/ctodie",
            "spiffe://unsigned.gg/wizard/platform/sadr",
        ] {
            let mut claims = sample_claims();
            claims.spiffe_id = id.to_owned();
            assert!(
                claims
                    .compile_to_harvest_mandate(SandboxClass::Kata, 1_750_000_000_000)
                    .is_err(),
                "claims must refuse {id}"
            );
        }
    }
}
