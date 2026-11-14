//! `lg serve`: the daemon that owns the database and the agent.
//!
//! The daemon is the single writer of one SQLite file. The runtime takes a
//! lock file next to the database, so a second daemon fails to open it. The
//! kernel drops the lock when the holder dies, so a crash strands nothing.
//!
//! One task owns the runtime. The socket and the chat channels, Discord and
//! Slack, hand each message to it over a channel, so ops run one at a time,
//! in arrival order.
//! A tick drives the assistant once a second, so a durable timer fires
//! without a message.
//!
//! A chat reply goes out once. The message id is the idempotency key of
//! its op, so a message that arrives twice runs once. The pairing store
//! keeps the last message whose reply went out. After a reconnect, the
//! daemon reads the history after that cursor, so a message sent while it
//! was down still gets its answer.
//!
//! A `run_code` call waits for a person ([`crate::approvals`]). While it
//! waits, the daemon takes no new op. It holds each new chat message and
//! runs it after the decision. `lg chat` gets a busy answer instead.

use std::collections::{BTreeSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use autumn_harvest_agent::{AgentHarness, AgentId, AgentModel, AgentOp, AgentProfile, sqlite};
use autumn_harvest_sqlite::SqliteRuntime;
use chrono::{DateTime, Utc};
use littleguy_channels::discord::{ButtonPress, DiscordRest, GatewayEvent, run_gateway};
use littleguy_channels::slack::{SlackEvent, SlackPress, run_socket};
use littleguy_channels::{ChannelError, DmMessage, Service, SlackApi};
use littleguy_core::{
    ASSISTANT_KIND, ChannelId, ChannelRule, Config, Inbound, MessageId, PairingCode, Pending,
    PendingPairings, Route, Routing, SenderId, WakeMessageId, discord_sender,
    pairing::PAIRING_TTL_SECS, route, slack_sender,
};
use littleguy_stores::SqliteStores;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::approvals::Requester;
use crate::brief::{BriefDelivery, Calendar};
use crate::camel::Camel;
use crate::chats::Chats;
use crate::oauth::Connections;
use crate::pairs::{Entry, PairStore};
use crate::protocol::{MAX_REQUEST_BYTES, PairEntry, Request, Response};
use littleguy::reflex_gate::ReflexClassifyClient;
use littleguy::wake::{WakeListener, cortex_wake_notice};

/// The channel id of the local socket.
pub const CLI_CHANNEL: &str = "cli";

/// The channel id of the Cortex mesh wake socket.
pub const CORTEX_CHANNEL: &str = "cortex";

/// The sender id of Cortex mesh wake notices.
pub const CORTEX_SENDER: &str = "cortex:mesh";

/// The sender id of `user` on `service`.
///
/// # Errors
///
/// Returns an error when `user` is not a user id of the service.
pub fn chat_sender(service: Service, user: &str) -> Result<SenderId> {
    Ok(match service {
        Service::Discord => discord_sender(user)?,
        Service::Slack => slack_sender(user)?,
    })
}

/// The message id of one chat message, the idempotency key of its op. A
/// Discord snowflake is unique on its own. A Slack `ts` is unique in its
/// channel only, so the channel goes in the key.
///
/// # Errors
///
/// Returns an error when the key is not a valid message id.
pub fn chat_message_id(service: Service, message: &DmMessage) -> Result<MessageId> {
    let key = match service {
        Service::Discord => format!("{service}:{}", message.id),
        Service::Slack => format!("{service}:{}:{}", message.channel_id, message.id),
    };
    Ok(MessageId::new(key)?)
}

/// The delivery cursor in a chat message id: the part after its last `:`.
fn cursor_of(message_id: &MessageId) -> String {
    message_id
        .as_str()
        .rsplit_once(':')
        .map(|(_, cursor)| cursor.to_owned())
        .unwrap_or_default()
}

/// How often the daemon drives the assistant without a message.
pub const TICK: Duration = Duration::from_secs(1);

/// The sender id of a local user.
///
/// # Errors
///
/// Never in practice: the text is printable ASCII and short.
pub fn cli_sender(uid: u32) -> Result<SenderId> {
    Ok(SenderId::new(format!("uid:{uid}"))?)
}

/// The routing table of a daemon run by `owner_uid`.
///
/// The user who runs the daemon is the one allowed sender on the local
/// socket, which offers no pairing. Each chat service in `chats` allows the
/// senders of the config file and of the pairing store. Each camel route of
/// the config is an allowed sender of the camel channel.
///
/// # Errors
///
/// Returns an error when the owner name is not a valid agent name.
pub fn routing(
    config: &Config,
    owner_uid: u32,
    store: &PairStore,
    chats: &[Service],
) -> Result<Routing> {
    let cli = ChannelRule {
        enabled: true,
        pairing: false,
        allowed: std::iter::once(cli_sender(owner_uid)?).collect(),
    };
    let cortex = ChannelRule {
        enabled: true,
        pairing: false,
        allowed: std::iter::once(SenderId::new(CORTEX_SENDER)?).collect(),
    };
    let mut table = Routing::new(&config.owner)
        .with_context(|| format!("`{}` is not a valid agent name", config.owner))?
        .channel(ChannelId::new(CLI_CHANNEL)?, cli)
        .channel(ChannelId::new(CORTEX_CHANNEL)?, cortex);
    for service in chats {
        let channel = ChannelId::new(service.name())?;
        let settings = chat_config(config, *service);
        let paired = store
            .entries()
            .filter(|e| e.paired && e.channel == channel)
            .map(|e| e.sender.clone());
        let rule = ChannelRule {
            enabled: true,
            pairing: settings.pairing,
            allowed: settings.allowed.iter().cloned().chain(paired).collect(),
        };
        table = table.channel(channel, rule);
    }
    if !config.camel.is_empty() {
        let rule = ChannelRule {
            enabled: true,
            pairing: false,
            allowed: crate::camel::senders(&config.camel)?.into_iter().collect(),
        };
        table = table.channel(ChannelId::new(crate::camel::CAMEL_CHANNEL)?, rule);
    }
    Ok(table)
}

/// Warns when `approvals.approver` names a chat service that is currently off.
fn warn_if_approver_offline(approver: Option<&SenderId>, services: &[Service]) {
    let approver_on = approver
        .and_then(|sender| Service::split_sender(sender.as_str()))
        .is_some_and(|(service, _)| services.contains(&service));
    if approver.is_some() && !approver_on {
        tracing::warn!("approvals.approver is set, but its chat is off: decide with `lg approve`");
    }
}

/// The config section of `service`.
const fn chat_config(config: &Config, service: Service) -> &littleguy_core::ChatConfig {
    match service {
        Service::Discord => &config.discord,
        Service::Slack => &config.slack,
    }
}

/// The harness of the assistant. With `memory`, the assistant keeps notes
/// and an episode log in that store. `db` is the database that the
/// consolidation dispatcher reads. With `brief`, the delivery sends the
/// answer of each heartbeat tick to the owner. The daemon arms each tick.
/// `tools` gives the assistant its tools, and a workspace when the file
/// tools are on. `guard` adds the `run_code` tool, the flow rules and the
/// approval gate.
#[must_use]
pub fn harness(
    config: &Config,
    model: Arc<dyn AgentModel>,
    memory: Option<(Arc<SqliteStores>, &Path)>,
    brief: Option<Arc<BriefDelivery>>,
    tools: crate::tools::Tools,
    guard: Option<crate::guard::Guard>,
) -> AgentHarness {
    let profile = AgentProfile::new(ASSISTANT_KIND)
        .system(config.system.as_str())
        .window(config.window);
    let (mut harness, mut profile) = tools.apply(AgentHarness::new(model), profile);
    if let Some(guard) = guard {
        (harness, profile) = guard.apply(harness, profile);
    }
    if let Some((stores, db)) = memory {
        harness = crate::memory::harness(harness, stores, db);
        profile = crate::memory::profile(profile);
    }
    if let Some(delivery) = brief {
        harness = harness.delivery(delivery);
        profile = profile.deliver();
    }
    harness.agent(profile)
}

/// The seconds since the Unix epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A fresh pairing code from the operating system's randomness.
fn fresh_code() -> PairingCode {
    let mut bytes = [0_u8; 5];
    if getrandom::fill(&mut bytes).is_err() {
        // No randomness: mix the clock in, so codes still differ. The owner
        // still approves each one, so a weak code admits no one alone.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        bytes.copy_from_slice(&nanos.to_le_bytes()[..5]);
    }
    PairingCode::from_random(bytes)
}

/// The text that a stranger gets for `code`.
#[must_use]
pub fn pairing_text(code: Option<&str>) -> String {
    let minutes = PAIRING_TTL_SECS / 60;
    code.map_or_else(
        || String::from("littleguy cannot pair anyone right now. Try again in a few minutes."),
        |code| {
            format!(
                "Hi! littleguy does not know you yet. Ask its owner to run `lg pair {code}` \
                 in the next {minutes} minutes."
            )
        },
    )
}

/// One reply for a chat channel to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    /// The DM channel.
    pub channel_id: String,
    /// The text.
    pub text: String,
    /// The sender, when the reply moves its delivery cursor.
    pub track: Option<SenderId>,
    /// The message that this reply answers.
    pub message_id: String,
}

/// One cursor to read the missed history from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// The DM channel.
    pub channel_id: String,
    /// The last message whose reply went out.
    pub after: String,
}

/// What [`Daemon::open_with`] adds to a config: the parts that need async
/// work or a test seam.
#[derive(Debug, Default)]
pub struct Extras {
    /// The runtime of the `run_code` tool.
    pub sandbox: Option<Arc<autumn_harvest::sandbox::SandboxRuntime>>,
    /// The tools of the MCP servers.
    pub mcp: Vec<Arc<dyn autumn_harvest_agent::Tool>>,
    /// The Google API of the owner, for the `[google]` tools.
    pub google: Option<Arc<dyn littleguy_tools::google::GoogleApi>>,
    /// The Slack channel takes messages.
    pub slack: bool,
    /// Optional `reflexd` `/v1/classify` client (`specs/0008`).
    pub reflex: Option<ReflexClassifyClient>,
}

/// The daemon state: the runtime, the routing table and the pairings.
pub struct Daemon {
    rt: SqliteRuntime,
    routing: Routing,
    owner_uid: u32,
    config_allowed: BTreeSet<SenderId>,
    pending: PendingPairings,
    store: PairStore,
    connections: Option<Connections>,
    brief: Option<Calendar>,
    chats: Arc<Chats>,
    clock: Clock,
    /// The store of the calls that wait, when a call can wait.
    approvals: Option<Arc<SqliteStores>>,
    /// The chat user who decides, such as `slack:U1`.
    approver: Option<SenderId>,
    /// The request that the assistant waits on.
    waiting: Option<i64>,
    /// The chat messages that arrived while a request waited.
    held: VecDeque<(Service, DmMessage)>,
    /// The Cortex mesh wakes that arrived while a request waited.
    held_wakes: VecDeque<WakeMessageId>,
}

/// The wall clock of the daemon. Tests set one clock for the daemon and its
/// runtime.
type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("assistant", self.routing.assistant())
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// Opens the database at `db` and registers the assistant. With
    /// `discord`, the Discord channel takes messages. Slack stays off. It starts no MCP
    /// server. A test seam: `lg serve` calls [`Self::open_with`].
    ///
    /// # Errors
    ///
    /// Returns an error when another daemon holds the database, the database
    /// or the pairing store does not open, or the owner name is not a valid
    /// agent name.
    #[cfg(test)]
    pub fn open(
        db: &Path,
        config: &Config,
        model: Arc<dyn AgentModel>,
        owner_uid: u32,
        discord: bool,
    ) -> Result<Self> {
        let sandbox = config
            .sandbox
            .as_ref()
            .map(crate::sandbox::runtime)
            .transpose()?;
        let extras = Extras {
            sandbox,
            ..Extras::default()
        };
        Self::open_with(db, config, model, owner_uid, discord, extras)
    }

    /// [`Self::open`] with `extras`: the sandbox runtime, the MCP tools, the
    /// Google API and Slack. The sandbox runtime counts only with a
    /// `[sandbox]` section.
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    pub fn open_with(
        db: &Path,
        config: &Config,
        model: Arc<dyn AgentModel>,
        owner_uid: u32,
        discord: bool,
        extras: Extras,
    ) -> Result<Self> {
        if let Some(parent) = db.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut rt =
            SqliteRuntime::open(db).with_context(|| format!("opening {}", db.display()))?;
        let brief = config
            .brief
            .as_ref()
            .map(|settings| Calendar::new(settings, Utc::now()))
            .transpose()?;
        // The brief keeps its sent reports in the memory store, and the
        // guard keeps its calls that wait. So each opens the store even
        // when memory is off.
        let runtime = extras.sandbox.filter(|_| config.sandbox.is_some());
        let google = crate::tools::google_tools(config.google.as_ref(), extras.google);
        let google_on = !google.is_empty();
        let tools =
            crate::tools::Tools::new(&config.tools, db, crate::tools::search_key_from_env())
                .with(extras.mcp)
                .with(google);
        tracing::info!(tools = ?tools.names(), "the assistant tools");
        let flow = config.flow.rules();
        let asks = crate::guard::Guard::asks(
            &flow,
            tools.all(),
            runtime.is_some() || google_on || extras.reflex.is_some(),
        );
        let stores = if config.memory.enabled || config.brief.is_some() || asks {
            Some(crate::memory::open(db)?)
        } else {
            None
        };
        let chats = Arc::new(Chats::default());
        let services: Vec<Service> = [(Service::Discord, discord), (Service::Slack, extras.slack)]
            .into_iter()
            .filter_map(|(service, on)| on.then_some(service))
            .collect();
        let delivery = config
            .brief
            .as_ref()
            .zip(stores.as_ref())
            .map(|(settings, stores)| {
                Arc::new(BriefDelivery::new(
                    settings,
                    Arc::clone(stores),
                    Arc::clone(&chats),
                ))
            });
        let approvals = stores.clone().filter(|_| asks);
        let guard = approvals.clone().map(|stores| crate::guard::Guard {
            stores,
            timeout: Duration::from_secs(config.approvals.timeout_secs),
            flow,
            sandbox: runtime,
            google: google_on,
            reflex: extras.reflex,
            agent_name: format!("{ASSISTANT_KIND}/{}", config.owner),
        });
        let approver = config.approvals.approver.clone();
        warn_if_approver_offline(approver.as_ref(), &services);
        let memory = stores
            .filter(|_| config.memory.enabled)
            .map(|stores| (stores, db));
        sqlite::register(
            &mut rt,
            Arc::new(harness(config, model, memory, delivery, tools, guard)),
        );
        crate::memory::schedule(&mut rt, &config.memory)?;
        // The runtime lock is held now, so this daemon is the store's one writer.
        let store = PairStore::open(crate::pairs::path_for(db))?;
        // A request that waited before a restart still waits.
        let waiting = match &approvals {
            Some(stores) => stores.open_approvals()?.first().map(|record| record.id),
            None => None,
        };
        Ok(Self {
            routing: routing(config, owner_uid, &store, &services)?,
            rt,
            owner_uid,
            config_allowed: config
                .discord
                .allowed
                .iter()
                .chain(&config.slack.allowed)
                .cloned()
                .collect(),
            pending: PendingPairings::default(),
            store,
            connections: None,
            brief,
            chats,
            clock: Arc::new(Utc::now),
            approvals,
            approver,
            waiting,
            held: VecDeque::new(),
            held_wakes: VecDeque::new(),
        })
    }

    /// Sets the clock of the daemon and of its runtime. A test seam.
    #[cfg(test)]
    fn with_clock(mut self, clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        let clock: Clock = Arc::new(clock);
        let runtime = Arc::clone(&clock);
        self.rt.set_clock(move || runtime());
        self.clock = clock;
        self
    }

    /// Gives the chats their Discord client. The first client stays.
    pub fn discord_rest(&self, rest: DiscordRest) {
        self.chats.set_discord(rest);
    }

    /// Gives the chats their Slack client. The first client stays.
    pub fn slack_api(&self, api: SlackApi) {
        self.chats.set_slack(api);
    }

    /// Keeps OAuth connections with `connections`.
    #[must_use]
    pub fn with_connections(mut self, connections: Connections) -> Self {
        self.connections = Some(connections);
        self
    }

    /// The agent that the owner talks to.
    #[must_use]
    pub const fn assistant(&self) -> &AgentId {
        self.routing.assistant()
    }

    /// The mapper of the camel routes of `config`. It checks each message
    /// against the routing table of this daemon.
    #[must_use]
    pub fn camel_mapper(&self, config: &Config) -> crate::camel::Mapper {
        crate::camel::Mapper {
            assistant: self.routing.assistant().clone(),
            routing: self.routing.clone(),
            routes: config.camel.clone(),
        }
    }

    /// Writes one camel batch into the runtime, then drives the assistant,
    /// so it answers at once.
    async fn camel(&mut self, apply: crate::camel::Apply) {
        let applied = apply.batch.apply(&mut self.rt);
        // The writes are durable now, so the camel task may ack them.
        let _ = apply.reply.send(applied);
        if let Err(error) = self.tick().await {
            tracing::warn!(error = %format!("{error:#}"), "driving after a camel batch failed");
            return;
        }
        match sqlite::state(&self.rt, self.routing.assistant()).await {
            Ok(Some(state)) => tracing::debug!(
                answer = state.last_answer.as_deref().unwrap_or_default(),
                "the assistant answered a camel message"
            ),
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "reading the assistant failed"),
        }
    }

    /// The committed state of the assistant, if it runs.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    #[cfg(test)]
    pub fn state(
        &self,
    ) -> impl Future<Output = Result<Option<autumn_harvest_agent::AgentState>>> + Send + 'static
    {
        // The query starts now, so the future holds no borrow of the runtime.
        let query = sqlite::state(&self.rt, self.routing.assistant());
        async move { Ok(query.await?) }
    }

    /// Handles one request from the local socket, sent by `uid`.
    pub async fn handle_request(&mut self, uid: u32, request: Request) -> Response {
        let result = match request {
            Request::Chat { message_id, text } => match cli_inbound(uid, message_id, text) {
                Ok(inbound) => self.handle(&inbound, "").await,
                Err(error) => Err(error),
            },
            owner_only if uid != self.owner_uid => {
                tracing::warn!(uid, request = ?owner_only, "refused an owner request");
                Ok(Response::Error {
                    message: String::from("only the owner of the daemon may do that"),
                })
            }
            Request::Approvals => self.approval_list(),
            Request::Approve { id } => {
                self.decide_and_resume(id, crate::approvals::Choice::Approve)
                    .await
            }
            Request::Deny { id, reason } => {
                let reason =
                    reason.unwrap_or_else(|| String::from(crate::approvals::DEFAULT_DENY_REASON));
                self.decide_and_resume(id, crate::approvals::Choice::Deny(reason))
                    .await
            }
            Request::Pair { code } => self.pair(&code),
            Request::Pairs => Ok(self.pairs()),
            Request::Unpair { sender } => self.unpair(&sender),
            request @ (Request::Connect { .. }
            | Request::Connections
            | Request::Disconnect { .. }) => {
                oauth_request(self.connections.as_ref(), request).await
            }
        };
        result.unwrap_or_else(|error| {
            tracing::error!(error = %format!("{error:#}"), "a request failed");
            Response::Error {
                message: format!("{error:#}"),
            }
        })
    }

    /// Routes one message, runs it, and returns the answer. `reply_to` is
    /// where a pairing prompt would go.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime cannot record, run or read the op.
    pub async fn handle(&mut self, inbound: &Inbound, reply_to: &str) -> Result<Response> {
        let id = match route(&self.routing, inbound) {
            Route::Agent(id) => id,
            Route::Pairing => {
                let pending = Pending {
                    channel: inbound.channel.clone(),
                    sender: inbound.sender.clone(),
                    reply_to: reply_to.to_owned(),
                    message_id: inbound.message_id.clone(),
                    issued_at: now_secs(),
                };
                let code = self.pending.issue(pending, fresh_code);
                tracing::info!(sender = %inbound.sender, issued = code.is_some(), "a stranger asked to pair");
                return Ok(Response::Pairing {
                    code: code.map(|code| code.to_string()),
                });
            }
            Route::Drop => {
                tracing::info!(channel = %inbound.channel, sender = %inbound.sender, "dropped a message");
                return Ok(Response::Dropped);
            }
        };
        if let Some(approval) = self.waiting {
            return Ok(Response::Busy { approval });
        }
        let before = sqlite::state(&self.rt, &id).await?;
        let failures_before = before.as_ref().map_or(0, |state| state.failures);
        let key = inbound.message_id.as_str();
        let op = AgentOp::message_with_id(inbound.text.as_str(), key);
        let outcome = sqlite::send(&mut self.rt, &id, &op, Some(key))?;
        if !outcome.signal_delivered {
            return Ok(Response::Duplicate {
                last_answer: before.and_then(|state| state.last_answer),
            });
        }
        let run = sqlite::drive(&mut self.rt, &id).await?;
        let requester = Service::from_name(inbound.channel.as_str())
            .map_or(Requester::Cli, |service| {
                Requester::Chat(service, reply_to.to_owned())
            });
        if let Some(approval) = self.settle(run.as_ref(), Some(&requester)).await? {
            return Ok(Response::Waiting { approval });
        }
        let Some(state) = sqlite::state(&self.rt, &id).await? else {
            return Ok(Response::NoAnswer {
                reason: String::from("the assistant has no running run"),
            });
        };
        if state.failures > failures_before {
            return Ok(Response::NoAnswer {
                reason: state
                    .last_error
                    .unwrap_or_else(|| String::from("the op failed")),
            });
        }
        Ok(state.last_answer.map_or_else(
            || Response::NoAnswer {
                reason: String::from("the op ended without an answer"),
            },
            |text| Response::Reply { text },
        ))
    }

    /// Handles one Cortex mesh wake notice. While an approval waits, the wake
    /// sits in `held_wakes` and runs after the decision settles.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime fails to record or drive the op.
    pub async fn wake(&mut self, message_id: &WakeMessageId) -> Result<Response> {
        if self.waiting.is_some() {
            if !self.held_wakes.contains(message_id) {
                self.held_wakes.push_back(message_id.clone());
            }
            return Ok(Response::Busy {
                approval: self.waiting.unwrap_or_default(),
            });
        }
        let inbound = Inbound {
            channel: ChannelId::new(CORTEX_CHANNEL)?,
            sender: SenderId::new(CORTEX_SENDER)?,
            message_id: MessageId::new(format!("{CORTEX_CHANNEL}:{message_id}"))?,
            text: cortex_wake_notice(message_id),
        };
        self.handle(&inbound, "").await
    }

    /// Handles one Discord direct message, as [`Self::chat`] does.
    ///
    /// # Errors
    ///
    /// As [`Self::chat`].
    #[cfg(test)]
    pub async fn discord(&mut self, message: &DmMessage) -> Result<Option<Outgoing>> {
        self.chat(Service::Discord, message).await
    }

    /// Handles one direct message on `service`. Returns the reply to send,
    /// if any. A message whose reply went out already gets none.
    ///
    /// # Errors
    ///
    /// Returns an error when the author id is not a user id of the service,
    /// or the runtime fails.
    pub async fn chat(
        &mut self,
        service: Service,
        message: &DmMessage,
    ) -> Result<Option<Outgoing>> {
        let channel = ChannelId::new(service.name())?;
        let sender = chat_sender(service, &message.author_id)?;
        if self.store.is_delivered(&channel, &sender, &message.id) {
            return Ok(None);
        }
        // A request waits, so the message runs after the decision.
        if self.waiting.is_some() {
            let known = self.held.iter().any(|(held, m)| {
                *held == service && m.id == message.id && m.channel_id == message.channel_id
            });
            if !known {
                self.held.push_back((service, message.clone()));
            }
            return Ok(None);
        }
        let inbound = Inbound {
            channel,
            sender: sender.clone(),
            message_id: chat_message_id(service, message)?,
            text: message.content.clone(),
        };
        let response = self.handle(&inbound, &message.channel_id).await?;
        let (text, track) = match response {
            Response::Reply { text } => (text, true),
            // The op ran before, but its reply did not go out.
            Response::Duplicate { last_answer } => (
                last_answer.unwrap_or_else(|| String::from("(no answer yet)")),
                true,
            ),
            Response::NoAnswer { reason } => (format!("Sorry, I could not answer: {reason}"), true),
            Response::Pairing { code } => (pairing_text(code.as_deref()), false),
            Response::Waiting { approval } => (crate::approvals::waiting_text(approval), true),
            _ => return Ok(None),
        };
        Ok(Some(Outgoing {
            channel_id: message.channel_id.clone(),
            text,
            track: track.then_some(sender),
            message_id: message.id.clone(),
        }))
    }

    /// Records that the reply to `message_id` went out. The service is the
    /// one that `sender` names.
    ///
    /// # Errors
    ///
    /// Returns an error when `sender` names no chat service, or the pairing
    /// store does not save.
    pub fn delivered(&mut self, sender: &SenderId, reply_to: &str, message_id: &str) -> Result<()> {
        let (service, _) = Service::split_sender(sender.as_str())
            .with_context(|| format!("`{sender}` is not a chat user"))?;
        let channel = ChannelId::new(service.name())?;
        self.store.delivered(&channel, sender, reply_to, message_id)
    }

    /// The cursor of each allowed sender of `service` with a known DM
    /// channel.
    #[must_use]
    pub fn cursors(&self, service: Service) -> Vec<Cursor> {
        let Ok(channel) = ChannelId::new(service.name()) else {
            return Vec::new();
        };
        let allowed: BTreeSet<&SenderId> = self.routing.allowed(&channel).collect();
        self.store
            .entries()
            .filter(|e| e.channel == channel && allowed.contains(&e.sender))
            .filter(|e| !e.reply_to.is_empty() && !e.delivered.is_empty())
            .map(|e| Cursor {
                channel_id: e.reply_to.clone(),
                after: e.delivered.clone(),
            })
            .collect()
    }

    /// Redeems a pairing code.
    fn pair(&mut self, code: &str) -> Result<Response> {
        let Some(code) = PairingCode::parse(code) else {
            return Ok(Response::Error {
                message: String::from("that is not a pairing code"),
            });
        };
        let Some(pending) = self.pending.redeem(&code, now_secs()) else {
            return Ok(Response::Error {
                message: String::from("no live pairing has that code. Codes last 10 minutes"),
            });
        };
        if !self.routing.allow(&pending.channel, pending.sender.clone()) {
            bail!("the channel {} takes no senders", pending.channel);
        }
        let delivered = cursor_of(&pending.message_id);
        self.store.put(Entry {
            channel: pending.channel,
            sender: pending.sender.clone(),
            reply_to: pending.reply_to,
            delivered,
            paired: true,
        })?;
        tracing::info!(sender = %pending.sender, "paired a sender");
        Ok(Response::Paired {
            sender: pending.sender.to_string(),
        })
    }

    /// The allowed senders of every channel.
    fn pairs(&self) -> Response {
        let mut entries = Vec::new();
        let channels = std::iter::once(CLI_CHANNEL).chain(Service::ALL.map(Service::name));
        for channel in channels {
            let Ok(id) = ChannelId::new(channel) else {
                continue;
            };
            for sender in self.routing.allowed(&id) {
                let source = if channel == CLI_CHANNEL {
                    "owner"
                } else if self.config_allowed.contains(sender) {
                    "config"
                } else {
                    "paired"
                };
                entries.push(PairEntry {
                    channel: channel.to_owned(),
                    sender: sender.to_string(),
                    source: source.to_owned(),
                });
            }
        }
        Response::Pairs { entries }
    }

    /// Takes a paired chat sender off its channel. A sender that the config
    /// file allows stays allowed.
    fn unpair(&mut self, sender: &str) -> Result<Response> {
        let Ok(sender) = SenderId::new(sender) else {
            return Ok(Response::Unpaired { removed: false });
        };
        let Some((service, _)) = Service::split_sender(sender.as_str()) else {
            return Ok(Response::Unpaired { removed: false });
        };
        let channel = ChannelId::new(service.name())?;
        let removed = self.store.unpair(&channel, &sender)?;
        if removed && !self.config_allowed.contains(&sender) {
            self.routing.revoke(&channel, &sender);
        }
        Ok(Response::Unpaired { removed })
    }

    /// Aims the engine timer at the next brief slot, when it needs an arm.
    /// It returns the op key of the arm that it sent. The key names the
    /// slot, so a slot is armed once.
    ///
    /// # Errors
    ///
    /// Returns an error when the state does not read or the runtime does
    /// not take the op.
    pub async fn arm_brief(&mut self) -> Result<Option<String>> {
        let now = (self.clock)();
        let id = self.routing.assistant().clone();
        let Some(calendar) = self.brief.as_mut() else {
            return Ok(None);
        };
        // The state query replays the run, so it runs only after a slot.
        let ticks = if calendar.waits_for_tick(now) {
            sqlite::state(&self.rt, &id)
                .await?
                .map_or(0, |state| state.ticks)
        } else {
            0
        };
        let Some(arm) = calendar.arm(now, ticks) else {
            return Ok(None);
        };
        tracing::info!(slot = %arm.key, "the next morning brief");
        sqlite::send(&mut self.rt, &id, &arm.op, Some(&arm.key))?;
        Ok(Some(arm.key))
    }

    /// Drives the assistant until it blocks, so a due timer fires. Then it
    /// arms the next brief slot.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime cannot drive the run.
    pub async fn tick(&mut self) -> Result<()> {
        // A poll fires each due schedule, such as the nightly consolidation,
        // and drives every run once.
        self.rt.poll_once().await?;
        let first = sqlite::drive(&mut self.rt, self.routing.assistant()).await?;
        // The arm comes after the drive, so a due brief tick runs first. A
        // second drive books the armed tick at once.
        let run = if self.arm_brief().await?.is_some() {
            sqlite::drive(&mut self.rt, self.routing.assistant()).await?
        } else {
            first
        };
        // A wait can start or end without a message: a brief tick asks, or
        // a deadline passes.
        self.settle(run.as_ref(), None).await?;
        self.release_held().await;
        Ok(())
    }
}

/// Handles one OAuth request of the owner.
async fn oauth_request(connections: Option<&Connections>, request: Request) -> Result<Response> {
    let Some(connections) = connections else {
        return Ok(Response::Error {
            message: String::from(
                "this daemon keeps no OAuth connections. Add a [san_mateo] section",
            ),
        });
    };
    Ok(match request {
        Request::Connect { integration } => Response::ConnectLink {
            link: connections.connect(&integration).await?,
        },
        Request::Disconnect {
            integration,
            connection,
        } => Response::Disconnected {
            removed: connections.disconnect(&integration, &connection).await?,
        },
        _ => Response::Connections {
            entries: connections.list().await?,
        },
    })
}

/// Builds the inbound message of one socket chat request.
fn cli_inbound(uid: u32, message_id: String, text: String) -> Result<Inbound> {
    Ok(Inbound {
        channel: ChannelId::new(CLI_CHANNEL)?,
        sender: cli_sender(uid)?,
        message_id: MessageId::new(message_id).context("bad message id")?,
        text,
    })
}

/// Binds the private socket at `path`.
///
/// A socket file that no daemon answers on is stale, and is removed. A live
/// one is an error. The socket gets mode 0600, so only its owner connects.
///
/// # Errors
///
/// Returns an error when another daemon answers on `path`, or the bind fails.
pub async fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            bail!("a daemon already answers on {}", path.display());
        }
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    set_private(path)?;
    Ok(listener)
}

fn set_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting the mode of {}", path.display()))
}

/// One piece of work for the runtime task.
enum Job {
    /// A request from the local socket.
    Socket {
        uid: u32,
        request: Request,
        reply: oneshot::Sender<Response>,
    },
    /// A direct message on a chat service.
    Chat {
        service: Service,
        message: DmMessage,
        reply: oneshot::Sender<Option<Outgoing>>,
    },
    /// A chat reply went out.
    Delivered {
        sender: SenderId,
        reply_to: String,
        message_id: String,
    },
    /// The cursors of `service` to read the missed history from.
    Cursors {
        service: Service,
        reply: oneshot::Sender<Vec<Cursor>>,
    },
    /// A press of an approval button.
    Press {
        press: Press,
        reply: oneshot::Sender<gate::PressAnswer>,
    },
    /// A validated Cortex mesh wake notice from `WakeListener`.
    Wake {
        message_id: WakeMessageId,
        reply: oneshot::Sender<bool>,
    },
}

/// One button press, as each chat service gives it.
#[derive(Debug, Clone)]
struct Press {
    service: Service,
    user_id: String,
    custom_id: String,
    channel_id: String,
}

/// What the daemon needs to run the Discord channel. Its `Debug` form
/// leaves the token out.
#[derive(Clone)]
pub struct DiscordLink {
    /// The REST client.
    pub rest: DiscordRest,
    /// The bot token, for the gateway.
    pub token: String,
    /// The gateway URL.
    pub gateway_url: String,
}

impl std::fmt::Debug for DiscordLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordLink")
            .field("rest", &self.rest)
            .field("gateway_url", &self.gateway_url)
            .finish_non_exhaustive()
    }
}

/// The chat channels that [`run`] starts.
#[derive(Debug, Clone, Default)]
pub struct Links {
    /// Discord, when it is on.
    pub discord: Option<DiscordLink>,
    /// Slack, when it is on. The client holds both tokens.
    pub slack: Option<SlackApi>,
}

/// Runs one job on the runtime.
async fn run_job(daemon: &mut Daemon, job: Job) {
    match job {
        Job::Socket {
            uid,
            request,
            reply,
        } => {
            let response = daemon.handle_request(uid, request).await;
            // A client that hung up gets no answer. The op still ran.
            let _ = reply.send(response);
            daemon.release_held().await;
        }
        Job::Chat {
            service,
            message,
            reply,
        } => {
            let outgoing = daemon.chat(service, &message).await.unwrap_or_else(|error| {
                tracing::error!(error = %format!("{error:#}"), %service, "a chat message failed");
                None
            });
            let _ = reply.send(outgoing);
        }
        Job::Delivered {
            sender,
            reply_to,
            message_id,
        } => {
            if let Err(error) = daemon.delivered(&sender, &reply_to, &message_id) {
                tracing::error!(error = %format!("{error:#}"), "the delivery cursor did not save");
            }
        }
        Job::Cursors { service, reply } => {
            let _ = reply.send(daemon.cursors(service));
        }
        Job::Press { press, reply } => {
            // The answer goes first: the chat waits three seconds for it.
            let (answer, decided) = match chat_sender(press.service, &press.user_id) {
                Ok(user) => daemon.press(&user, &press.custom_id),
                Err(_) => (
                    gate::PressAnswer::Private(String::from(
                        "Only the approver can decide this request.",
                    )),
                    None,
                ),
            };
            let _ = reply.send(answer);
            if let Some(record) = decided {
                daemon
                    .resume_after_press(&record, press.service, &press.channel_id)
                    .await;
            }
            daemon.release_held().await;
        }
        Job::Wake { message_id, reply } => {
            let ok = match daemon.wake(&message_id).await {
                Ok(_) => true,
                Err(error) => {
                    tracing::error!(error = %format!("{error:#}"), %message_id, "a cortex wake failed");
                    false
                }
            };
            let _ = reply.send(ok);
            daemon.release_held().await;
        }
    }
}

/// Runs the daemon until `stop` completes or the accept loop ends.
///
/// # Errors
///
/// Returns an error only when the loop cannot start.
pub async fn run(
    daemon: Daemon,
    listener: UnixListener,
    links: Links,
    camel: Option<Camel>,
    stop: impl Future<Output = ()>,
) -> Result<()> {
    Box::pin(run_with_wake(daemon, listener, None, links, camel, stop)).await
}

/// Runs the daemon with an optional Cortex [`WakeListener`] until `stop` completes.
///
/// # Errors
///
/// Returns an error only when the loop cannot start.
pub async fn run_with_wake(
    mut daemon: Daemon,
    listener: UnixListener,
    wake: Option<WakeListener>,
    links: Links,
    camel: Option<Camel>,
    stop: impl Future<Output = ()>,
) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<Job>(64);
    // Without camel routes, the sender drops at once and the arm sleeps.
    let (camel_tx, mut camel_rx) = mpsc::channel::<crate::camel::Apply>(4);
    let mut tasks = vec![tokio::spawn(accept_loop(listener, tx.clone()))];
    if let Some(wake_listener) = wake {
        let wake_jobs = tx.clone();
        tasks.push(tokio::spawn(async move {
            wake_listener
                .serve_until(
                    move |message_id| {
                        let wake_jobs = wake_jobs.clone();
                        async move {
                            let (reply, done) = oneshot::channel();
                            if wake_jobs
                                .send(Job::Wake { message_id, reply })
                                .await
                                .is_err()
                            {
                                return false;
                            }
                            done.await.unwrap_or(false)
                        }
                    },
                    std::future::pending::<()>(),
                )
                .await;
        }));
    }
    if let Some(link) = links.discord {
        daemon.discord_rest(link.rest.clone());
        let (events_tx, events) = mpsc::unbounded_channel();
        tasks.push(tokio::spawn(run_gateway(
            link.token.clone(),
            link.gateway_url.clone(),
            events_tx,
        )));
        let chats = Arc::clone(&daemon.chats);
        tasks.push(tokio::spawn(discord_loop(
            link.rest,
            chats,
            events,
            tx.clone(),
        )));
    }
    if let Some(api) = links.slack {
        daemon.slack_api(api.clone());
        let (events_tx, events) = mpsc::unbounded_channel();
        tasks.push(tokio::spawn(run_socket(api.clone(), events_tx)));
        let chats = Arc::clone(&daemon.chats);
        tasks.push(tokio::spawn(slack_loop(api, chats, events, tx.clone())));
    }
    if let Some(camel) = &camel {
        tasks.push(tokio::spawn(crate::camel::pump(camel.inbound(), camel_tx)));
    } else {
        drop(camel_tx);
    }
    drop(tx);
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tokio::pin!(stop);
    tracing::info!(assistant = %daemon.assistant(), "littleguy is listening");
    loop {
        tokio::select! {
            job = rx.recv() => {
                let Some(job) = job else { break };
                Box::pin(run_job(&mut daemon, job)).await;
            }
            Some(apply) = camel_rx.recv() => Box::pin(daemon.camel(apply)).await,
            _ = ticker.tick() => {
                if let Err(error) = Box::pin(daemon.tick()).await {
                    tracing::warn!(error = %format!("{error:#}"), "a tick failed");
                }
            }
            () = &mut stop => break,
        }
    }
    for task in tasks {
        task.abort();
    }
    if let Some(camel) = camel {
        camel.stop().await;
    }
    if let Some(connections) = daemon.connections.take() {
        connections.stop().await;
    }
    Ok(())
}

/// Asks the daemon for the cursors of `service`.
async fn cursors(jobs: &mpsc::Sender<Job>, service: Service) -> Option<Vec<Cursor>> {
    let (reply, cursors) = oneshot::channel();
    jobs.send(Job::Cursors { service, reply }).await.ok()?;
    Some(cursors.await.unwrap_or_default())
}

/// Turns gateway events into jobs, and sends each reply.
async fn discord_loop(
    rest: DiscordRest,
    chats: Arc<Chats>,
    mut events: mpsc::UnboundedReceiver<GatewayEvent>,
    jobs: mpsc::Sender<Job>,
) {
    let service = Service::Discord;
    while let Some(event) = events.recv().await {
        match event {
            GatewayEvent::Message(message) => deliver(&chats, &jobs, service, message).await,
            GatewayEvent::Press(press) => answer_discord_press(&rest, &jobs, press).await,
            GatewayEvent::Ready => {
                let Some(cursors) = cursors(&jobs, service).await else {
                    return;
                };
                for cursor in cursors {
                    match rest.messages_after(&cursor.channel_id, &cursor.after).await {
                        Ok(missed) => {
                            for message in missed {
                                deliver(&chats, &jobs, service, message).await;
                            }
                        }
                        Err(error) => tracing::warn!(%error, "reading missed messages failed"),
                    }
                }
            }
        }
    }
}

/// Turns Socket Mode events into jobs, and sends each reply.
async fn slack_loop(
    api: SlackApi,
    chats: Arc<Chats>,
    mut events: mpsc::UnboundedReceiver<SlackEvent>,
    jobs: mpsc::Sender<Job>,
) {
    let service = Service::Slack;
    while let Some(event) = events.recv().await {
        match event {
            SlackEvent::Message(message) => deliver(&chats, &jobs, service, message).await,
            SlackEvent::Press(press) => answer_slack_press(&api, &jobs, press).await,
            SlackEvent::Ready => {
                let Some(cursors) = cursors(&jobs, service).await else {
                    return;
                };
                for cursor in cursors {
                    match api.messages_after(&cursor.channel_id, &cursor.after).await {
                        Ok(missed) => {
                            for message in missed {
                                deliver(&chats, &jobs, service, message).await;
                            }
                        }
                        Err(error) => tracing::warn!(%error, "reading missed messages failed"),
                    }
                }
            }
        }
    }
}

/// Runs one message and sends its reply. A failed send moves no cursor, so
/// the history read after the next reconnect tries it again.
async fn deliver(chats: &Chats, jobs: &mpsc::Sender<Job>, service: Service, message: DmMessage) {
    let (reply, answer) = oneshot::channel();
    let job = Job::Chat {
        service,
        message,
        reply,
    };
    if jobs.send(job).await.is_err() {
        return;
    }
    let Ok(Some(outgoing)) = answer.await else {
        return;
    };
    if let Err(error) = chats
        .send(service, &outgoing.channel_id, &outgoing.text)
        .await
    {
        tracing::warn!(%error, %service, "a chat reply did not go out");
        return;
    }
    if let Some(sender) = outgoing.track {
        let _ = jobs
            .send(Job::Delivered {
                sender,
                reply_to: outgoing.channel_id,
                message_id: outgoing.message_id,
            })
            .await;
    }
}

/// Hands one press to the daemon, and returns its answer.
async fn decide_press(jobs: &mpsc::Sender<Job>, press: Press) -> Option<gate::PressAnswer> {
    let (reply, answer) = oneshot::channel();
    jobs.send(Job::Press { press, reply }).await.ok()?;
    answer.await.ok()
}

/// Hands one Discord button press to the daemon, and sends its answer.
async fn answer_discord_press(rest: &DiscordRest, jobs: &mpsc::Sender<Job>, press: ButtonPress) {
    let job = Press {
        service: Service::Discord,
        user_id: press.user_id.clone(),
        custom_id: press.custom_id.clone(),
        channel_id: press.channel_id.clone(),
    };
    let result = match decide_press(jobs, job).await {
        Some(gate::PressAnswer::Update(text)) => rest.update_pressed(&press, &text).await,
        Some(gate::PressAnswer::Private(text)) => rest.answer_privately(&press, &text).await,
        None => return,
    };
    if let Err(error) = result {
        tracing::warn!(%error, "the answer to a button press did not go out");
    }
}

/// Hands one Slack button press to the daemon, and sends its answer. The
/// socket acknowledged the press already, so this answer has no deadline.
async fn answer_slack_press(api: &SlackApi, jobs: &mpsc::Sender<Job>, press: SlackPress) {
    let job = Press {
        service: Service::Slack,
        user_id: press.user_id.clone(),
        custom_id: press.custom_id.clone(),
        channel_id: press.channel_id.clone(),
    };
    let result: Result<(), ChannelError> = match decide_press(jobs, job).await {
        Some(gate::PressAnswer::Update(text)) => api.update_pressed(&press, &text).await,
        Some(gate::PressAnswer::Private(text)) => api.answer_privately(&press, &text).await,
        None => return,
    };
    if let Err(error) = result {
        tracing::warn!(%error, "the answer to a button press did not go out");
    }
}

async fn accept_loop(listener: UnixListener, tx: mpsc::Sender<Job>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(stream, tx).await {
                        tracing::debug!(error = %format!("{error:#}"), "a connection failed");
                    }
                });
            }
            Err(error) => {
                tracing::warn!(%error, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn serve_connection(stream: UnixStream, tx: mpsc::Sender<Job>) -> Result<()> {
    let uid = stream
        .peer_cred()
        .context("reading the peer credentials")?
        .uid();
    let (read, mut write) = stream.into_split();
    let limit = u64::try_from(MAX_REQUEST_BYTES).unwrap_or(u64::MAX);
    let mut reader = BufReader::new(read.take(limit + 1));
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("reading the request")?;
    let response = if line.len() > MAX_REQUEST_BYTES {
        Response::Error {
            message: format!("the request is longer than {MAX_REQUEST_BYTES} bytes"),
        }
    } else {
        match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let (reply, answer) = oneshot::channel();
                tx.send(Job::Socket {
                    uid,
                    request,
                    reply,
                })
                .await
                .context("the daemon is stopping")?;
                answer.await.context("the daemon dropped the request")?
            }
            Err(error) => Response::Error {
                message: format!("the request is not valid: {error}"),
            },
        }
    };
    let mut out = serde_json::to_vec(&response)?;
    out.push(b'\n');
    write
        .write_all(&out)
        .await
        .context("writing the response")?;
    Ok(())
}

mod gate;

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use littleguy_core::OwnerName;
    use littleguy_models::OfflineModel;

    use super::*;

    const UID: u32 = 1000;

    fn config() -> Config {
        Config::for_owner(OwnerName::new("mark").unwrap())
    }

    fn request(id: &str, text: &str) -> Request {
        Request::Chat {
            message_id: String::from(id),
            text: String::from(text),
        }
    }

    fn open(db: &Path, model: Arc<OfflineModel>) -> Daemon {
        Daemon::open(db, &config(), model, UID, false).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_owner_gets_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(OfflineModel::new());
        let mut daemon = open(&dir.path().join("lg.db"), Arc::clone(&model));
        let response = daemon.handle_request(UID, request("m-1", "hello")).await;
        assert_eq!(
            response,
            Response::Reply {
                text: String::from("littleguy (offline): you said 'hello'")
            }
        );
        assert_eq!(model.calls(), 1);
        assert_eq!(daemon.assistant().as_str(), "assistant/mark");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_same_message_id_runs_once() {
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(OfflineModel::new());
        let mut daemon = open(&dir.path().join("lg.db"), Arc::clone(&model));
        daemon.handle_request(UID, request("m-1", "hello")).await;
        let again = daemon.handle_request(UID, request("m-1", "hello")).await;
        assert_eq!(
            again,
            Response::Duplicate {
                last_answer: Some(String::from("littleguy (offline): you said 'hello'"))
            }
        );
        assert_eq!(model.calls(), 1);
        assert_eq!(daemon.state().await.unwrap().unwrap().segments, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn another_user_is_dropped_and_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(OfflineModel::new());
        let mut daemon = open(&dir.path().join("lg.db"), Arc::clone(&model));
        let response = daemon.handle_request(UID + 1, request("m-1", "hi")).await;
        assert_eq!(response, Response::Dropped);
        assert_eq!(model.calls(), 0);
        assert!(daemon.state().await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_message_id_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(&dir.path().join("lg.db"), Arc::new(OfflineModel::new()));
        let response = daemon.handle_request(UID, request("", "hi")).await;
        assert!(matches!(response, Response::Error { .. }));
    }

    /// The message is acknowledged, then the process dies before it runs.
    /// A new daemon runs it once, and a finished turn is never paid again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_runs_an_acknowledged_message_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let id = AgentId::new("assistant", "mark").unwrap();
        {
            let first = Arc::new(OfflineModel::new());
            let mut daemon = open(&db, Arc::clone(&first));
            let op = AgentOp::message_with_id("hello", "m-1");
            sqlite::send(&mut daemon.rt, &id, &op, Some("m-1")).unwrap();
            assert_eq!(first.calls(), 0);
        }
        let second = Arc::new(OfflineModel::new());
        let mut daemon = open(&db, Arc::clone(&second));
        daemon.tick().await.unwrap();
        assert_eq!(second.calls(), 1);
        let state = daemon.state().await.unwrap().unwrap();
        assert_eq!(
            state.last_answer.as_deref(),
            Some("littleguy (offline): you said 'hello'")
        );
        drop(daemon);

        let third = Arc::new(OfflineModel::new());
        let mut daemon = open(&db, Arc::clone(&third));
        daemon.tick().await.unwrap();
        assert_eq!(
            third.calls(),
            0,
            "a recorded turn is replayed, not paid again"
        );
    }

    async fn raw(socket: &Path, line: &[u8]) -> Response {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(line).await.unwrap();
        write.shutdown().await.unwrap();
        let mut answer = String::new();
        BufReader::new(read).read_line(&mut answer).await.unwrap();
        serde_json::from_str(&answer).unwrap()
    }

    /// The whole socket path: accept, route, run, answer, stop.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_loop_answers_over_the_socket_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("lg.sock");
        let uid = rustix::process::getuid().as_raw();
        let model = Arc::new(OfflineModel::new());
        let daemon = Daemon::open(&dir.path().join("lg.db"), &config(), model, uid, false).unwrap();
        let listener = bind(&socket).await.unwrap();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop = async {
            let _ = stop_rx.await;
        };
        let client = async {
            let reply = crate::client::send(&socket, &request("m-1", "hi"))
                .await
                .unwrap();
            let bad_json = raw(&socket, b"not json\n").await;
            let too_long = raw(&socket, &vec![b'x'; MAX_REQUEST_BYTES + 2]).await;
            stop_tx.send(()).unwrap();
            (reply, bad_json, too_long)
        };
        let (served, (reply, bad_json, too_long)) =
            tokio::join!(run(daemon, listener, Links::default(), None, stop), client);
        served.unwrap();
        assert_eq!(
            reply,
            Response::Reply {
                text: String::from("littleguy (offline): you said 'hi'")
            }
        );
        assert!(
            matches!(&bad_json, Response::Error { message } if message.contains("not valid")),
            "{bad_json:?}"
        );
        assert!(
            matches!(&too_long, Response::Error { message } if message.contains("longer than")),
            "{too_long:?}"
        );
    }

    #[test]
    fn a_second_daemon_on_one_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let _first = open(&db, Arc::new(OfflineModel::new()));
        let second = Daemon::open(&db, &config(), Arc::new(OfflineModel::new()), UID, false);
        let error = format!("{:#}", second.unwrap_err());
        assert!(error.contains("lock"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_socket_is_refused_and_a_stale_one_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run").join("lg.sock");
        let listener = bind(&path).await.unwrap();
        assert!(bind(&path).await.is_err());
        drop(listener);
        assert!(path.exists());
        let again = bind(&path).await;
        assert!(again.is_ok());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    fn discord_open(db: &Path, config: &Config, model: Arc<OfflineModel>) -> Daemon {
        Daemon::open(db, config, model, UID, true).unwrap()
    }

    fn dm(id: &str, author: &str, text: &str) -> DmMessage {
        DmMessage {
            id: id.to_owned(),
            channel_id: format!("dm{author}"),
            author_id: author.to_owned(),
            content: text.to_owned(),
        }
    }

    /// The code inside a pairing text.
    fn code_in(text: &str) -> String {
        let start = text.find("lg pair ").unwrap() + "lg pair ".len();
        text[start..].split('`').next().unwrap().to_owned()
    }

    fn echo(text: &str) -> String {
        format!("littleguy (offline): you said '{text}'")
    }

    #[test]
    fn the_pairing_text_names_the_code_or_says_try_later() {
        let text = pairing_text(Some("ABCD-1234"));
        assert!(text.contains("`lg pair ABCD-1234`"), "{text}");
        assert!(text.contains("10 minutes"), "{text}");
        assert!(pairing_text(None).contains("Try again"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_pair_requests_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = discord_open(
            &dir.path().join("lg.db"),
            &config(),
            Arc::new(OfflineModel::new()),
        );
        let prompt = daemon
            .discord(&dm("1001", "111", "hello"))
            .await
            .unwrap()
            .unwrap();
        let code = code_in(&prompt.text);
        let refused = daemon
            .handle_request(UID + 1, Request::Pair { code: code.clone() })
            .await;
        assert!(matches!(refused, Response::Error { ref message } if message.contains("owner")));
        let typo = daemon
            .handle_request(
                UID,
                Request::Pair {
                    code: String::from("nope"),
                },
            )
            .await;
        assert!(
            matches!(typo, Response::Error { ref message } if message.contains("not a pairing code"))
        );
        let unknown = daemon
            .handle_request(
                UID,
                Request::Pair {
                    code: String::from("ZZZZ-ZZZZ"),
                },
            )
            .await;
        assert!(
            matches!(unknown, Response::Error { ref message } if message.contains("no live pairing"))
        );

        for request in [
            Request::Pairs,
            Request::Unpair {
                sender: String::from("discord:111"),
            },
        ] {
            let refused = daemon.handle_request(UID + 1, request).await;
            assert!(matches!(refused, Response::Error { .. }));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stranger_pairs_through_the_owner_and_unpairs() {
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(OfflineModel::new());
        let mut daemon = discord_open(&dir.path().join("lg.db"), &config(), Arc::clone(&model));

        let first = daemon
            .discord(&dm("1001", "111", "hello"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.channel_id, "dm111");
        assert_eq!(first.track, None, "a pairing prompt moves no cursor");
        let code = code_in(&first.text);
        assert_eq!(model.calls(), 0, "a stranger runs nothing");
        // The stranger asks again and gets the same code.
        let again = daemon
            .discord(&dm("1002", "111", "hello?"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(code_in(&again.text), code);

        let paired = daemon
            .handle_request(
                UID,
                Request::Pair {
                    code: code.to_lowercase(),
                },
            )
            .await;
        assert_eq!(
            paired,
            Response::Paired {
                sender: String::from("discord:111")
            }
        );
        let used = daemon.handle_request(UID, Request::Pair { code }).await;
        assert!(matches!(used, Response::Error { .. }), "a code works once");

        let reply = daemon
            .discord(&dm("1003", "111", "hi"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.text, echo("hi"));
        assert_eq!(reply.track, Some(discord_sender("111").unwrap()));
        assert_eq!(model.calls(), 1);

        let pairs = daemon.handle_request(UID, Request::Pairs).await;
        // An answer can hold a connect link, so the failure does not print it.
        let Response::Pairs { entries } = pairs else {
            panic!("`lg pairs` gave another answer");
        };
        let rows: Vec<(&str, &str, &str)> = entries
            .iter()
            .map(|e| (e.channel.as_str(), e.sender.as_str(), e.source.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("cli", "uid:1000", "owner"),
                ("discord", "discord:111", "paired")
            ]
        );

        let removed = daemon
            .handle_request(
                UID,
                Request::Unpair {
                    sender: String::from("discord:111"),
                },
            )
            .await;
        assert_eq!(removed, Response::Unpaired { removed: true });
        let gone = daemon
            .discord(&dm("1004", "111", "still there?"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            gone.track, None,
            "an unpaired sender gets a pairing prompt again"
        );
        let bad = daemon
            .handle_request(
                UID,
                Request::Unpair {
                    sender: String::new(),
                },
            )
            .await;
        assert_eq!(bad, Response::Unpaired { removed: false });
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pairing_survives_a_restart_and_gives_a_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        {
            let mut daemon = discord_open(&db, &config(), Arc::new(OfflineModel::new()));
            let prompt = daemon
                .discord(&dm("2001", "222", "hi"))
                .await
                .unwrap()
                .unwrap();
            let code = code_in(&prompt.text);
            daemon.handle_request(UID, Request::Pair { code }).await;
        }
        let model = Arc::new(OfflineModel::new());
        let mut daemon = discord_open(&db, &config(), Arc::clone(&model));
        assert_eq!(
            daemon.cursors(Service::Discord),
            vec![Cursor {
                channel_id: String::from("dm222"),
                after: String::from("2001")
            }],
            "the prompted message counts as answered"
        );
        assert_eq!(
            daemon.discord(&dm("2001", "222", "hi")).await.unwrap(),
            None
        );
        let reply = daemon
            .discord(&dm("2002", "222", "back"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.text, echo("back"));
        assert_eq!(model.calls(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reply_goes_out_once_and_a_failed_one_is_sent_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config();
        config.discord.allowed.push(discord_sender("333").unwrap());
        let model = Arc::new(OfflineModel::new());
        let mut daemon = discord_open(&dir.path().join("lg.db"), &config, Arc::clone(&model));
        assert!(
            daemon.cursors(Service::Discord).is_empty(),
            "no DM channel is known yet"
        );

        let first = daemon
            .discord(&dm("3001", "333", "one"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.text, echo("one"));
        // The send failed, so no cursor moved. The same message comes back.
        let resent = daemon
            .discord(&dm("3001", "333", "one"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            resent.text,
            echo("one"),
            "the stored answer goes out, not a new run"
        );
        assert_eq!(model.calls(), 1);

        let sender = discord_sender("333").unwrap();
        daemon.delivered(&sender, "dm333", "3001").unwrap();
        assert_eq!(
            daemon.discord(&dm("3001", "333", "one")).await.unwrap(),
            None
        );
        assert_eq!(
            daemon.cursors(Service::Discord),
            vec![Cursor {
                channel_id: String::from("dm333"),
                after: String::from("3001")
            }]
        );

        // A sender from the config file stays allowed after an unpair.
        let removed = daemon
            .handle_request(
                UID,
                Request::Unpair {
                    sender: sender.to_string(),
                },
            )
            .await;
        assert_eq!(removed, Response::Unpaired { removed: false });
        let Response::Pairs { entries } = daemon.handle_request(UID, Request::Pairs).await else {
            panic!("not pairs");
        };
        assert!(
            entries
                .iter()
                .any(|e| e.sender == "discord:333" && e.source == "config")
        );
        let next = daemon
            .discord(&dm("3002", "333", "two"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.text, echo("two"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_author_id_is_an_error_and_the_cli_takes_no_discord() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = discord_open(
            &dir.path().join("lg.db"),
            &config(),
            Arc::new(OfflineModel::new()),
        );
        assert!(daemon.discord(&dm("1", "not-a-number", "x")).await.is_err());
        let mut plain = open(
            &dir.path().join("other").join("lg.db"),
            Arc::new(OfflineModel::new()),
        );
        assert_eq!(
            plain.discord(&dm("1", "444", "x")).await.unwrap(),
            None,
            "no discord rule drops it"
        );
    }

    /// The whole Discord path: a stranger pairs over the socket, then a
    /// reconnect reads the missed history and answers each message once.
    #[tokio::test(flavor = "multi_thread")]
    async fn discord_pairs_answers_and_backfills_once() {
        use littleguy_channels::testing::{FakeDiscord, dm_event};

        let fake = FakeDiscord::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("lg.sock");
        let uid = rustix::process::getuid().as_raw();
        let model = Arc::new(OfflineModel::new());
        let daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &config(),
            Arc::clone(&model) as Arc<dyn AgentModel>,
            uid,
            true,
        )
        .unwrap();
        let listener = bind(&socket).await.unwrap();
        let link = DiscordLink {
            rest: DiscordRest::new(fake.api_base.clone(), "token"),
            token: String::from("token"),
            gateway_url: fake.gateway_url.clone(),
        };
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop = async {
            let _ = stop_rx.await;
        };
        let script = async {
            assert!(fake.wait_for(10, |f| !f.identifies().is_empty()).await);
            fake.push_dm("5001", "dm555", "555", "hello");
            assert!(fake.wait_for(10, |f| f.sent().len() == 1).await);
            let code = code_in(&fake.sent()[0].content);
            let paired = crate::client::send(&socket, &Request::Pair { code })
                .await
                .unwrap();
            assert_eq!(
                paired,
                Response::Paired {
                    sender: String::from("discord:555")
                }
            );

            fake.push_dm("5002", "dm555", "555", "hi");
            assert!(fake.wait_for(10, |f| f.sent().len() == 2).await);

            // The daemon misses two messages while it is away.
            for (id, text) in [
                ("5001", "hello"),
                ("5002", "hi"),
                ("5003", "missed"),
                ("5004", "also"),
            ] {
                fake.add_history(dm_event(id, "dm555", "555", text));
            }
            fake.drop_connection();
            assert!(fake.wait_for(10, |f| f.sent().len() == 4).await);
            // A live copy of a backfilled message runs nothing new.
            fake.push_dm("5004", "dm555", "555", "also");
            fake.push_dm("5005", "dm555", "555", "last");
            assert!(fake.wait_for(10, |f| f.sent().len() == 5).await);
            stop_tx.send(()).unwrap();
        };
        let (served, ()) = tokio::join!(
            run(
                daemon,
                listener,
                Links {
                    discord: Some(link),
                    ..Links::default()
                },
                None,
                stop
            ),
            script
        );
        served.unwrap();
        let sent: Vec<String> = fake.sent().into_iter().map(|s| s.content).collect();
        assert_eq!(
            sent[1..],
            [echo("hi"), echo("missed"), echo("also"), echo("last")]
        );
        assert!(fake.sent().iter().all(|s| s.channel_id == "dm555"));
        assert_eq!(model.calls(), 4);
        assert!(fake.connects() >= 2);
    }

    /// The whole Slack path: a stranger pairs over the socket, then a
    /// message gets one answer, and a missed message is read back once.
    #[tokio::test(flavor = "multi_thread")]
    async fn slack_pairs_answers_and_backfills_once() {
        use littleguy_channels::testing::slack::{APP_TOKEN, BOT_TOKEN, FakeSlack, dm_event};

        let fake = FakeSlack::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("lg.sock");
        let uid = rustix::process::getuid().as_raw();
        let model = Arc::new(OfflineModel::new());
        let extras = Extras {
            slack: true,
            ..Extras::default()
        };
        let daemon = Daemon::open_with(
            &dir.path().join("lg.db"),
            &config(),
            Arc::clone(&model) as Arc<dyn AgentModel>,
            uid,
            false,
            extras,
        )
        .unwrap();
        let listener = bind(&socket).await.unwrap();
        let links = Links {
            slack: Some(SlackApi::new(fake.api_base.clone(), BOT_TOKEN, APP_TOKEN)),
            ..Links::default()
        };
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop = async {
            let _ = stop_rx.await;
        };
        let script =
            async {
                assert!(fake.wait_for(10, |f| f.connects() == 1).await);
                fake.push_dm("5001.000001", "D555", "U555", "hello");
                assert!(fake.wait_for(10, |f| f.posted().len() == 1).await);
                let code = code_in(&fake.posted()[0].text);
                let paired = crate::client::send(&socket, &Request::Pair { code })
                    .await
                    .unwrap();
                assert_eq!(
                    paired,
                    Response::Paired {
                        sender: String::from("slack:U555")
                    }
                );
                let pairs = crate::client::send(&socket, &Request::Pairs).await.unwrap();
                let Response::Pairs { entries } = pairs else {
                    panic!("not a list of pairs");
                };
                assert!(entries.iter().any(|e| e.channel == "slack"
                    && e.sender == "slack:U555"
                    && e.source == "paired"));

                fake.push_dm("5002.000001", "D555", "U555", "hi");
                assert!(fake.wait_for(10, |f| f.posted().len() == 2).await);

                // The daemon misses two messages while it is away.
                for (ts, text) in [
                    ("5001.000001", "hello"),
                    ("5002.000001", "hi"),
                    ("5002.000002", "missed"),
                    ("5003.000000", "also"),
                ] {
                    fake.add_history(dm_event(ts, "D555", "U555", text));
                }
                fake.drop_connection();
                assert!(fake.wait_for(10, |f| f.posted().len() == 4).await);
                // A live copy of a backfilled message runs nothing new.
                fake.push_dm("5003.000000", "D555", "U555", "also");
                fake.push_dm("5004.000000", "D555", "U555", "last");
                assert!(fake.wait_for(10, |f| f.posted().len() == 5).await);
                let unpaired = crate::client::send(
                    &socket,
                    &Request::Unpair {
                        sender: String::from("slack:U555"),
                    },
                )
                .await
                .unwrap();
                assert_eq!(unpaired, Response::Unpaired { removed: true });
                stop_tx.send(()).unwrap();
            };
        let (served, ()) = tokio::join!(run(daemon, listener, links, None, stop), script);
        served.unwrap();
        let posted: Vec<String> = fake.posted().into_iter().map(|p| p.text).collect();
        assert_eq!(
            posted[1..],
            [echo("hi"), echo("missed"), echo("also"), echo("last")]
                .map(|text| littleguy_channels::slack::escape(&text))
        );
        assert!(fake.posted().iter().all(|p| p.channel == "D555"));
        assert_eq!(model.calls(), 4);
        assert!(fake.connects() >= 2);
    }

    #[test]
    fn a_chat_message_has_a_key_and_a_cursor() {
        let message = DmMessage {
            id: String::from("5.000001"),
            channel_id: String::from("D1"),
            author_id: String::from("U1"),
            content: String::from("x"),
        };
        let slack = chat_message_id(Service::Slack, &message).unwrap();
        assert_eq!(slack.as_str(), "slack:D1:5.000001");
        assert_eq!(cursor_of(&slack), "5.000001");
        let discord = chat_message_id(Service::Discord, &message).unwrap();
        assert_eq!(cursor_of(&discord), "5.000001");
        assert_eq!(cursor_of(&MessageId::new("plain").unwrap()), "");
        assert_eq!(
            chat_sender(Service::Slack, "U1").unwrap().as_str(),
            "slack:U1"
        );
        assert!(chat_sender(Service::Slack, "42").is_err());
        assert!(chat_sender(Service::Discord, "U1").is_err());
    }

    fn camel_config(dir: &Path, noop: bool) -> Config {
        let mut config = config();
        let from = format!(
            "file:{}?initialDelay=0&delay=100&{}",
            dir.display(),
            if noop { "noop=true" } else { "delete=true" }
        );
        config.camel.push(littleguy_core::CamelRoute {
            id: String::from("inbox"),
            from,
            prompt: Some(String::from("A file arrived.")),
        });
        config
    }

    /// Runs the daemon with the camel routes of `config` until `done`
    /// holds or 15 seconds pass. Returns whether `done` held.
    async fn run_camel(
        db: &Path,
        config: &Config,
        model: Arc<OfflineModel>,
        done: impl Fn(&OfflineModel) -> bool + Sync,
    ) -> bool {
        let dir = db.parent().unwrap();
        let daemon = Daemon::open(
            db,
            config,
            Arc::clone(&model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap();
        let camel = Camel::start(daemon.camel_mapper(config), db).await.unwrap();
        let listener = bind(&dir.join("lg.sock")).await.unwrap();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop = async {
            let _ = stop_rx.await;
        };
        let watch = async {
            let mut held = false;
            for _ in 0..150 {
                if done(&model) {
                    held = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            stop_tx.send(()).unwrap();
            held
        };
        let (served, held) = tokio::join!(
            run(daemon, listener, Links::default(), Some(camel), stop),
            watch
        );
        served.unwrap();
        held
    }

    /// A file in the watched directory becomes one message for the
    /// assistant, and the route deletes it once the write is durable.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_route_feeds_the_assistant() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("in");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("note.txt"), "buy milk").unwrap();
        // A body that is not UTF-8 is poison: a dead letter, not a turn.
        std::fs::write(inbox.join("bad.bin"), [0xff_u8, 0xfe]).unwrap();
        let db = dir.path().join("lg.db");
        let config = camel_config(&inbox, false);
        let model = Arc::new(OfflineModel::new());
        let held = run_camel(&db, &config, Arc::clone(&model), |m| m.calls() >= 1).await;
        assert!(held, "the file never reached the assistant");
        assert_eq!(model.calls(), 1);
        let daemon = open(&db, Arc::new(OfflineModel::new()));
        let state = daemon.state().await.unwrap().unwrap();
        assert_eq!(
            state.last_answer.as_deref(),
            Some("littleguy (offline): you said 'A file arrived.\n\nbuy milk'")
        );
        drop(daemon);
        for _ in 0..50 {
            if std::fs::read_dir(&inbox).unwrap().next().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let left: Vec<_> = std::fs::read_dir(&inbox).unwrap().collect();
        assert!(
            left.is_empty(),
            "both files are settled and deleted: {left:?}"
        );
    }

    /// A route with `noop=true` reads every file again after a restart. The
    /// signal key of each file is the same, so a file runs once.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_read_again_after_a_restart_runs_once() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("in");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("a.txt"), "first").unwrap();
        let db = dir.path().join("lg.db");
        let config = camel_config(&inbox, true);
        let first = Arc::new(OfflineModel::new());
        assert!(run_camel(&db, &config, Arc::clone(&first), |m| m.calls() >= 1).await);
        assert_eq!(first.calls(), 1);

        std::fs::write(inbox.join("b.txt"), "second").unwrap();
        let second = Arc::new(OfflineModel::new());
        assert!(run_camel(&db, &config, Arc::clone(&second), |m| m.calls() >= 1).await);
        // The poll that read b.txt read a.txt too. Give it time to misbehave.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(second.calls(), 1, "a.txt ran once, before the restart");
        let daemon = open(&db, Arc::new(OfflineModel::new()));
        let state = daemon.state().await.unwrap().unwrap();
        assert!(
            state
                .last_answer
                .as_deref()
                .is_some_and(|a| a.contains("second")),
            "{state:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_camel_channel_allows_only_the_config_routes() {
        let dir = tempfile::tempdir().unwrap();
        let config = camel_config(dir.path(), true);
        let daemon = open_with(&dir.path().join("lg.db"), &config);
        let channel = ChannelId::new(crate::camel::CAMEL_CHANNEL).unwrap();
        let allowed: Vec<&str> = daemon
            .routing
            .allowed(&channel)
            .map(SenderId::as_str)
            .collect();
        assert_eq!(allowed, vec!["camel:inbox"]);
        let plain = open(
            &dir.path().join("other").join("lg.db"),
            Arc::new(OfflineModel::new()),
        );
        assert_eq!(plain.routing.allowed(&channel).count(), 0);
    }

    fn open_with(db: &Path, config: &Config) -> Daemon {
        Daemon::open(db, config, Arc::new(OfflineModel::new()), UID, false).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oauth_requests_are_for_the_owner_and_need_the_section() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut daemon = open(&db, Arc::new(OfflineModel::new()));
        let refused = daemon.handle_request(UID + 1, Request::Connections).await;
        assert!(matches!(refused, Response::Error { ref message } if message.contains("owner")));
        let none = daemon.handle_request(UID, Request::Connections).await;
        assert!(matches!(none, Response::Error { ref message } if message.contains("[san_mateo]")));

        let fake = san_mateo::MemoryTransport::new();
        let config = crate::oauth::tests::section("acme", None);
        let connections = crate::oauth::tests::start(&db, &config, &fake)
            .await
            .unwrap();
        let mut daemon = daemon.with_connections(connections);
        let link = daemon
            .handle_request(
                UID,
                Request::Connect {
                    integration: String::from("acme"),
                },
            )
            .await;
        // The link holds a session token, so the assertion does not print it.
        assert!(
            matches!(&link, Response::ConnectLink { link } if link.contains("connect_session_token="))
        );
        let unknown = daemon
            .handle_request(
                UID,
                Request::Connect {
                    integration: String::from("x"),
                },
            )
            .await;
        assert!(matches!(unknown, Response::Error { .. }));
        // These answers come from San Mateo. The checks do not print them.
        let list = daemon.handle_request(UID, Request::Connections).await;
        assert!(matches!(list, Response::Connections { ref entries } if entries.is_empty()));
        let gone = daemon
            .handle_request(
                UID,
                Request::Disconnect {
                    integration: String::from("acme"),
                    connection: String::from("c-1"),
                },
            )
            .await;
        assert!(matches!(gone, Response::Disconnected { removed: false }));
        if let Some(connections) = daemon.connections.take() {
            connections.stop().await;
        }
    }

    /// A model for the memory tests. `remember <note>` calls the memory
    /// tool. `save <path> <text>` and `show <path>` call the file tools.
    /// `run <code>` calls `run_code`, and answers `ran: <stdout>`.
    /// `use <tool> <json>` calls any tool, and answers `got: <text>` for a
    /// text result.
    /// `recall` answers with the notes in its prompt, `quiet` gets
    /// `HEARTBEAT_OK`, and any other text gets `ok: <text>`. A
    /// consolidation pass gets one fact per episode that says `tea`.
    #[derive(Debug, Default)]
    struct MemoryModel {
        calls: std::sync::atomic::AtomicU64,
        passes: std::sync::atomic::AtomicU64,
    }

    impl MemoryModel {
        fn text(message: &autumn_harvest_agent::ChatMessage) -> String {
            message
                .content
                .iter()
                .filter_map(|part| match part {
                    autumn_harvest_agent::ContentPart::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect()
        }

        fn reply(request: &autumn_harvest_agent::ChatRequest) -> autumn_harvest_agent::ContentPart {
            use autumn_harvest_agent::{ChatRole, ContentPart};
            let system = request
                .messages
                .first()
                .filter(|message| message.role == ChatRole::System)
                .map(Self::text)
                .unwrap_or_default();
            let last = request.messages.last();
            let user = last.map(Self::text).unwrap_or_default();
            if system == autumn_harvest_agent::consolidate::PASS_SYSTEM {
                let facts: Vec<serde_json::Value> = user
                    .lines()
                    .filter(|line| line.contains("tea"))
                    .filter_map(|line| line.split("id=\"").nth(1)?.split('"').next())
                    .map(|id| {
                        serde_json::json!({
                            "subject": "mark", "relation": "likes", "object": "tea",
                            "episodes": [id],
                        })
                    })
                    .collect();
                return ContentPart::Text(serde_json::json!({ "facts": facts }).to_string());
            }
            let answered = last.is_some_and(|message| {
                message
                    .content
                    .iter()
                    .any(|part| matches!(part, ContentPart::ToolResult { .. }))
            });
            if answered {
                // A read answers with the text of the file.
                let read = last
                    .into_iter()
                    .flat_map(|message| &message.content)
                    .find_map(|part| match part {
                        ContentPart::ToolResult { content, .. } => {
                            serde_json::from_str::<serde_json::Value>(content).ok()
                        }
                        _ => None,
                    })
                    .and_then(|result| {
                        let ran = result["stdout"]
                            .as_str()
                            .map(|out| format!("ran: {}", out.trim()));
                        let said = result.as_str().map(|text| format!("got: {text}"));
                        result["text"].as_str().map(str::to_owned).or(ran).or(said)
                    });
                return ContentPart::Text(read.unwrap_or_else(|| String::from("noted")));
            }
            let file_call = |name: &str, arguments: serde_json::Value| ContentPart::ToolCall {
                id: String::from("call-1"),
                name: String::from(name),
                arguments,
            };
            if let Some(rest) = user.strip_prefix("save ") {
                let (path, text) = rest.split_once(' ').unwrap_or((rest, ""));
                return file_call(
                    "write_file",
                    serde_json::json!({"path": path, "text": text}),
                );
            }
            if let Some(code) = user.strip_prefix("run ") {
                return file_call(
                    crate::sandbox::TOOL,
                    serde_json::json!({"runtime": "sh", "code": code}),
                );
            }
            if let Some(rest) = user.strip_prefix("use ") {
                let (tool, input) = rest.split_once(' ').unwrap_or((rest, "{}"));
                let input = serde_json::from_str(input).unwrap_or_default();
                return file_call(tool, input);
            }
            if let Some(path) = user.strip_prefix("show ") {
                return file_call("read_file", serde_json::json!({"path": path}));
            }
            if let Some(note) = user.strip_prefix("remember ") {
                return ContentPart::ToolCall {
                    id: String::from("call-1"),
                    name: String::from(autumn_harvest_agent::memory::MEMORY_TOOL),
                    arguments: serde_json::json!({
                        "action": "add", "block": "memory", "text": note,
                    }),
                };
            }
            if user == "quiet" {
                return ContentPart::Text(String::from(
                    autumn_harvest_agent::delivery::HEARTBEAT_OK,
                ));
            }
            if user != "recall" {
                return ContentPart::Text(format!("ok: {user}"));
            }
            let notes: Vec<&str> = system
                .lines()
                .filter_map(|line| line.strip_prefix("- "))
                .collect();
            ContentPart::Text(format!("I know: {}", notes.join("; ")))
        }
    }

    impl AgentModel for MemoryModel {
        fn chat<'a>(
            &'a self,
            request: &'a autumn_harvest_agent::ChatRequest,
        ) -> autumn_harvest_agent::BoxFuture<
            'a,
            Result<autumn_harvest_agent::ChatResponse, autumn_harvest_agent::AgentError>,
        > {
            use std::sync::atomic::Ordering;
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let content = Self::reply(request);
                if request.tools.is_empty() {
                    self.passes.fetch_add(1, Ordering::SeqCst);
                }
                let stop_reason =
                    if matches!(content, autumn_harvest_agent::ContentPart::ToolCall { .. }) {
                        autumn_harvest_agent::StopReason::ToolUse
                    } else {
                        autumn_harvest_agent::StopReason::EndTurn
                    };
                Ok(autumn_harvest_agent::ChatResponse {
                    content: vec![content],
                    stop_reason,
                    usage: autumn_harvest_agent::TokenUsage::new(1, 1),
                })
            })
        }
    }

    fn reply_text(response: &Response) -> &str {
        match response {
            Response::Reply { text } => text,
            _ => "",
        }
    }

    fn notes(db: &Path) -> Vec<String> {
        use autumn_harvest_agent::memory::{MemoryScope, MemoryStore};
        let stores = littleguy_stores::SqliteStores::open(crate::memory::path_for(db)).unwrap();
        let scope = MemoryScope::new("agent:assistant/mark");
        let blocks = futures_ready(stores.load(&scope)).unwrap();
        blocks
            .into_iter()
            .find(|block| block.label == "memory")
            .map(|block| block.entries)
            .unwrap_or_default()
    }

    /// Runs a store future, which is ready at once.
    fn futures_ready<T>(future: impl std::future::Future<Output = T>) -> T {
        use std::task::{Context, Poll, Waker};
        let mut future = std::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("a store call waited"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_note_reaches_the_next_run_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        {
            let mut daemon =
                Daemon::open(&db, &config(), Arc::new(MemoryModel::default()), UID, false).unwrap();
            let noted = daemon
                .handle_request(UID, request("m-1", "remember Mark drinks tea"))
                .await;
            assert_eq!(reply_text(&noted), "noted");
        }
        assert_eq!(notes(&db), vec![String::from("Mark drinks tea")]);

        let mut daemon =
            Daemon::open(&db, &config(), Arc::new(MemoryModel::default()), UID, false).unwrap();
        let recall = daemon.handle_request(UID, request("m-2", "recall")).await;
        assert_eq!(reply_text(&recall), "I know: Mark drinks tea");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_nightly_pass_turns_episodes_into_facts_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let model = Arc::new(MemoryModel::default());
        let mut daemon = Daemon::open(
            &db,
            &config(),
            Arc::clone(&model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap();
        daemon
            .handle_request(UID, request("m-1", "I like tea"))
            .await;
        daemon
            .handle_request(UID, request("m-2", "the weather is bad"))
            .await;

        // Fire the nightly schedule now, as 03:00 would.
        let schedules = daemon.rt.list_schedules().unwrap();
        assert_eq!(schedules.len(), 1);
        daemon
            .rt
            .trigger_schedule(
                autumn_harvest_agent::consolidate_all::CONSOLIDATE_ALL_WORKFLOW_NAME,
                false,
                None,
            )
            .unwrap();
        daemon.rt.run_until_idle().await.unwrap();
        let passes = model.passes.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(passes, 1);
        let facts = notes(&db);
        assert_eq!(facts.len(), 1, "{facts:?}");
        assert!(
            facts[0].starts_with("mark likes tea [episodes: "),
            "{facts:?}"
        );

        // No new episode waits, so the next firing calls no model.
        daemon
            .rt
            .trigger_schedule(
                autumn_harvest_agent::consolidate_all::CONSOLIDATE_ALL_WORKFLOW_NAME,
                false,
                None,
            )
            .unwrap();
        daemon.rt.run_until_idle().await.unwrap();
        assert_eq!(model.passes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn memory_off_keeps_no_notes_and_no_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut on = config();
        drop(open_with(&db, &on));
        on.memory.enabled = false;
        let mut daemon = open_with(&db, &on);
        assert!(daemon.rt.list_schedules().unwrap().is_empty());
        let reply = daemon.handle_request(UID, request("m-1", "hi")).await;
        assert!(matches!(reply, Response::Reply { .. }));
        // A daemon that never kept memory opens no memory file.
        let fresh = dir.path().join("fresh");
        std::fs::create_dir(&fresh).unwrap();
        drop(open_with(&fresh.join("lg.db"), &on));
        assert!(!crate::memory::path_for(&fresh.join("lg.db")).exists());
    }

    #[test]
    fn a_bad_consolidation_schedule_is_refused_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut bad = config();
        bad.memory.timezone = String::from("Mars/Olympus");
        let error = Daemon::open(
            &dir.path().join("lg.db"),
            &bad,
            Arc::new(OfflineModel::new()),
            UID,
            false,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("memory.consolidate"),
            "{error:#}"
        );
    }

    fn brief_config(prompt: &str, to: Option<&str>) -> Config {
        let mut config = config();
        config.brief = Some(littleguy_core::BriefConfig {
            schedule: String::from("0 8 * * *"),
            timezone: String::from("UTC"),
            prompt: String::from(prompt),
            to: to.map(|sender| SenderId::new(sender).unwrap()),
        });
        config
    }

    /// Ticks until `done` holds, at most twenty times.
    async fn tick_until(daemon: &mut Daemon, done: impl Fn() -> bool) {
        for _ in 0..20 {
            daemon.tick().await.unwrap();
            if done() {
                return;
            }
        }
    }

    /// A clock that a test moves by hand.
    #[derive(Clone)]
    struct HandClock(Arc<std::sync::atomic::AtomicI64>);

    impl HandClock {
        fn at(text: &str) -> Self {
            let clock = Self(Arc::new(std::sync::atomic::AtomicI64::new(0)));
            clock.set(text);
            clock
        }

        fn set(&self, text: &str) {
            let millis = DateTime::parse_from_rfc3339(text)
                .unwrap()
                .timestamp_millis();
            self.0.store(millis, std::sync::atomic::Ordering::SeqCst);
        }

        fn now(&self) -> DateTime<Utc> {
            let millis = self.0.load(std::sync::atomic::Ordering::SeqCst);
            DateTime::from_timestamp_millis(millis).unwrap()
        }
    }

    /// A daemon with a brief, a hand clock and a fake Discord.
    fn brief_daemon(
        db: &Path,
        config: &Config,
        model: &Arc<MemoryModel>,
        clock: &HandClock,
        fake: &littleguy_channels::testing::FakeDiscord,
    ) -> Daemon {
        let hand = clock.clone();
        let daemon = Daemon::open(
            db,
            config,
            Arc::clone(model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap()
        .with_clock(move || hand.now());
        daemon.discord_rest(DiscordRest::new(fake.api_base.clone(), "token"));
        daemon
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_brief_reaches_the_owner_once_a_day() {
        use littleguy_channels::testing::FakeDiscord;
        use std::sync::atomic::Ordering;

        let fake = FakeDiscord::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(MemoryModel::default());
        let clock = HandClock::at("2026-11-09T07:00:00Z");
        let config = brief_config("Morning.", Some("discord:555"));
        let mut daemon = brief_daemon(&dir.path().join("lg.db"), &config, &model, &clock, &fake);

        let armed = daemon.arm_brief().await.unwrap();
        assert_eq!(armed.as_deref(), Some("brief:2026-11-09T08:00:00+00:00"));
        // Before the slot, ticks send nothing and arm nothing new.
        for _ in 0..3 {
            daemon.tick().await.unwrap();
        }
        assert_eq!(daemon.arm_brief().await.unwrap(), None);
        assert_eq!(fake.sent().len(), 0);
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);

        clock.set("2026-11-09T08:00:01Z");
        tick_until(&mut daemon, || fake.sent().len() == 1).await;
        let sent = fake.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].channel_id, "dm555");
        assert_eq!(sent[0].content, "ok: Morning.");
        let next = daemon.brief.as_ref().and_then(Calendar::slot);
        assert_eq!(
            next.map(|slot| slot.to_rfc3339()).as_deref(),
            Some("2026-11-10T08:00:00+00:00")
        );

        // More ticks in the same slot run nothing new.
        for _ in 0..3 {
            daemon.tick().await.unwrap();
        }
        assert_eq!(fake.sent().len(), 1);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);

        // A chat answer goes to its channel, not to the brief.
        let answer = daemon.handle_request(UID, request("m-1", "hello")).await;
        assert_eq!(reply_text(&answer), "ok: hello");
        assert_eq!(fake.sent().len(), 1);

        clock.set("2026-11-10T08:00:01Z");
        tick_until(&mut daemon, || fake.sent().len() == 2).await;
        assert_eq!(fake.sent().len(), 2);
        assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_brief_to_a_slack_user_goes_to_slack() {
        use littleguy_channels::testing::FakeDiscord;
        use littleguy_channels::testing::slack::{APP_TOKEN, BOT_TOKEN, FakeSlack};

        let discord = FakeDiscord::start().await.unwrap();
        let slack = FakeSlack::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(MemoryModel::default());
        let clock = HandClock::at("2026-11-09T07:00:00Z");
        let config = brief_config("Morning <team>.", Some("slack:U555"));
        let mut daemon = brief_daemon(&dir.path().join("lg.db"), &config, &model, &clock, &discord);
        daemon.tick().await.unwrap();
        clock.set("2026-11-09T08:00:01Z");
        // Slack is off at first, so the brief logs and stays unsent.
        tick_until(&mut daemon, || {
            model.calls.load(std::sync::atomic::Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(
            slack.posted(),
            [] as [littleguy_channels::testing::slack::Posted; 0]
        );
        daemon.slack_api(SlackApi::new(slack.api_base.clone(), BOT_TOKEN, APP_TOKEN));
        clock.set("2026-11-10T08:00:01Z");
        tick_until(&mut daemon, || slack.posted().len() == 1).await;
        let posted = slack.posted();
        assert_eq!(posted[0].channel, "DU555");
        assert_eq!(posted[0].text, "ok: Morning &lt;team&gt;.");
        assert_eq!(discord.sent(), [] as [littleguy_channels::testing::Sent; 0]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_quiet_brief_sends_nothing() {
        use littleguy_channels::testing::FakeDiscord;
        use std::sync::atomic::Ordering;

        let fake = FakeDiscord::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(MemoryModel::default());
        let clock = HandClock::at("2026-11-09T07:00:00Z");
        let mut config = brief_config("quiet", Some("discord:555"));
        // The brief opens its store even with memory off.
        config.memory.enabled = false;
        let mut daemon = brief_daemon(&dir.path().join("lg.db"), &config, &model, &clock, &fake);
        daemon.tick().await.unwrap();
        clock.set("2026-11-09T08:00:01Z");
        tick_until(&mut daemon, || model.calls.load(Ordering::SeqCst) == 1).await;
        for _ in 0..3 {
            daemon.tick().await.unwrap();
        }
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.sent().len(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_arms_the_same_slot_once() {
        use littleguy_channels::testing::FakeDiscord;
        use std::sync::atomic::Ordering;

        let fake = FakeDiscord::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let model = Arc::new(MemoryModel::default());
        let clock = HandClock::at("2026-11-09T07:00:00Z");
        let config = brief_config("Morning.", Some("discord:555"));
        {
            let mut daemon = brief_daemon(&db, &config, &model, &clock, &fake);
            daemon.tick().await.unwrap();
        }
        clock.set("2026-11-09T07:30:00Z");
        let mut daemon = brief_daemon(&db, &config, &model, &clock, &fake);
        // The new daemon arms the same slot. The engine takes it once.
        assert!(daemon.arm_brief().await.unwrap().is_some());
        clock.set("2026-11-09T08:00:01Z");
        tick_until(&mut daemon, || fake.sent().len() == 1).await;
        for _ in 0..3 {
            daemon.tick().await.unwrap();
        }
        assert_eq!(fake.sent().len(), 1);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_bad_brief_schedule_is_refused_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut bad = brief_config("Morning.", None);
        if let Some(brief) = bad.brief.as_mut() {
            brief.timezone = String::from("Mars/Olympus");
        }
        let Err(error) = Daemon::open(
            &dir.path().join("lg.db"),
            &bad,
            Arc::new(OfflineModel::new()),
            UID,
            false,
        ) else {
            panic!("a bad brief schedule opened");
        };
        assert!(format!("{error:#}").contains("brief.schedule"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_workspace_file_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let open =
            || Daemon::open(&db, &config(), Arc::new(MemoryModel::default()), UID, false).unwrap();
        {
            let mut daemon = open();
            let saved = daemon
                .handle_request(UID, request("w-1", "save notes/tea.md green tea"))
                .await;
            assert_eq!(reply_text(&saved), "noted");
        }
        assert!(crate::tools::workspace_for(&db).is_dir());
        let mut daemon = open();
        let shown = daemon
            .handle_request(UID, request("w-2", "show notes/tea.md"))
            .await;
        assert_eq!(reply_text(&shown), "green tea");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn files_off_gives_no_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut config = config();
        config.tools.files = false;
        let mut daemon =
            Daemon::open(&db, &config, Arc::new(MemoryModel::default()), UID, false).unwrap();
        // The model calls tools that the assistant does not have, so
        // nothing is kept.
        daemon
            .handle_request(UID, request("w-1", "save notes/tea.md green"))
            .await;
        let shown = daemon
            .handle_request(UID, request("w-2", "show notes/tea.md"))
            .await;
        assert_ne!(reply_text(&shown), "green");
        assert!(!crate::tools::workspace_for(&db).exists());
    }

    mod flow;
    mod google;
    mod sandboxed;
}
