//! The config file of the daemon.
//!
//! ```toml
//! owner = "mark"
//! system = "You are littleguy, a helpful assistant."
//! window = 40
//!
//! [discord]
//! pairing = true
//! allowed = ["123456789012345678"]
//!
//! [[camel.route]]
//! id = "inbox"
//! from = "file:/home/mark/lg-inbox?delete=true"
//! prompt = "A file arrived. Summarize it."
//!
//! [memory]
//! consolidate = "0 3 * * *"
//! timezone = "Europe/London"
//!
//! [brief]
//! schedule = "30 7 * * 1-5"
//! timezone = "Europe/London"
//! to = "123456789012345678"
//!
//! [tools]
//! web_fetch = true
//! web_search = true
//! files = true
//! fetch_max_chars = 20000
//!
//! [approvals]
//! approver = "123456789012345678"
//! timeout_secs = 3600
//!
//! [flow]
//! rule_of_two = true
//! recheck = "each_call"
//!
//! [[mcp.server]]
//! id = "github"
//! command = "github-mcp-server"
//! args = ["stdio"]
//! env = ["GITHUB_PERSONAL_ACCESS_TOKEN"]
//! labels = ["untrusted_input", "private_data"]
//! effect = "external"
//!
//! [[mcp.server]]
//! id = "notes"
//! url = "http://127.0.0.1:8808/mcp"
//! bearer_env = "LG_NOTES_TOKEN"
//! labels = ["private_data"]
//! effect = "write"
//! tools = ["search", "append"]
//!
//! [sandbox]
//! backend = "firecracker"
//!
//! [sandbox.firecracker]
//! firecracker = "/usr/local/bin/firecracker"
//! jailer = "/usr/local/bin/jailer"
//! kernel = "/var/lib/littleguy/vmlinux"
//! rootfs = "/var/lib/littleguy/rootfs.ext4"
//! uid = 990
//! gid = 990
//! netns = "/run/netns/littleguy"
//!
//! [san_mateo]
//! bind = "127.0.0.1:3003"
//!
//! [[san_mateo.integration]]
//! key = "google"
//! provider = "google"
//! client_id = "1234.apps.googleusercontent.com"
//! client_secret_env = "LG_GOOGLE_CLIENT_SECRET"
//! scopes = [
//!     "https://www.googleapis.com/auth/gmail.modify",
//!     "https://www.googleapis.com/auth/calendar.events",
//! ]
//!
//! [google]
//! integration = "google"
//! mail = true
//! calendar = true
//! ```
//!
//! The `[discord]` section is optional. The bot token never goes in the
//! file. It comes from `DISCORD_BOT_TOKEN` only.
//!
//! The `[slack]` section has the same keys, with Slack user ids such as
//! `U012AB3CD`. The tokens come from `SLACK_BOT_TOKEN` and
//! `SLACK_APP_TOKEN` only.
//!
//! Each `[[camel.route]]` is a camel route that feeds the assistant. The
//! owner writes it, so its messages need no pairing.
//!
//! The `[memory]` section is optional. The assistant keeps notes across
//! runs by default. Each night a consolidation pass turns its recent answers
//! into facts in those notes. `consolidate` is a cron expression in
//! `timezone`. Set `enabled = false` to keep no notes.
//!
//! The `[brief]` section turns on a morning brief. At each `schedule` slot,
//! in `timezone`, the assistant writes a short brief. It sends the brief as
//! a direct message to the user `to`, and stays silent when nothing needs the
//! owner. The time zone defaults to the memory time zone.
//!
//! The `[tools]` section is optional. By default the assistant can read a
//! public web page, search the web when `LG_BRAVE_API_KEY` is set, and keep
//! files in its own workspace. Set a key to `false` to take a tool away.
//!
//! The `[approvals]` section names who decides a call that waits for a
//! person. `approver` names the chat user who gets approve and deny
//! buttons: a Discord id, or `slack:` and a Slack id. The owner can always decide with `lg approve` and `lg deny`. A
//! call with no decision after `timeout_secs` is denied.
//!
//! The `[flow]` section sets the flow rules. By default the Rule of Two is
//! on: a call waits for a person when the conversation would then hold
//! untrusted input, private data, and a write or an outside effect. Under
//! `recheck = "each_call"` each such call waits. Under `"until_approved"`
//! one approval releases the rule for the rest of that op.
//!
//! Each `[[mcp.server]]` adds the tools of one MCP server. A server runs as
//! a child process (`command`) or answers at a URL (`url`). A child gets
//! only `PATH`, `HOME` and the variables that `env` names. `bearer_env`
//! names the variable that holds the bearer token of a URL. A URL that is
//! not on this machine must use https. `labels` and `effect` feed the flow
//! rules, and the effect defaults to `external`. `tools` keeps only the
//! named tools. The model sees each tool as `<id>__<tool>`.
//!
//! The `[sandbox]` section gives the assistant a `run_code` tool. Each call
//! waits until a person approves it. The `firecracker` backend runs each
//! call in a new microVM. The `process` backend runs code as a host
//! process. It shares the host kernel, so it is for development only.
//!
//! The `[san_mateo]` section names the OAuth integrations that San Mateo
//! keeps tokens for. A client secret never goes in the file: the file names
//! the variable that holds it.
//!
//! The `[google]` section gives the assistant Gmail and Google Calendar
//! tools on one San Mateo integration with the `google` provider. Each send
//! and each invite waits for a person.

use std::path::{Path, PathBuf};

use autumn_harvest_agent::ToolEffect;
use autumn_harvest_agent::flow::{FlowRules, Recheck};
use serde::Deserialize;
use thiserror::Error;

use crate::ids::{IdError, OwnerName, SenderId};

/// The system prompt when the config names none.
pub const DEFAULT_SYSTEM: &str = "You are littleguy, a small and helpful personal assistant.";

/// The transcript bound when the config names none, in messages.
pub const DEFAULT_WINDOW: u32 = 40;

/// Why a config did not load.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file did not read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The path of the file.
        path: String,
        /// The read error.
        source: std::io::Error,
    },
    /// The text is not valid TOML for this config.
    #[error("the config is not valid: {0}")]
    Parse(#[from] toml::de::Error),
    /// An id in the config is not valid.
    #[error("the config holds a bad id: {0}")]
    Id(#[from] IdError),
    /// The window is zero.
    #[error("window must be at least 1")]
    Window,
    /// A Discord user id is not a snowflake.
    #[error("`{0}` is not a Discord user id")]
    DiscordUser(String),
    /// A Slack user id is not 2 to 32 of A-Z and 0-9, after a `U` or a `W`.
    #[error("`{0}` is not a Slack user id")]
    SlackUser(String),
    /// A camel route id is not 1 to 32 lowercase letters, digits or `-`.
    #[error("`{0}` is not a camel route id: use 1 to 32 of a-z, 0-9 and -")]
    CamelRouteId(String),
    /// Two camel routes have one id.
    #[error("two camel routes have the id `{0}`")]
    CamelRouteTwice(String),
    /// The consolidation schedule is not a five-field cron expression.
    #[error("memory.consolidate `{0}` is not a cron expression such as \"0 3 * * *\"")]
    Consolidate(String),
    /// The time zone is empty or holds a space.
    #[error("the time zone `{0}` is not a time zone name such as \"Europe/London\"")]
    Timezone(String),
    /// The brief schedule is not a five-field cron expression.
    #[error("brief.schedule `{0}` is not a cron expression such as \"30 7 * * 1-5\"")]
    BriefSchedule(String),
    /// The brief prompt is empty.
    #[error("brief.prompt must not be empty")]
    BriefPrompt,
    /// The page size of the fetch tool is zero.
    #[error("tools.fetch_max_chars must be at least 1")]
    FetchMaxChars,
    /// The sandbox section is not valid.
    #[error("the sandbox section is not valid: {0}")]
    Sandbox(String),
    /// The approvals section is not valid.
    #[error("the approvals section is not valid: {0}")]
    Approvals(String),
    /// An MCP server is not valid.
    #[error("mcp server `{id}` is not valid: {why}")]
    Mcp {
        /// The server id as written.
        id: String,
        /// What is wrong.
        why: String,
    },
    /// The San Mateo section is not valid.
    #[error(transparent)]
    SanMateo(#[from] SanMateoError),
    /// The Google section is not valid.
    #[error("the google section is not valid: {0}")]
    Google(String),
    /// The Cortex section is not valid.
    #[error("the cortex section is not valid: {0}")]
    Cortex(String),
    /// A camel route reads from a component that littleguy does not run.
    #[error("camel route `{id}` reads from `{from}`: use one of {}", CAMEL_SCHEMES.join(", "))]
    CamelScheme {
        /// The route id.
        id: String,
        /// The `from` URI.
        from: String,
    },
}

/// A San Mateo setting is not valid.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SanMateoError {
    /// The bind address is not an IP address and port.
    #[error("san_mateo.bind `{0}` is not an address such as 127.0.0.1:3003")]
    Bind(String),
    /// The bind address is not a loopback address.
    #[error("san_mateo.bind `{0}` is not a loopback address: OAuth callbacks stay on this machine")]
    NotLoopback(String),
    /// An integration key is not 1 to 100 of `A-Z a-z 0-9 _ . -`.
    #[error("`{0}` is not an integration key: use 1 to 100 of A-Z, a-z, 0-9, _, . and -")]
    Key(String),
    /// Two integrations have one key.
    #[error("two san_mateo integrations have the key `{0}`")]
    KeyTwice(String),
    /// A variable name is not an environment variable name.
    #[error("`{0}` is not an environment variable name")]
    Variable(String),
}

/// The default cron expression of the nightly consolidation: 03:00.
pub const DEFAULT_CONSOLIDATE: &str = "0 3 * * *";

/// The default time zone of the consolidation schedule.
pub const DEFAULT_TIMEZONE: &str = "UTC";

/// The memory settings of the assistant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryConfig {
    /// Whether the assistant keeps notes and an episode log.
    pub enabled: bool,
    /// The cron expression of the consolidation pass, five fields.
    pub consolidate: String,
    /// The IANA time zone of `consolidate`.
    pub timezone: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            consolidate: String::from(DEFAULT_CONSOLIDATE),
            timezone: String::from(DEFAULT_TIMEZONE),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMemory {
    enabled: Option<bool>,
    consolidate: Option<String>,
    timezone: Option<String>,
}

/// Checks the memory section. The engine checks each cron field and the
/// time zone name when the daemon opens. This check catches a wrong field
/// count early, with a message that names the key.
fn memory(raw: RawMemory) -> Result<MemoryConfig, ConfigError> {
    let defaults = MemoryConfig::default();
    let consolidate = raw.consolidate.unwrap_or(defaults.consolidate);
    if consolidate.split_whitespace().count() != 5 {
        return Err(ConfigError::Consolidate(consolidate));
    }
    let timezone = raw.timezone.unwrap_or(defaults.timezone);
    if timezone.trim().is_empty() || timezone.chars().any(char::is_whitespace) {
        return Err(ConfigError::Timezone(timezone));
    }
    Ok(MemoryConfig {
        enabled: raw.enabled.unwrap_or(defaults.enabled),
        consolidate,
        timezone,
    })
}

/// The default page size of the fetch tool, in characters.
pub const DEFAULT_FETCH_MAX_CHARS: usize = 20_000;

/// The tools of the assistant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolsConfig {
    /// Read public web pages.
    pub web_fetch: bool,
    /// Search the web. It also needs the key in `LG_BRAVE_API_KEY`.
    pub web_search: bool,
    /// Keep files in a persistent workspace.
    pub files: bool,
    /// The most characters of one fetched page.
    pub fetch_max_chars: usize,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            web_fetch: true,
            web_search: true,
            files: true,
            fetch_max_chars: DEFAULT_FETCH_MAX_CHARS,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTools {
    web_fetch: Option<bool>,
    web_search: Option<bool>,
    files: Option<bool>,
    fetch_max_chars: Option<usize>,
}

fn tools(raw: &RawTools) -> Result<ToolsConfig, ConfigError> {
    let defaults = ToolsConfig::default();
    let fetch_max_chars = raw.fetch_max_chars.unwrap_or(defaults.fetch_max_chars);
    if fetch_max_chars == 0 {
        return Err(ConfigError::FetchMaxChars);
    }
    Ok(ToolsConfig {
        web_fetch: raw.web_fetch.unwrap_or(defaults.web_fetch),
        web_search: raw.web_search.unwrap_or(defaults.web_search),
        files: raw.files.unwrap_or(defaults.files),
        fetch_max_chars,
    })
}

/// The default schedule of the morning brief: 07:30 on weekdays.
pub const DEFAULT_BRIEF_SCHEDULE: &str = "30 7 * * 1-5";

/// The default instruction of the morning brief. It names the silent
/// answer, so a morning with nothing to say sends nothing.
pub const DEFAULT_BRIEF_PROMPT: &str = "It is time for the morning brief. From your notes \
     and the recent conversation, list in a few short lines what needs the owner today: \
     open questions, promises, reminders and anything that changed. Do not invent \
     items. If nothing needs the owner, reply with exactly HEARTBEAT_OK.";

/// The settings of the morning brief.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefConfig {
    /// The cron expression of the brief, five fields.
    pub schedule: String,
    /// The IANA time zone of `schedule`.
    pub timezone: String,
    /// The instruction that starts each brief.
    pub prompt: String,
    /// The chat user that gets the brief, on Discord or Slack. `None`
    /// writes it to the log only.
    pub to: Option<SenderId>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBrief {
    schedule: Option<String>,
    timezone: Option<String>,
    prompt: Option<String>,
    to: Option<String>,
}

/// Checks the brief section. Its time zone defaults to `memory_timezone`.
fn brief(raw: RawBrief, memory_timezone: &str) -> Result<BriefConfig, ConfigError> {
    let schedule = raw
        .schedule
        .unwrap_or_else(|| String::from(DEFAULT_BRIEF_SCHEDULE));
    if schedule.split_whitespace().count() != 5 {
        return Err(ConfigError::BriefSchedule(schedule));
    }
    let timezone = raw.timezone.unwrap_or_else(|| memory_timezone.to_owned());
    if timezone.trim().is_empty() || timezone.chars().any(char::is_whitespace) {
        return Err(ConfigError::Timezone(timezone));
    }
    let prompt = raw
        .prompt
        .unwrap_or_else(|| String::from(DEFAULT_BRIEF_PROMPT));
    if prompt.trim().is_empty() {
        return Err(ConfigError::BriefPrompt);
    }
    Ok(BriefConfig {
        schedule,
        timezone,
        prompt,
        to: raw.to.as_deref().map(chat_user).transpose()?,
    })
}

/// The San Mateo provider that the Google tools need.
pub const GOOGLE_PROVIDER: &str = "google";

/// The Gmail and Google Calendar tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleConfig {
    /// The San Mateo integration that holds the Google connection.
    pub integration: String,
    /// The Gmail tools.
    pub mail: bool,
    /// The Google Calendar tools.
    pub calendar: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGoogle {
    integration: String,
    mail: Option<bool>,
    calendar: Option<bool>,
}

/// Checks the Google section against the San Mateo integrations.
fn google(raw: RawGoogle, san_mateo: Option<&SanMateoConfig>) -> Result<GoogleConfig, ConfigError> {
    let found = san_mateo
        .into_iter()
        .flat_map(|section| &section.integrations)
        .find(|integration| integration.key == raw.integration);
    match found {
        None => {
            return Err(ConfigError::Google(format!(
                "integration `{}` is not a [[san_mateo.integration]]",
                raw.integration
            )));
        }
        Some(integration) if integration.provider != GOOGLE_PROVIDER => {
            return Err(ConfigError::Google(format!(
                "integration `{}` has provider `{}`: use provider `{GOOGLE_PROVIDER}`",
                raw.integration, integration.provider
            )));
        }
        Some(_) => {}
    }
    let config = GoogleConfig {
        integration: raw.integration,
        mail: raw.mail.unwrap_or(true),
        calendar: raw.calendar.unwrap_or(true),
    };
    if !config.mail && !config.calendar {
        return Err(ConfigError::Google(String::from(
            "mail and calendar are both off: leave the section out",
        )));
    }
    Ok(config)
}

/// The default San Mateo address.
pub const DEFAULT_SAN_MATEO_BIND: &str = "127.0.0.1:3003";

/// The camel components that a route may read from.
pub const CAMEL_SCHEMES: [&str; 2] = ["file", "timer"];

/// The longest camel route id, in bytes.
pub const MAX_CAMEL_ROUTE_ID: usize = 32;

/// Where the `run_code` tool runs code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBackend {
    /// A new Firecracker microVM for each call, behind the jailer.
    Firecracker(FirecrackerConfig),
    /// A host process with limits. It shares the host kernel, so it does not
    /// contain hostile code. Use it for development only.
    Process,
}

/// The Firecracker files and the jail of the `firecracker` backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirecrackerConfig {
    /// The Firecracker binary.
    pub firecracker: PathBuf,
    /// The jailer binary.
    pub jailer: PathBuf,
    /// The uncompressed guest kernel.
    pub kernel: PathBuf,
    /// The ext4 root file system of the guest, with the guest agent.
    pub rootfs: PathBuf,
    /// The uid of the jailed VMM.
    pub uid: u32,
    /// The gid of the jailed VMM.
    pub gid: u32,
    /// An empty network namespace for the VMM, under `/run/netns`.
    pub netns: Option<PathBuf>,
}

/// The `run_code` tool. Each call waits for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    /// Where the code runs.
    pub backend: SandboxBackend,
    /// Whether the code may use the network of the backend.
    pub network: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSandbox {
    backend: String,
    network: Option<bool>,
    firecracker: Option<RawFirecracker>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFirecracker {
    firecracker: PathBuf,
    jailer: PathBuf,
    kernel: PathBuf,
    rootfs: PathBuf,
    uid: u32,
    gid: u32,
    netns: Option<PathBuf>,
}

/// Checks the sandbox section.
fn sandbox(raw: RawSandbox) -> Result<SandboxConfig, ConfigError> {
    let backend = match (raw.backend.as_str(), raw.firecracker) {
        ("firecracker", Some(fc)) => SandboxBackend::Firecracker(FirecrackerConfig {
            firecracker: fc.firecracker,
            jailer: fc.jailer,
            kernel: fc.kernel,
            rootfs: fc.rootfs,
            uid: fc.uid,
            gid: fc.gid,
            netns: fc.netns,
        }),
        ("firecracker", None) => {
            return Err(ConfigError::Sandbox(String::from(
                "the firecracker backend needs a [sandbox.firecracker] section",
            )));
        }
        ("process", None) => SandboxBackend::Process,
        ("process", Some(_)) => {
            return Err(ConfigError::Sandbox(String::from(
                "the process backend takes no [sandbox.firecracker] section",
            )));
        }
        (other, _) => {
            return Err(ConfigError::Sandbox(format!(
                "backend `{other}` is not one of firecracker, process"
            )));
        }
    };
    Ok(SandboxConfig {
        backend,
        network: raw.network.unwrap_or(false),
    })
}

/// The approval wait of a call when the config names none: one hour.
pub const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 3_600;

/// Who decides a call that waits, and how long it waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalsConfig {
    /// The chat user who gets the approve and deny buttons, on Discord or
    /// Slack.
    pub approver: Option<SenderId>,
    /// How long a call waits for a decision. A call with no decision is
    /// denied.
    pub timeout_secs: u64,
}

impl Default for ApprovalsConfig {
    fn default() -> Self {
        Self {
            approver: None,
            timeout_secs: DEFAULT_APPROVAL_TIMEOUT_SECS,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApprovals {
    approver: Option<String>,
    timeout_secs: Option<u64>,
}

/// Checks the approvals section.
fn approvals(raw: &RawApprovals) -> Result<ApprovalsConfig, ConfigError> {
    let timeout_secs = raw.timeout_secs.unwrap_or(DEFAULT_APPROVAL_TIMEOUT_SECS);
    if timeout_secs == 0 {
        return Err(ConfigError::Approvals(String::from(
            "timeout_secs must be at least 1",
        )));
    }
    Ok(ApprovalsConfig {
        approver: raw.approver.as_deref().map(chat_user).transpose()?,
        timeout_secs,
    })
}

/// When a flow rule asks again after a person approved a call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowRecheck {
    /// Each call that trips a rule waits for a person.
    #[default]
    EachCall,
    /// One approval releases the rule for the rest of the op.
    UntilApproved,
}

/// The flow rules of the assistant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowConfig {
    /// Whether the Rule of Two holds.
    pub rule_of_two: bool,
    /// When a rule asks again.
    pub recheck: FlowRecheck,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            rule_of_two: true,
            recheck: FlowRecheck::EachCall,
        }
    }
}

impl FlowConfig {
    /// The engine rules of this config. They are empty when the Rule of Two
    /// is off.
    #[must_use]
    pub fn rules(&self) -> FlowRules {
        if !self.rule_of_two {
            return FlowRules::new();
        }
        let recheck = match self.recheck {
            FlowRecheck::EachCall => Recheck::EachCall,
            FlowRecheck::UntilApproved => Recheck::UntilApproved,
        };
        FlowRules::rule_of_two().with_recheck(recheck)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFlow {
    rule_of_two: Option<bool>,
    recheck: Option<FlowRecheck>,
}

/// How long one MCP call may take when the config names no bound.
pub const DEFAULT_MCP_TIMEOUT_SECS: u64 = 60;

/// The longest MCP server id, in bytes.
pub const MAX_MCP_SERVER_ID: usize = 16;

/// The longest flow label, in bytes.
pub const MAX_LABEL: usize = 64;

/// How littleguy reaches an MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    /// A child process that speaks MCP on its stdin and stdout.
    Stdio {
        /// The program.
        command: String,
        /// Its arguments.
        args: Vec<String>,
        /// The variables of the daemon that the child also gets.
        env: Vec<String>,
    },
    /// A server that answers streamable HTTP at a URL.
    Http {
        /// The URL of the MCP endpoint.
        url: String,
        /// The variable that holds the bearer token.
        bearer_env: Option<String>,
    },
}

/// One MCP server whose tools the assistant may call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// The server id. Each tool name starts with `<id>__`.
    pub id: String,
    /// How littleguy reaches the server.
    pub transport: McpTransport,
    /// The flow labels of each tool of the server.
    pub labels: Vec<String>,
    /// What each tool of the server can change.
    pub effect: ToolEffect,
    /// The tools to keep, by their MCP names. `None` keeps every tool.
    pub tools: Option<Vec<String>>,
    /// How long one call may take, in seconds.
    pub timeout_secs: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMcp {
    #[serde(default)]
    server: Vec<RawMcpServer>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMcpServer {
    id: String,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    url: Option<String>,
    bearer_env: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    effect: Option<ToolEffect>,
    tools: Option<Vec<String>>,
    timeout_secs: Option<u64>,
}

/// Whether `url` may carry a bearer token: https, or http to this machine.
fn safe_mcp_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "https" => parsed.host().is_some(),
        "http" => match parsed.host() {
            Some(url::Host::Domain(name)) => name == "localhost",
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        },
        _ => false,
    }
}

/// Checks one MCP server.
fn mcp_server(raw: RawMcpServer) -> Result<McpServer, ConfigError> {
    let fail = |why: &str| ConfigError::Mcp {
        id: raw.id.clone(),
        why: why.to_owned(),
    };
    let plain = !raw.id.is_empty()
        && raw.id.len() <= MAX_MCP_SERVER_ID
        && raw
            .id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !plain {
        return Err(fail("the id needs 1 to 16 of a-z, 0-9 and -"));
    }
    if let Some(var) = raw
        .env
        .iter()
        .chain(&raw.bearer_env)
        .find(|v| !is_variable(v))
    {
        return Err(fail(&format!(
            "`{var}` is not an environment variable name"
        )));
    }
    let label_ok = |label: &String| {
        !label.is_empty()
            && label.len() <= MAX_LABEL
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_:.-".contains(&b))
    };
    if let Some(label) = raw.labels.iter().find(|l| !label_ok(l)) {
        return Err(fail(&format!(
            "`{label}` is not a label: use 1 to 64 of a-z, 0-9, _, :, . and -"
        )));
    }
    if raw.tools.as_ref().is_some_and(Vec::is_empty) {
        return Err(fail("an empty tools list keeps no tool: leave it out"));
    }
    let timeout_secs = raw.timeout_secs.unwrap_or(DEFAULT_MCP_TIMEOUT_SECS);
    if timeout_secs == 0 {
        return Err(fail("timeout_secs must be at least 1"));
    }
    let transport = match (raw.command, raw.url) {
        (Some(command), None) => {
            if raw.bearer_env.is_some() {
                return Err(fail("bearer_env needs a url"));
            }
            if command.trim().is_empty() {
                return Err(fail("the command is empty"));
            }
            McpTransport::Stdio {
                command,
                args: raw.args,
                env: raw.env,
            }
        }
        (None, Some(url)) => {
            if !raw.args.is_empty() || !raw.env.is_empty() {
                return Err(fail("args and env need a command"));
            }
            if !safe_mcp_url(&url) {
                return Err(fail(
                    "the url must use https, or http to localhost or a loopback address",
                ));
            }
            McpTransport::Http {
                url,
                bearer_env: raw.bearer_env,
            }
        }
        _ => return Err(fail("set exactly one of command and url")),
    };
    Ok(McpServer {
        id: raw.id,
        transport,
        labels: raw.labels,
        effect: raw.effect.unwrap_or(ToolEffect::External),
        tools: raw.tools,
        timeout_secs,
    })
}

/// Checks the MCP servers of a config.
fn mcp_servers(raw: Vec<RawMcpServer>) -> Result<Vec<McpServer>, ConfigError> {
    let mut seen = std::collections::BTreeSet::new();
    raw.into_iter()
        .map(|raw| {
            let server = mcp_server(raw)?;
            if !seen.insert(server.id.clone()) {
                return Err(ConfigError::Mcp {
                    id: server.id,
                    why: String::from("two servers have this id"),
                });
            }
            Ok(server)
        })
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    owner: String,
    system: Option<String>,
    window: Option<u32>,
    #[serde(default)]
    discord: RawDiscord,
    #[serde(default)]
    slack: RawDiscord,
    #[serde(default)]
    camel: RawCamel,
    san_mateo: Option<RawSanMateo>,
    google: Option<RawGoogle>,
    #[serde(default)]
    memory: RawMemory,
    brief: Option<RawBrief>,
    #[serde(default)]
    tools: RawTools,
    sandbox: Option<RawSandbox>,
    #[serde(default)]
    approvals: RawApprovals,
    #[serde(default)]
    flow: RawFlow,
    #[serde(default)]
    cortex: RawCortex,
    #[serde(default)]
    mcp: RawMcp,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCortex {
    wake: Option<bool>,
    wake_socket: Option<String>,
}

/// The Cortex mesh wake settings of `lg serve` (`specs/0007`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CortexConfig {
    /// Listen on the `lane-wake` Unix socket for `cortex-edge` channel wakes.
    pub wake: bool,
    /// Explicit Unix socket path override; when `None` and `wake` is `true`,
    /// `lg serve` derives `${CORTEX_STATE_DIR:-$HOME/.cortex}/lane-wake/<safe_pane>.sock`
    /// from `HERDR_SESSION` and `HERDR_PANE_ID`.
    pub wake_socket: Option<PathBuf>,
}

fn cortex(raw: RawCortex) -> Result<CortexConfig, ConfigError> {
    let wake_socket = match raw.wake_socket {
        Some(path) => {
            if path.trim().is_empty() {
                return Err(ConfigError::Cortex(String::from(
                    "wake_socket must not be empty",
                )));
            }
            Some(PathBuf::from(path))
        }
        None => None,
    };
    let wake = raw.wake.unwrap_or_else(|| wake_socket.is_some());
    Ok(CortexConfig { wake, wake_socket })
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSanMateo {
    bind: Option<String>,
    #[serde(default)]
    integration: Vec<RawIntegration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIntegration {
    key: String,
    provider: String,
    client_id: Option<String>,
    client_secret_env: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
}

/// One OAuth integration that San Mateo keeps tokens for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integration {
    /// The integration key, such as `google`.
    pub key: String,
    /// The provider name in the San Mateo catalog.
    pub provider: String,
    /// The OAuth client id.
    pub client_id: Option<String>,
    /// The variable that holds the OAuth client secret.
    pub client_secret_env: Option<String>,
    /// The scopes to ask for.
    pub scopes: Vec<String>,
}

/// The San Mateo settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanMateoConfig {
    /// The loopback address of the connect pages and the OAuth callback.
    pub bind: std::net::SocketAddr,
    /// The integrations.
    pub integrations: Vec<Integration>,
}

/// Whether `name` is a plain environment variable name.
fn is_variable(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Checks the San Mateo section of a config.
fn san_mateo(raw: RawSanMateo) -> Result<SanMateoConfig, SanMateoError> {
    let text = raw
        .bind
        .unwrap_or_else(|| String::from(DEFAULT_SAN_MATEO_BIND));
    let bind: std::net::SocketAddr = text
        .parse()
        .map_err(|_| SanMateoError::Bind(text.clone()))?;
    if !bind.ip().is_loopback() {
        return Err(SanMateoError::NotLoopback(text));
    }
    let mut seen = std::collections::BTreeSet::new();
    let integrations = raw
        .integration
        .into_iter()
        .map(|i| {
            let plain = !i.key.is_empty()
                && i.key.len() <= 100
                && i.key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
            if !plain {
                return Err(SanMateoError::Key(i.key));
            }
            if !seen.insert(i.key.clone()) {
                return Err(SanMateoError::KeyTwice(i.key));
            }
            if let Some(var) = i.client_secret_env.as_deref().filter(|v| !is_variable(v)) {
                return Err(SanMateoError::Variable(var.to_owned()));
            }
            Ok(Integration {
                key: i.key,
                provider: i.provider,
                client_id: i.client_id.filter(|c| !c.is_empty()),
                client_secret_env: i.client_secret_env,
                scopes: i.scopes,
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(SanMateoConfig { bind, integrations })
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCamel {
    #[serde(default)]
    route: Vec<RawCamelRoute>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCamelRoute {
    id: String,
    from: String,
    prompt: Option<String>,
}

/// One camel route that feeds the assistant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CamelRoute {
    /// The route id. The sender of its messages is `camel:<id>`.
    pub id: String,
    /// The camel URI that the route reads from, such as `file:/inbox`.
    pub from: String,
    /// Text that goes before each message body.
    pub prompt: Option<String>,
}

impl CamelRoute {
    /// The sender id of the messages of this route.
    ///
    /// # Errors
    ///
    /// Never for a checked route: its id is short and plain.
    pub fn sender(&self) -> Result<SenderId, IdError> {
        SenderId::new(format!("camel:{}", self.id))
    }
}

/// Checks the camel routes of a config.
fn camel_routes(raw: Vec<RawCamelRoute>) -> Result<Vec<CamelRoute>, ConfigError> {
    let mut seen = std::collections::BTreeSet::new();
    raw.into_iter()
        .map(|route| {
            let plain = !route.id.is_empty()
                && route.id.len() <= MAX_CAMEL_ROUTE_ID
                && route
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !plain {
                return Err(ConfigError::CamelRouteId(route.id));
            }
            if !seen.insert(route.id.clone()) {
                return Err(ConfigError::CamelRouteTwice(route.id));
            }
            let scheme = route.from.split_once(':').map(|(scheme, _)| scheme);
            if !scheme.is_some_and(|scheme| CAMEL_SCHEMES.contains(&scheme)) {
                return Err(ConfigError::CamelScheme {
                    id: route.id,
                    from: route.from,
                });
            }
            Ok(CamelRoute {
                id: route.id,
                from: route.from,
                prompt: route.prompt.filter(|prompt| !prompt.trim().is_empty()),
            })
        })
        .collect()
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDiscord {
    pairing: Option<bool>,
    #[serde(default)]
    allowed: Vec<String>,
}

/// The settings of one chat service: Discord or Slack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatConfig {
    /// A stranger who sends a direct message gets a pairing code.
    pub pairing: bool,
    /// The users that may talk to the assistant from the start.
    pub allowed: Vec<SenderId>,
}

/// The Discord settings.
pub type DiscordConfig = ChatConfig;

/// The Slack settings.
pub type SlackConfig = ChatConfig;

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            pairing: true,
            allowed: Vec::new(),
        }
    }
}

/// The sender id of the Discord user `user_id`.
///
/// # Errors
///
/// Returns an error when `user_id` is not a Discord snowflake: 1 to 20
/// digits.
pub fn discord_sender(user_id: &str) -> Result<SenderId, ConfigError> {
    let digits =
        !user_id.is_empty() && user_id.len() <= 20 && user_id.bytes().all(|b| b.is_ascii_digit());
    if !digits {
        return Err(ConfigError::DiscordUser(user_id.to_owned()));
    }
    Ok(SenderId::new(format!("discord:{user_id}"))?)
}

/// The sender id of the Slack user `user_id`.
///
/// # Errors
///
/// Returns an error when `user_id` is not 2 to 32 uppercase letters and
/// digits that start with `U` or `W`.
pub fn slack_sender(user_id: &str) -> Result<SenderId, ConfigError> {
    let valid = (2..=32).contains(&user_id.len())
        && (user_id.starts_with('U') || user_id.starts_with('W'))
        && user_id
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
    if !valid {
        return Err(ConfigError::SlackUser(user_id.to_owned()));
    }
    Ok(SenderId::new(format!("slack:{user_id}"))?)
}

/// The sender id of one chat user in the config. `slack:U123` names a
/// Slack user. `discord:123` or a bare snowflake names a Discord user.
///
/// # Errors
///
/// Returns an error when the id does not fit its service.
pub fn chat_user(text: &str) -> Result<SenderId, ConfigError> {
    if let Some(user) = text.strip_prefix("slack:") {
        return slack_sender(user);
    }
    discord_sender(text.strip_prefix("discord:").unwrap_or(text))
}

/// The checked config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The owner. The assistant is `assistant/<owner>`.
    pub owner: OwnerName,
    /// The system prompt of the assistant.
    pub system: String,
    /// The transcript bound of the assistant, in messages.
    pub window: u32,
    /// The Discord settings.
    pub discord: DiscordConfig,
    /// The Slack settings.
    pub slack: SlackConfig,
    /// The camel routes that feed the assistant.
    pub camel: Vec<CamelRoute>,
    /// The San Mateo settings, when the config has the section.
    pub san_mateo: Option<SanMateoConfig>,
    /// The Gmail and Google Calendar tools, when the config has the section.
    pub google: Option<GoogleConfig>,
    /// The memory settings.
    pub memory: MemoryConfig,
    /// The morning brief, when the config has the section.
    pub brief: Option<BriefConfig>,
    /// The tools of the assistant.
    pub tools: ToolsConfig,
    /// The `run_code` tool, when the config has the section.
    pub sandbox: Option<SandboxConfig>,
    /// Who decides a call that waits.
    pub approvals: ApprovalsConfig,
    /// The flow rules.
    pub flow: FlowConfig,
    /// The Cortex mesh wake settings.
    pub cortex: CortexConfig,
    /// The MCP servers.
    pub mcp: Vec<McpServer>,
}

impl Config {
    /// A config for `owner` with every default.
    #[must_use]
    pub fn for_owner(owner: OwnerName) -> Self {
        Self {
            owner,
            system: String::from(DEFAULT_SYSTEM),
            window: DEFAULT_WINDOW,
            discord: DiscordConfig::default(),
            slack: SlackConfig::default(),
            camel: Vec::new(),
            san_mateo: None,
            google: None,
            memory: MemoryConfig::default(),
            brief: None,
            tools: ToolsConfig::default(),
            sandbox: None,
            approvals: ApprovalsConfig::default(),
            flow: FlowConfig::default(),
            cortex: CortexConfig::default(),
            mcp: Vec::new(),
        }
    }

    /// Parses and checks a config text.
    ///
    /// # Errors
    ///
    /// Returns an error when the text is not valid TOML, names an unknown
    /// key, holds a bad owner name or Discord user id, sets a zero window,
    /// or holds a bad camel route, memory or San Mateo setting.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let raw: Raw = toml::from_str(text)?;
        let window = raw.window.unwrap_or(DEFAULT_WINDOW);
        if window == 0 {
            return Err(ConfigError::Window);
        }
        let allowed = raw
            .discord
            .allowed
            .iter()
            .map(|id| discord_sender(id))
            .collect::<Result<_, _>>()?;
        let slack_allowed = raw
            .slack
            .allowed
            .iter()
            .map(|id| slack_sender(id))
            .collect::<Result<_, _>>()?;
        let memory = memory(raw.memory)?;
        let san_mateo_section = raw.san_mateo.map(san_mateo).transpose()?;
        let google_section = raw
            .google
            .map(|raw| google(raw, san_mateo_section.as_ref()))
            .transpose()?;
        let brief = raw
            .brief
            .map(|raw| brief(raw, &memory.timezone))
            .transpose()?;
        Ok(Self {
            owner: OwnerName::new(raw.owner)?,
            system: raw.system.unwrap_or_else(|| String::from(DEFAULT_SYSTEM)),
            window,
            discord: DiscordConfig {
                pairing: raw.discord.pairing.unwrap_or(true),
                allowed,
            },
            slack: SlackConfig {
                pairing: raw.slack.pairing.unwrap_or(true),
                allowed: slack_allowed,
            },
            camel: camel_routes(raw.camel.route)?,
            san_mateo: san_mateo_section,
            google: google_section,
            memory,
            brief,
            tools: tools(&raw.tools)?,
            sandbox: raw.sandbox.map(sandbox).transpose()?,
            approvals: approvals(&raw.approvals)?,
            flow: FlowConfig {
                rule_of_two: raw.flow.rule_of_two.unwrap_or(true),
                recheck: raw.flow.recheck.unwrap_or_default(),
            },
            cortex: cortex(raw.cortex)?,
            mcp: mcp_servers(raw.mcp.server)?,
        })
    }

    /// Reads and checks the config file at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error when the file does not read or does not parse.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_config_takes_the_defaults() {
        let config = Config::parse("owner = \"mark\"").unwrap();
        assert_eq!(config, Config::for_owner(OwnerName::new("mark").unwrap()));
    }

    #[test]
    fn the_tools_section_is_read_with_its_defaults() {
        let plain = Config::parse("owner = \"mark\"").unwrap();
        assert_eq!(plain.tools, ToolsConfig::default());
        let config = Config::parse(
            "owner = \"mark\"\n[tools]\nweb_search = false\nfiles = false\nfetch_max_chars = 500",
        )
        .unwrap();
        assert_eq!(
            config.tools,
            ToolsConfig {
                web_fetch: true,
                web_search: false,
                files: false,
                fetch_max_chars: 500,
            }
        );
    }

    #[test]
    fn a_bad_tools_section_is_refused() {
        let zero = Config::parse("owner = \"mark\"\n[tools]\nfetch_max_chars = 0");
        assert!(matches!(zero, Err(ConfigError::FetchMaxChars)));
        let unknown = Config::parse("owner = \"mark\"\n[tools]\nshell = true");
        assert!(unknown.is_err());
    }

    #[test]
    fn the_sandbox_section_is_read() {
        let plain = Config::parse("owner = \"mark\"").unwrap();
        assert_eq!(plain.sandbox, None);
        let process = Config::parse("owner = \"mark\"\n[sandbox]\nbackend = \"process\"").unwrap();
        assert_eq!(
            process.sandbox,
            Some(SandboxConfig {
                backend: SandboxBackend::Process,
                network: false,
            })
        );
        let vm = Config::parse(
            "owner = \"mark\"\n[sandbox]\nbackend = \"firecracker\"\nnetwork = true\n\
             [sandbox.firecracker]\nfirecracker = \"/fc\"\n\
             jailer = \"/j\"\nkernel = \"/k\"\nrootfs = \"/r\"\nuid = 9\ngid = 8",
        )
        .unwrap()
        .sandbox
        .unwrap();
        assert!(vm.network);
        assert_eq!(
            vm.backend,
            SandboxBackend::Firecracker(FirecrackerConfig {
                firecracker: PathBuf::from("/fc"),
                jailer: PathBuf::from("/j"),
                kernel: PathBuf::from("/k"),
                rootfs: PathBuf::from("/r"),
                uid: 9,
                gid: 8,
                netns: None,
            })
        );
    }

    #[test]
    fn a_bad_sandbox_section_is_refused() {
        let bad = |section: &str| {
            let text = format!("owner = \"mark\"\n[sandbox]\n{section}");
            match Config::parse(&text) {
                Err(ConfigError::Sandbox(why)) => why,
                other => panic!("{section}: {other:?}"),
            }
        };
        assert!(bad("backend = \"docker\"").contains("docker"));
        assert!(bad("backend = \"firecracker\"").contains("[sandbox.firecracker]"));
        let fc = "[sandbox.firecracker]\nfirecracker = \"/fc\"\njailer = \"/j\"\n\
                  kernel = \"/k\"\nrootfs = \"/r\"\nuid = 1\ngid = 1";
        assert!(bad(&format!("backend = \"process\"\n{fc}")).contains("no [sandbox"));
        let missing = Config::parse("owner = \"mark\"\n[sandbox]\nnetwork = true");
        assert!(missing.is_err());
        // The approver moved to [approvals]. The old key fails loudly.
        let moved =
            Config::parse("owner = \"mark\"\n[sandbox]\nbackend = \"process\"\napprover = \"42\"");
        assert!(moved.is_err());
    }

    #[test]
    fn the_approvals_section_is_read() {
        let plain = Config::parse("owner = \"mark\"").unwrap();
        assert_eq!(plain.approvals, ApprovalsConfig::default());
        assert_eq!(plain.approvals.timeout_secs, DEFAULT_APPROVAL_TIMEOUT_SECS);
        let set =
            Config::parse("owner = \"mark\"\n[approvals]\napprover = \"42\"\ntimeout_secs = 60")
                .unwrap();
        assert_eq!(
            set.approvals,
            ApprovalsConfig {
                approver: Some(SenderId::new("discord:42").unwrap()),
                timeout_secs: 60,
            }
        );
        let zero = Config::parse("owner = \"mark\"\n[approvals]\ntimeout_secs = 0");
        assert!(matches!(zero, Err(ConfigError::Approvals(why)) if why.contains("at least 1")));
        let user = Config::parse("owner = \"mark\"\n[approvals]\napprover = \"x\"");
        assert!(matches!(user, Err(ConfigError::DiscordUser(_))));
    }

    #[test]
    fn the_rule_of_two_is_on_by_default() {
        let plain = Config::parse("owner = \"mark\"").unwrap();
        assert_eq!(plain.flow, FlowConfig::default());
        assert_eq!(plain.flow.rules(), FlowRules::rule_of_two());
        let until =
            Config::parse("owner = \"mark\"\n[flow]\nrecheck = \"until_approved\"").unwrap();
        assert_eq!(until.flow.recheck, FlowRecheck::UntilApproved);
        assert!(until.flow.rules().can_release());
        assert!(!plain.flow.rules().can_release());
        let off = Config::parse("owner = \"mark\"\n[flow]\nrule_of_two = false").unwrap();
        assert!(off.flow.rules().is_empty());
        let bad = Config::parse("owner = \"mark\"\n[flow]\nrecheck = \"never\"");
        assert!(bad.is_err());
    }

    #[test]
    fn mcp_servers_are_read() {
        let config = Config::parse(
            "owner = \"mark\"\n\
             [[mcp.server]]\nid = \"gh\"\ncommand = \"gh-mcp\"\nargs = [\"stdio\"]\n\
             env = [\"GH_TOKEN\"]\nlabels = [\"untrusted_input\", \"private_data\"]\n\
             [[mcp.server]]\nid = \"notes\"\nurl = \"http://127.0.0.1:8808/mcp\"\n\
             bearer_env = \"LG_NOTES\"\neffect = \"read_only\"\ntools = [\"search\"]\n\
             timeout_secs = 5",
        )
        .unwrap();
        assert_eq!(
            config.mcp,
            vec![
                McpServer {
                    id: String::from("gh"),
                    transport: McpTransport::Stdio {
                        command: String::from("gh-mcp"),
                        args: vec![String::from("stdio")],
                        env: vec![String::from("GH_TOKEN")],
                    },
                    labels: vec![
                        String::from("untrusted_input"),
                        String::from("private_data")
                    ],
                    effect: ToolEffect::External,
                    tools: None,
                    timeout_secs: DEFAULT_MCP_TIMEOUT_SECS,
                },
                McpServer {
                    id: String::from("notes"),
                    transport: McpTransport::Http {
                        url: String::from("http://127.0.0.1:8808/mcp"),
                        bearer_env: Some(String::from("LG_NOTES")),
                    },
                    labels: Vec::new(),
                    effect: ToolEffect::ReadOnly,
                    tools: Some(vec![String::from("search")]),
                    timeout_secs: 5,
                },
            ]
        );
    }

    #[test]
    fn a_bad_mcp_server_is_refused() {
        let bad = |body: &str| {
            let text = format!("owner = \"mark\"\n[[mcp.server]]\n{body}");
            match Config::parse(&text) {
                Err(ConfigError::Mcp { why, .. }) => why,
                other => panic!("{body}: {other:?}"),
            }
        };
        assert!(bad("id = \"Bad\"\ncommand = \"x\"").contains("a-z"));
        assert!(bad("id = \"a-very-long-server-id\"\ncommand = \"x\"").contains("1 to 16"));
        assert!(bad("id = \"a\"").contains("exactly one"));
        assert!(
            bad("id = \"a\"\ncommand = \"x\"\nurl = \"https://h/mcp\"").contains("exactly one")
        );
        assert!(bad("id = \"a\"\ncommand = \" \"").contains("empty"));
        assert!(bad("id = \"a\"\ncommand = \"x\"\nenv = [\"lower\"]").contains("`lower`"));
        assert!(bad("id = \"a\"\ncommand = \"x\"\nbearer_env = \"T\"").contains("needs a url"));
        assert!(
            bad("id = \"a\"\nurl = \"https://h/mcp\"\nargs = [\"x\"]").contains("need a command")
        );
        assert!(bad("id = \"a\"\nurl = \"http://example.com/mcp\"").contains("https"));
        assert!(bad("id = \"a\"\nurl = \"ftp://127.0.0.1/mcp\"").contains("https"));
        assert!(bad("id = \"a\"\nurl = \"not a url\"").contains("https"));
        assert!(bad("id = \"a\"\ncommand = \"x\"\nlabels = [\"Has Space\"]").contains("label"));
        assert!(bad("id = \"a\"\ncommand = \"x\"\ntools = []").contains("empty tools"));
        assert!(bad("id = \"a\"\ncommand = \"x\"\ntimeout_secs = 0").contains("at least 1"));
        let twice = Config::parse(
            "owner = \"mark\"\n[[mcp.server]]\nid = \"a\"\ncommand = \"x\"\n\
             [[mcp.server]]\nid = \"a\"\ncommand = \"y\"",
        );
        assert!(matches!(twice, Err(ConfigError::Mcp { why, .. }) if why.contains("two servers")));
        let effect = Config::parse(
            "owner = \"mark\"\n[[mcp.server]]\nid = \"a\"\ncommand = \"x\"\neffect = \"loud\"",
        );
        assert!(effect.is_err());
    }

    #[test]
    fn an_http_mcp_url_is_safe_only_on_this_machine() {
        assert!(safe_mcp_url("https://mcp.example.com/mcp"));
        assert!(safe_mcp_url("http://localhost:8808/mcp"));
        assert!(safe_mcp_url("http://127.0.0.1/mcp"));
        assert!(safe_mcp_url("http://[::1]:9/mcp"));
        assert!(!safe_mcp_url("http://10.0.0.2/mcp"));
        assert!(!safe_mcp_url("http://localhost.example.com/mcp"));
        assert!(!safe_mcp_url("file:///etc/passwd"));
    }

    #[test]
    fn the_memory_section_is_read() {
        let config = Config::parse(
            "owner = \"mark\"\n[memory]\nenabled = false\nconsolidate = \"30 2 * * 1-5\"\ntimezone = \"Europe/London\"",
        )
        .unwrap();
        assert_eq!(
            config.memory,
            MemoryConfig {
                enabled: false,
                consolidate: String::from("30 2 * * 1-5"),
                timezone: String::from("Europe/London"),
            }
        );
    }

    #[test]
    fn a_bad_memory_section_is_refused() {
        let parse =
            |section: &str| Config::parse(&format!("owner = \"mark\"\n[memory]\n{section}"));
        assert!(matches!(
            parse("consolidate = \"every night\""),
            Err(ConfigError::Consolidate(_))
        ));
        assert!(matches!(
            parse("timezone = \"\""),
            Err(ConfigError::Timezone(_))
        ));
        assert!(matches!(parse("other = 1"), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn the_brief_section_is_read_with_its_defaults() {
        let config = Config::parse(
            "owner = \"mark\"\n[memory]\ntimezone = \"Europe/London\"\n[brief]\nto = \"123\"",
        )
        .unwrap();
        assert_eq!(
            config.brief,
            Some(BriefConfig {
                schedule: String::from(DEFAULT_BRIEF_SCHEDULE),
                timezone: String::from("Europe/London"),
                prompt: String::from(DEFAULT_BRIEF_PROMPT),
                to: Some(discord_sender("123").unwrap()),
            })
        );
        assert!(DEFAULT_BRIEF_PROMPT.contains("HEARTBEAT_OK"));
        let bare = Config::parse("owner = \"mark\"\n[brief]\nschedule = \"0 8 * * *\"").unwrap();
        let brief = bare.brief.unwrap();
        assert_eq!(brief.timezone, DEFAULT_TIMEZONE);
        assert_eq!(brief.to, None);
    }

    #[test]
    fn a_bad_brief_section_is_refused() {
        let parse = |section: &str| Config::parse(&format!("owner = \"mark\"\n[brief]\n{section}"));
        assert!(matches!(
            parse("schedule = \"at seven\""),
            Err(ConfigError::BriefSchedule(_))
        ));
        assert!(matches!(
            parse("timezone = \" \""),
            Err(ConfigError::Timezone(_))
        ));
        assert!(matches!(
            parse("prompt = \"  \""),
            Err(ConfigError::BriefPrompt)
        ));
        assert!(matches!(
            parse("to = \"mark\""),
            Err(ConfigError::DiscordUser(_))
        ));
        assert!(matches!(parse("other = 1"), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn the_documented_example_parses() {
        let source = include_str!("config.rs");
        let example: String = source
            .lines()
            .skip_while(|line| *line != "//! ```toml")
            .skip(1)
            .take_while(|line| *line != "//! ```")
            .fold(String::new(), |mut text, line| {
                text.push_str(line.trim_start_matches("//!").trim_start());
                text.push('\n');
                text
            });
        let config = Config::parse(&example).unwrap();
        assert_eq!(config.mcp.len(), 2);
        assert!(config.approvals.approver.is_some());
        assert!(config.sandbox.is_some());
    }

    #[test]
    fn each_readme_config_block_parses() {
        let readme = include_str!("../../../README.md");
        let mut blocks = 0;
        for block in readme.split("```toml\n").skip(1) {
            let body = block.split("```").next().unwrap_or_default();
            let text = if body.contains("owner =") {
                body.to_owned()
            } else {
                format!("owner = \"m\"\n{body}")
            };
            if let Err(error) = Config::parse(&text) {
                panic!("a README block does not parse: {error}\n{body}");
            }
            blocks += 1;
        }
        assert!(blocks >= 5, "the README lost its config blocks");
    }

    #[test]
    fn the_google_section_needs_a_google_integration() {
        let san = "[san_mateo]\n[[san_mateo.integration]]\nkey = \"g\"\nprovider = \"google\"\n\
                   [[san_mateo.integration]]\nkey = \"gh\"\nprovider = \"github\"\n";
        let parse =
            |google: &str| Config::parse(&format!("owner = \"m\"\n{san}[google]\n{google}"));
        assert_eq!(
            parse("integration = \"g\"").unwrap().google,
            Some(GoogleConfig {
                integration: String::from("g"),
                mail: true,
                calendar: true,
            })
        );
        let mail_only = parse("integration = \"g\"\ncalendar = false").unwrap();
        assert!(mail_only.google.is_some_and(|g| g.mail && !g.calendar));
        let bad = |google: &str| match parse(google) {
            Err(ConfigError::Google(why)) => why,
            other => panic!("{google}: {other:?}"),
        };
        assert!(bad("integration = \"x\"").contains("not a [[san_mateo.integration]]"));
        assert!(bad("integration = \"gh\"").contains("provider `github`"));
        assert!(bad("integration = \"g\"\nmail = false\ncalendar = false").contains("both off"));
        let alone = Config::parse("owner = \"m\"\n[google]\nintegration = \"g\"");
        assert!(matches!(alone, Err(ConfigError::Google(_))));
        assert_eq!(Config::parse("owner = \"m\"").unwrap().google, None);
    }

    #[test]
    fn every_key_is_read() {
        let config = Config::parse("owner = \"mark\"\nsystem = \"Be brief.\"\nwindow = 8").unwrap();
        assert_eq!(config.system, "Be brief.");
        assert_eq!(config.window, 8);
    }

    #[test]
    fn a_bad_config_is_refused() {
        assert!(matches!(
            Config::parse("owner = \"a b\""),
            Err(ConfigError::Id(_))
        ));
        assert!(matches!(
            Config::parse("owner = \"m\"\nwindow = 0"),
            Err(ConfigError::Window)
        ));
        assert!(matches!(
            Config::parse("owner = \"m\"\nchannels = 1"),
            Err(ConfigError::Parse(_))
        ));
        assert!(matches!(Config::parse(""), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn the_discord_section_is_read() {
        let config = Config::parse(
            "owner = \"m\"\n[discord]\npairing = false\nallowed = [\"123\", \"456\"]",
        )
        .unwrap();
        assert!(!config.discord.pairing);
        let allowed: Vec<&str> = config
            .discord
            .allowed
            .iter()
            .map(SenderId::as_str)
            .collect();
        assert_eq!(allowed, ["discord:123", "discord:456"]);
        let plain = Config::parse("owner = \"m\"").unwrap();
        assert_eq!(plain.discord, DiscordConfig::default());
        assert!(plain.discord.pairing);
    }

    #[test]
    fn the_slack_section_and_slack_users_are_read() {
        let config = Config::parse(
            "owner = \"m\"\n[slack]\npairing = false\nallowed = [\"U012AB3CD\"]\n\
             [approvals]\napprover = \"slack:U012AB3CD\"\n",
        )
        .unwrap();
        assert!(!config.slack.pairing);
        assert_eq!(config.slack.allowed[0].as_str(), "slack:U012AB3CD");
        assert_eq!(
            config.approvals.approver.unwrap().as_str(),
            "slack:U012AB3CD"
        );
        assert_eq!(
            Config::parse("owner = \"m\"").unwrap().slack,
            SlackConfig::default()
        );
        for bad in [
            "u012",
            "U",
            "X012",
            "U01-2",
            &format!("U{}", "1".repeat(32)),
        ] {
            assert!(
                matches!(slack_sender(bad), Err(ConfigError::SlackUser(_))),
                "{bad}"
            );
        }
        assert!(matches!(
            Config::parse("owner = \"m\"\n[slack]\nallowed = [\"123\"]"),
            Err(ConfigError::SlackUser(_))
        ));
        assert_eq!(chat_user("discord:42").unwrap().as_str(), "discord:42");
        assert_eq!(chat_user("42").unwrap().as_str(), "discord:42");
        assert!(chat_user("slack:42").is_err());
    }

    #[test]
    fn a_bad_discord_section_is_refused() {
        assert!(matches!(
            Config::parse("owner = \"m\"\n[discord]\nallowed = [\"me\"]"),
            Err(ConfigError::DiscordUser(_))
        ));
        assert!(matches!(
            Config::parse("owner = \"m\"\n[discord]\ntoken = \"x\""),
            Err(ConfigError::Parse(_))
        ));
        assert!(discord_sender("").is_err());
        assert!(discord_sender(&"1".repeat(21)).is_err());
        assert_eq!(discord_sender("42").unwrap().as_str(), "discord:42");
    }

    #[test]
    fn the_camel_routes_are_read() {
        let config = Config::parse(
            "owner = \"m\"\n\
             [[camel.route]]\nid = \"inbox\"\nfrom = \"file:/in?delete=true\"\nprompt = \"Read it.\"\n\
             [[camel.route]]\nid = \"tick-1\"\nfrom = \"timer:t?period=60000\"\nprompt = \" \"\n",
        )
        .unwrap();
        assert_eq!(
            config.camel,
            vec![
                CamelRoute {
                    id: String::from("inbox"),
                    from: String::from("file:/in?delete=true"),
                    prompt: Some(String::from("Read it.")),
                },
                CamelRoute {
                    id: String::from("tick-1"),
                    from: String::from("timer:t?period=60000"),
                    prompt: None,
                },
            ]
        );
        assert_eq!(config.camel[0].sender().unwrap().as_str(), "camel:inbox");
    }

    #[test]
    fn a_bad_camel_route_is_refused() {
        let route = |id: &str, from: &str| {
            Config::parse(&format!(
                "owner = \"m\"\n[[camel.route]]\nid = \"{id}\"\nfrom = \"{from}\"\n"
            ))
        };
        for id in ["", "Inbox", "a b", &"x".repeat(33)] {
            assert!(
                matches!(route(id, "file:/in"), Err(ConfigError::CamelRouteId(_))),
                "{id}"
            );
        }
        for from in ["kafka:topic", "file", "/in"] {
            let error = route("a", from).unwrap_err();
            assert!(matches!(error, ConfigError::CamelScheme { .. }), "{from}");
            assert!(error.to_string().contains("file, timer"));
        }
        let twice = Config::parse(
            "owner = \"m\"\n[[camel.route]]\nid = \"a\"\nfrom = \"file:/x\"\n\
             [[camel.route]]\nid = \"a\"\nfrom = \"file:/y\"\n",
        );
        assert!(matches!(twice, Err(ConfigError::CamelRouteTwice(_))));
        assert!(Config::parse("owner = \"m\"\n[camel]\nother = 1\n").is_err());
    }

    #[test]
    fn the_san_mateo_section_is_read() {
        assert_eq!(Config::parse("owner = \"m\"").unwrap().san_mateo, None);
        let config = Config::parse("owner = \"m\"\n[san_mateo]\n").unwrap();
        let section = config.san_mateo.unwrap();
        assert_eq!(section.bind.to_string(), DEFAULT_SAN_MATEO_BIND);
        assert_eq!(section.integrations, Vec::new());
        let config = Config::parse(
            "owner = \"m\"\n[san_mateo]\nbind = \"[::1]:4000\"\n\
             [[san_mateo.integration]]\nkey = \"google\"\nprovider = \"google\"\n\
             client_id = \"c\"\nclient_secret_env = \"LG_G_SECRET\"\nscopes = [\"s1\"]\n\
             [[san_mateo.integration]]\nkey = \"gh.work\"\nprovider = \"github\"\nclient_id = \"\"\n",
        )
        .unwrap();
        let section = config.san_mateo.unwrap();
        assert_eq!(section.bind.port(), 4000);
        assert_eq!(
            section.integrations,
            vec![
                Integration {
                    key: String::from("google"),
                    provider: String::from("google"),
                    client_id: Some(String::from("c")),
                    client_secret_env: Some(String::from("LG_G_SECRET")),
                    scopes: vec![String::from("s1")],
                },
                Integration {
                    key: String::from("gh.work"),
                    provider: String::from("github"),
                    client_id: None,
                    client_secret_env: None,
                    scopes: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn a_bad_san_mateo_section_is_refused() {
        let error = |section: &str| match Config::parse(&format!(
            "owner = \"m\"\n[san_mateo]\n{section}"
        )) {
            Err(ConfigError::SanMateo(error)) => error,
            other => panic!("{other:?}"),
        };
        assert!(matches!(error("bind = \"nope\"\n"), SanMateoError::Bind(_)));
        assert!(matches!(
            error("bind = \"0.0.0.0:3003\"\n"),
            SanMateoError::NotLoopback(_)
        ));
        let one = |key: &str, extra: &str| {
            format!("[[san_mateo.integration]]\nkey = \"{key}\"\nprovider = \"p\"\n{extra}")
        };
        assert!(matches!(error(&one("a b", "")), SanMateoError::Key(_)));
        assert!(matches!(error(&one("", "")), SanMateoError::Key(_)));
        let twice = format!("{}{}", one("a", ""), one("a", ""));
        assert!(matches!(error(&twice), SanMateoError::KeyTwice(_)));
        for var in ["lower", "1ABC", "A-B", ""] {
            let extra = format!("client_secret_env = \"{var}\"\n");
            assert!(
                matches!(error(&one("a", &extra)), SanMateoError::Variable(_)),
                "{var}"
            );
        }
    }

    #[test]
    fn a_missing_file_names_its_path() {
        let error = Config::load(Path::new("/nonexistent/lg.toml")).unwrap_err();
        assert!(error.to_string().contains("/nonexistent/lg.toml"));
    }

    #[test]
    fn the_cortex_section_is_read_and_checked() {
        let plain = Config::parse("owner = \"mark\"").unwrap();
        assert_eq!(plain.cortex, CortexConfig::default());

        let herdr = Config::parse("owner = \"mark\"\n[cortex]\nwake = true").unwrap();
        assert_eq!(
            herdr.cortex,
            CortexConfig {
                wake: true,
                wake_socket: None,
            }
        );

        let socket_implies_wake =
            Config::parse("owner = \"mark\"\n[cortex]\nwake_socket = \"/tmp/lg-wake.sock\"")
                .unwrap();
        assert_eq!(
            socket_implies_wake.cortex,
            CortexConfig {
                wake: true,
                wake_socket: Some(PathBuf::from("/tmp/lg-wake.sock")),
            }
        );

        let disabled = Config::parse(
            "owner = \"mark\"\n[cortex]\nwake = false\nwake_socket = \"/tmp/lg-wake.sock\"",
        )
        .unwrap();
        assert!(!disabled.cortex.wake);

        let empty_sock = Config::parse("owner = \"mark\"\n[cortex]\nwake_socket = \"   \"");
        assert!(matches!(empty_sock, Err(ConfigError::Cortex(_))));

        let unknown = Config::parse("owner = \"mark\"\n[cortex]\nunknown_key = true");
        assert!(unknown.is_err());
    }
}
