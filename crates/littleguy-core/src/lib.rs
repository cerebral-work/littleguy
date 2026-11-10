//! The core of littleguy: ids, configuration and the routing kernel.
//!
//! The crate holds no I/O. The binary crate reads the config file and the
//! sockets, then asks [`route`] where each inbound message goes.

pub mod addr;
pub mod config;
pub mod ids;
pub mod mandate_bridge;
pub mod orchestration;
pub mod pairing;
pub mod route;

pub use config::{
    ApprovalsConfig, BriefConfig, CamelRoute, ChatConfig, Config, ConfigError, DiscordConfig,
    FirecrackerConfig, FlowConfig, FlowRecheck, GoogleConfig, Integration, McpServer, McpTransport,
    MemoryConfig, SanMateoConfig, SanMateoError, SandboxBackend, SandboxConfig, SlackConfig,
    ToolsConfig, chat_user, discord_sender, slack_sender,
};
pub use ids::{ChannelId, IdError, MessageId, OwnerName, SenderId};
pub use mandate_bridge::{
    BiscuitMandateClaims, ConstitutionalSealedAction, ESTATE_TRUST_DOMAIN, MandateBridgeError,
    REJECTED_SPLIT_HORIZON_DOMAINS, ReversibilityClass, SandboxClass, SpiffeAgentIdentity,
    SpiffePathFamily,
};
pub use orchestration::{
    AgentEffect, AuthorityGrant, AuthorityViolation, CompensationPlan, DomainGatePolicy,
    EffectKind, EffectProvenance, EscapementLeaseState, EscapementPoolClass,
    EscapementPoolDispatcher, EscapementPoolTask, EscapementTaskKind, EscapementTaskSpec,
    EstateSubject, LittleguySeatSlot, LittleguySeatSubstrate, MeshDomainKind, SeatId, SeatJobSpec,
    SeatJobState, SeatLiveness, SeatReceipt, SeatSubstrateError, TripwireRegistry, WebMcpManifest,
    WebMcpToolDescriptor, WheelhorseActionDescriptor, WheelhorseActionKind, WheelhorseGate,
    WheelhorseGateDecision, WheelhorseGateRecord, derive_queue_item_id, is_red_gated_repo,
    resolve_domain_gate_policy,
};
pub use pairing::{PairingCode, Pending, PendingPairings};
pub use route::{ChannelRule, Decision, Inbound, Route, Routing, decide, route};

/// The agent kind of the assistant that the owner talks to.
pub const ASSISTANT_KIND: &str = "assistant";
