//! Cortex `littleman` pool consumer (`specs/0005`, `REQ-LG-POOL-01..07`).

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::io::{self, Read as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use autumn_harvest_agent::model::BoxFuture;
use autumn_harvest_agent::{AgentHarness, AgentId, AgentModel, AgentOp, AgentProfile, sqlite};
use autumn_harvest_sqlite::SqliteRuntime;
use littleguy_core::{
    ASSISTANT_KIND, AuthorityGrant, BiscuitMandateClaims, Config, DeliveryToken, EstateSubject,
    GhostBinding, LittleguySeatSubstrate, PoolPeerId, ReversibilityClass, RoleName, SandboxClass,
    SeatJobSpec, SeatReceipt, ShellId, TaskId, WheelhorseGate,
};
use littleguy_stores::{
    AdapterError, HttpMethod, HttpTransport, ReqwestHttpTransport, StoreHttpRequest,
    StoreHttpResponse, join_url_segments, parse_clean_base_url, sha256_hex,
};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

/// Default allowed sender role on Cortex (`escapement-serve` pool publisher).
pub const DEFAULT_ALLOWED_SENDER: &str = "service-escapement-pool";
/// Default Cortex role reported by this worker (`escapement-serve/src/pool.rs:182`).
pub const DEFAULT_MEMBER_ROLE: &str = "service-pool-littleman-1";
/// Default long-poll wait in milliseconds for `/v1/mesh/pool/recv`.
pub const DEFAULT_WAIT_MS: u32 = 5_000;
/// Default redelivery delay in milliseconds for `/v1/mesh/pool/nack`.
pub const DEFAULT_NACK_DELAY_MS: u32 = 5_000;
/// Maximum size in bytes of the `--token-file` read (`REQ-LG-POOL-01`).
pub const MAX_TOKEN_FILE_BYTES: usize = 4 * 1024;
/// Maximum inbound delivery body bytes (`escapement-serve/src/pool.rs:44` `POOL_BODY_MAX`).
pub const MAX_DELIVERY_BODY_BYTES: usize = 12 * 1024;
/// Maximum inbound delivery subject bytes (`escapement-serve/src/pool.rs:48` `POOL_SUBJECT_MAX`).
pub const MAX_DELIVERY_SUBJECT_BYTES: usize = 512;
/// Maximum serialized `refs` bytes (`escapement-serve/src/pool.rs:50` `POOL_REFS_MAX`).
pub const MAX_DELIVERY_REFS_BYTES: usize = 16 * 1024;
/// Maximum reply body bytes (`escapement-serve/src/pool.rs:1005` `MAX_RESULT_BYTES`).
pub const MAX_REPLY_BODY_BYTES: usize = 8 * 1024;
/// Maximum characters in `refs.error` and `body.error` on permanent turn failures.
pub const MAX_ERROR_CHARS: usize = 1_024;
/// Maximum `(TaskId, attempt)` entries held in the worker idempotency cache.
pub const MAX_CACHED_RESULTS: usize = 256;
/// Maximum raw delivery token bytes accepted when settling a poison delivery with `term`.
const MAX_RAW_SETTLE_TOKEN_BYTES: usize = 4 * 1024;

/// Errors returned while configuring or running `lg pool`.
#[derive(Debug, Error)]
pub enum PoolError {
    /// Metadata lookup or file read failed on `--token-file`.
    #[error("failed to read the pool token file: {0}")]
    TokenFileStat(String),
    /// `--token-file` is a symlink or non-regular file.
    #[error("the pool token file must be a regular file and not a symlink")]
    TokenFileNotRegular,
    /// `--token-file` has permissions other than `0600`.
    #[error("the pool token file has mode {mode:04o}; expected 0600")]
    TokenFilePermissions {
        /// Observed permission bits (`mode & 0o7777`).
        mode: u32,
    },
    /// `--token-file` is owned by another UID.
    #[error("the pool token file is owned by uid {owner_uid}; expected {expected_uid}")]
    TokenFileOwner {
        /// File owner UID.
        owner_uid: u32,
        /// Expected effective UID.
        expected_uid: u32,
    },
    /// `--token-file` exceeds [`MAX_TOKEN_FILE_BYTES`].
    #[error("the pool token file exceeds {max_bytes} bytes")]
    TokenFileTooLarge {
        /// Maximum permitted size in bytes.
        max_bytes: usize,
    },
    /// The token file contents are empty, non-UTF-8, or contain non-graphic ASCII bytes.
    #[error("the pool token file does not hold a valid bearer token")]
    TokenInvalid,
    /// `--cortex-url` failed clean base URL validation.
    #[error("invalid cortex base URL: {0}")]
    InvalidUrl(String),
    /// An HTTP call to Cortex failed.
    #[error("cortex pool HTTP call failed: {message}")]
    Transport {
        /// HTTP status code when Cortex returned a non-2xx response.
        status: Option<u16>,
        /// Whether a retry could succeed (`429`, `5xx`, or network failure).
        retryable: bool,
        /// Redacted diagnostic message without bearer credentials.
        message: String,
    },
    /// A JSON payload failed to serialize or decode.
    #[error("cortex pool JSON decode failed: {0}")]
    Decode(String),
    /// Opening the local SQLite runtime or seat substrate failed.
    #[error("pool turn runtime failed: {0}")]
    Runtime(String),
}

impl PoolError {
    /// Returns `true` only for startup, configuration, or authentication (`401`/`403`) errors.
    const fn is_fatal(&self) -> bool {
        match self {
            Self::Transport {
                status: Some(401 | 403),
                ..
            }
            | Self::TokenFileStat(_)
            | Self::TokenFileNotRegular
            | Self::TokenFilePermissions { .. }
            | Self::TokenFileOwner { .. }
            | Self::TokenFileTooLarge { .. }
            | Self::TokenInvalid
            | Self::InvalidUrl(_)
            | Self::Runtime(_) => true,
            Self::Transport { .. } | Self::Decode(_) => false,
        }
    }
}

/// Redacted bearer token read from a `0600` regular file (`REQ-LG-POOL-01`).
#[derive(Clone, PartialEq, Eq)]
pub struct PoolToken(String);

impl fmt::Debug for PoolToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PoolToken([REDACTED])")
    }
}

impl PoolToken {
    /// Loads a bearer token from `path` for the process effective UID (`0600`, regular file).
    ///
    /// # Errors
    ///
    /// Returns [`PoolError`] when `path` fails `O_NOFOLLOW` open, file-type, mode, effective UID,
    /// size (`<= 4 KiB`), or token-byte checks.
    pub fn load(path: &Path) -> Result<Self, PoolError> {
        Self::load_for_uid(path, rustix::process::geteuid().as_raw())
    }

    /// Loads a bearer token from `path`, checking ownership against `expected_uid`.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError`] when `path` fails `O_NOFOLLOW` open, file-type, mode, owner, size,
    /// or token-byte checks.
    pub fn load_for_uid(path: &Path, expected_uid: u32) -> Result<Self, PoolError> {
        let stat_err = |e: io::Error| PoolError::TokenFileStat(e.kind().to_string());
        let fd = match open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::LOOP | Errno::ISDIR) => return Err(PoolError::TokenFileNotRegular),
            Err(err) => return Err(stat_err(io::Error::from(err))),
        };
        let file = std::fs::File::from(fd);
        let meta = file.metadata().map_err(stat_err)?;
        if !meta.file_type().is_file() {
            return Err(PoolError::TokenFileNotRegular);
        }
        let mode = meta.mode() & 0o7777;
        if mode != 0o600 {
            return Err(PoolError::TokenFilePermissions { mode });
        }
        if meta.uid() != expected_uid {
            return Err(PoolError::TokenFileOwner {
                owner_uid: meta.uid(),
                expected_uid,
            });
        }
        let max_u64 = u64::try_from(MAX_TOKEN_FILE_BYTES).unwrap_or(u64::MAX - 1);
        if meta.len() > max_u64 {
            return Err(PoolError::TokenFileTooLarge {
                max_bytes: MAX_TOKEN_FILE_BYTES,
            });
        }
        let mut buf = Vec::with_capacity(256);
        (&file)
            .take(max_u64.saturating_add(1))
            .read_to_end(&mut buf)
            .map_err(stat_err)?;
        if buf.len() > MAX_TOKEN_FILE_BYTES {
            return Err(PoolError::TokenFileTooLarge {
                max_bytes: MAX_TOKEN_FILE_BYTES,
            });
        }
        let raw = std::str::from_utf8(&buf).map_err(|_| PoolError::TokenInvalid)?;
        let trimmed = raw.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(PoolError::TokenInvalid);
        }
        Ok(Self(trimmed.to_owned()))
    }
}

/// Validated configuration for [`PoolClient`] and [`PoolWorker`].
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Base URL of the Cortex API server.
    pub cortex_url: String,
    /// Redacted bearer token for Cortex pool and send calls.
    pub token: PoolToken,
    /// Registered Cortex peer UUID for this pool worker.
    pub peer_id: PoolPeerId,
    /// Registered Cortex member role (`service-pool-littleman-<n>`).
    pub role: RoleName,
    /// Required `from_session` role on inbound deliveries.
    pub allowed_sender: RoleName,
    /// Long-poll wait duration in milliseconds for `/v1/mesh/pool/recv`.
    pub wait_ms: u32,
    /// Redelivery backoff in milliseconds for `/v1/mesh/pool/nack`.
    pub nack_delay_ms: u32,
    /// Shell identifier reported on turn outputs.
    pub shell_id: ShellId,
}

#[derive(Debug, Deserialize)]
struct RawDelivery {
    delivery_token: String,
    stream_seq: u64,
    lease_ms: u32,
    delivery_attempt: u32,
    message: RawLaneMessage,
}

#[derive(Debug, Default, Deserialize)]
struct RawLaneMessage {
    #[serde(default)]
    from_session: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    refs: Option<Value>,
}

enum RecvItem {
    Valid(RawDelivery),
    Malformed {
        raw_token: Option<String>,
        reason: String,
    },
}

/// HTTP client for Cortex `/v1/mesh/pool/{recv,ack,extend,nack,term}` and `/v1/mesh/send`.
#[derive(Debug, Clone)]
pub struct PoolClient {
    transport: Arc<dyn HttpTransport>,
    cortex_url: String,
    token: PoolToken,
}

impl PoolClient {
    /// Builds a [`PoolClient`] backed by [`ReqwestHttpTransport`].
    ///
    /// # Errors
    ///
    /// Returns [`PoolError`] when `config.cortex_url` is malformed or Reqwest cannot start.
    pub fn new(config: &PoolConfig) -> Result<Self, PoolError> {
        let base = parse_clean_base_url(&config.cortex_url, "cortex_url")
            .map_err(|e| PoolError::InvalidUrl(e.to_string()))?;
        let transport = ReqwestHttpTransport::new().map_err(map_adapter_err)?;
        Ok(Self {
            transport: Arc::new(transport),
            cortex_url: base.as_str().to_owned(),
            token: config.token.clone(),
        })
    }

    async fn post<T: Serialize + Sync>(
        &self,
        segments: &[&str],
        wait_ms: Option<u32>,
        payload: &T,
    ) -> Result<StoreHttpResponse, PoolError> {
        let base = parse_clean_base_url(&self.cortex_url, "cortex_url")
            .map_err(|e| PoolError::InvalidUrl(e.to_string()))?;
        let mut url =
            join_url_segments(&base, segments).map_err(|e| PoolError::InvalidUrl(e.to_string()))?;
        if let Some(ms) = wait_ms {
            url.query_pairs_mut()
                .append_pair("wait_ms", &ms.to_string());
        }
        let timeout = Duration::from_millis(u64::from(wait_ms.unwrap_or(5_000)) + 5_000);
        let req = StoreHttpRequest::new(HttpMethod::Post, url.clone())
            .with_timeout(timeout)
            .with_header("Authorization", format!("Bearer {}", self.token.0))
            .with_json_body(payload)
            .map_err(|e| PoolError::Decode(e.to_string()))?;
        self.transport
            .send(req)
            .await
            .and_then(|r| r.ensure_success(&url))
            .map_err(map_adapter_err)
    }

    async fn recv(&self, peer_id: &PoolPeerId, wait_ms: u32) -> Result<Vec<RecvItem>, PoolError> {
        let body = json!({ "peer_id": peer_id.as_str(), "free_slots": 1 });
        let resp = self
            .post(&["v1", "mesh", "pool", "recv"], Some(wait_ms), &body)
            .await?;
        let Ok(Value::Array(items)) = serde_json::from_slice::<Value>(&resp.body) else {
            tracing::warn!(
                body_bytes = resp.body.len(),
                "malformed pool recv payload; skipping cycle"
            );
            return Ok(Vec::new());
        };
        Ok(items
            .into_iter()
            .map(|item| {
                let tok = item
                    .get("delivery_token")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                serde_json::from_value::<RawDelivery>(item).map_or_else(
                    |e| RecvItem::Malformed {
                        raw_token: tok,
                        reason: format!(
                            "malformed delivery: {}",
                            truncate_chars(&e.to_string(), 200)
                        ),
                    },
                    RecvItem::Valid,
                )
            })
            .collect())
    }

    async fn settle(&self, action: &str, body: &Value) -> Result<(), PoolError> {
        self.post(&["v1", "mesh", "pool", action], None, body)
            .await
            .map(|_| ())
    }

    async fn send_reply(
        &self,
        cfg: &PoolConfig,
        subject: &str,
        body: &str,
        refs: &Map<String, Value>,
    ) -> Result<(), PoolError> {
        let peer = cfg.peer_id.as_str();
        let msg = json!({
            "kind": "msg", "from_peer": peer, "from_session": cfg.role.as_str(),
            "subject": subject, "body": body, "refs": refs,
        });
        let env = json!({ "peer_id": peer, "to": { "role": cfg.allowed_sender.as_str() }, "message": msg });
        self.post(&["v1", "mesh", "send"], None, &env)
            .await
            .map(|_| ())
    }
}

fn map_adapter_err(err: AdapterError) -> PoolError {
    match err {
        AdapterError::HttpStatus { status, url, body } => PoolError::Transport {
            status: Some(status),
            retryable: status == 429 || status >= 500,
            message: format!("HTTP {status} from {url}: {body}"),
        },
        AdapterError::Transport { message } => PoolError::Transport {
            status: None,
            retryable: true,
            message,
        },
        other => PoolError::Decode(other.to_string()),
    }
}

/// Validated input passed to [`TurnRunner::run_turn`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnInput {
    /// Pool task identifier.
    pub task_id: TaskId,
    /// 1-based task attempt number.
    pub attempt: u32,
    /// Worker shell identifier.
    pub shell_id: ShellId,
    /// Prompt text extracted from the delivery.
    pub prompt: String,
    /// Validated GHOST binding when the delivery carries a non-null binding.
    pub ghost_binding: Option<GhostBinding>,
}

/// Completed turn output returned by [`TurnRunner::run_turn`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutput {
    /// Assistant answer text.
    pub answer: String,
    /// 64-character lowercase hex SHA-256 digest of the turn transcript.
    pub transcript_sha256: String,
    /// Completed seat receipt from `LittleguySeatSubstrate`.
    pub receipt: SeatReceipt,
}

/// Failure classification returned by [`TurnRunner::run_turn`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TurnError {
    /// Temporary failure: [`PoolWorker`] calls `nack` with delay so Cortex redelivers.
    #[error("transient turn failure: {0}")]
    Transient(String),
    /// Non-retryable turn failure: [`PoolWorker`] reports `outcome: failed` and `ack`s.
    #[error("permanent turn failure: {0}")]
    Permanent(String),
}

/// Executes one bounded turn per pool delivery (`REQ-LG-POOL-05`).
pub trait TurnRunner: Send + Sync {
    /// Runs one turn for `input`.
    fn run_turn<'a>(&'a self, input: &'a TurnInput)
    -> BoxFuture<'a, Result<TurnOutput, TurnError>>;
}

impl<T: TurnRunner + ?Sized> TurnRunner for &T {
    fn run_turn<'a>(
        &'a self,
        input: &'a TurnInput,
    ) -> BoxFuture<'a, Result<TurnOutput, TurnError>> {
        (*self).run_turn(input)
    }
}

struct RuntimeState {
    rt: SqliteRuntime,
    agent_id: AgentId,
    substrate: LittleguySeatSubstrate,
    owner: String,
    work_dir: PathBuf,
}

/// Production [`TurnRunner`] backed by `LittleguySeatSubstrate` and single-writer `SqliteRuntime`.
pub struct RuntimeTurnRunner {
    shell_id: ShellId,
    state: Arc<tokio::sync::Mutex<RuntimeState>>,
}

impl fmt::Debug for RuntimeTurnRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeTurnRunner")
            .field("shell_id", &self.shell_id)
            .finish_non_exhaustive()
    }
}

impl RuntimeTurnRunner {
    /// Opens the single-writer SQLite runtime at `db` and prepares the seat substrate.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::Runtime`] when `db` cannot be opened or `config.owner` is invalid.
    pub fn open(
        db: &Path,
        config: &Config,
        model: Arc<dyn AgentModel>,
        shell_id: ShellId,
    ) -> Result<Self, PoolError> {
        let parent = db.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(dir) = parent {
            std::fs::create_dir_all(dir).map_err(|e| PoolError::Runtime(e.to_string()))?;
        }
        let mut rt = SqliteRuntime::open(db).map_err(|e| PoolError::Runtime(e.to_string()))?;
        let profile = AgentProfile::new(ASSISTANT_KIND)
            .system(config.system.as_str())
            .window(config.window);
        sqlite::register(&mut rt, Arc::new(AgentHarness::new(model).agent(profile)));
        let agent_id = AgentId::new(ASSISTANT_KIND, config.owner.as_str())
            .map_err(|e| PoolError::Runtime(e.to_string()))?;
        let work_dir = parent.unwrap_or_else(|| Path::new(".")).to_path_buf();
        let substrate = LittleguySeatSubstrate::new(WheelhorseGate::new());
        let owner = config.owner.as_str().to_owned();
        Ok(Self {
            shell_id,
            state: Arc::new(tokio::sync::Mutex::new(RuntimeState {
                rt,
                agent_id,
                substrate,
                owner,
                work_dir,
            })),
        })
    }
}

impl TurnRunner for RuntimeTurnRunner {
    fn run_turn<'a>(
        &'a self,
        input: &'a TurnInput,
    ) -> BoxFuture<'a, Result<TurnOutput, TurnError>> {
        Box::pin(async move {
            if tokio::runtime::Handle::current().runtime_flavor()
                == tokio::runtime::RuntimeFlavor::CurrentThread
            {
                let (state, owned) = (Arc::clone(&self.state), input.clone());
                return tokio::task::spawn_blocking(move || {
                    tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .build()
                        .map_err(|e| TurnError::Transient(e.to_string()))?
                        .block_on(async move { run_turn_locked(&state, &owned).await })
                })
                .await
                .map_err(|e| TurnError::Transient(e.to_string()))?;
            }
            run_turn_locked(&self.state, input).await
        })
    }
}

async fn run_turn_locked(
    mutex: &tokio::sync::Mutex<RuntimeState>,
    input: &TurnInput,
) -> Result<TurnOutput, TurnError> {
    let mut st = mutex.lock().await;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX / 2));
    let job_id = format!("{}:{}", input.task_id, input.attempt);
    let seat = prepare_seat_job(&mut st, input, &job_id, now_ms)?;
    let answer = match drive_sqlite_turn(&mut st, input, &job_id).await {
        Ok(ans) => ans,
        Err(err) => {
            let _ = st.substrate.cancel(&seat, &job_id);
            let _ = st.substrate.release(&seat);
            drop(st);
            return Err(err);
        }
    };
    let transcript = json!({
        "task_id": input.task_id.as_str(), "attempt": input.attempt,
        "shell_id": input.shell_id.as_str(), "prompt": &input.prompt, "answer": &answer,
    });
    let sha = sha256_hex(transcript.to_string().as_bytes());
    let mut receipt = st
        .substrate
        .complete_job(&seat, &job_id, "turn_completed", Some(sha.clone()))
        .map_err(|e| TurnError::Permanent(e.to_string()))?;
    receipt.ghost_binding.clone_from(&input.ghost_binding);
    st.substrate
        .release(&seat)
        .map_err(|e| TurnError::Transient(e.to_string()))?;
    drop(st);
    Ok(TurnOutput {
        answer,
        transcript_sha256: sha,
        receipt,
    })
}

fn prepare_seat_job(
    st: &mut RuntimeState,
    input: &TurnInput,
    job_id: &str,
    now_ms: u64,
) -> Result<littleguy_core::SeatId, TurnError> {
    let spiffe_id = input
        .ghost_binding
        .as_ref()
        .and_then(GhostBinding::spiffe_seat)
        .unwrap_or("spiffe://unsigned.gg/agent/littleman/pool-worker")
        .to_owned();
    let principal = input
        .ghost_binding
        .as_ref()
        .map_or_else(|| st.owner.clone(), |b| b.mitc_principal().to_owned());
    let claims = BiscuitMandateClaims {
        spiffe_id,
        acting_for_principal: principal,
        min_sandbox_class: SandboxClass::Process,
        allowed_repo_prefixes: Vec::new(),
        max_reversibility: ReversibilityClass::Compensatable,
        expires_at_ms: now_ms.saturating_add(86_400_000),
    };
    let grant = AuthorityGrant::pool_worker_default(
        "LittlemanPool",
        Vec::new(),
        vec!["cortex-pool".into()],
    );
    st.substrate
        .register_slot(
            "littleman-0",
            st.work_dir.clone(),
            EstateSubject::AgentFabric,
            SandboxClass::Process,
            claims,
            grant,
            now_ms,
        )
        .map_err(|e| TurnError::Permanent(e.to_string()))?;
    let seat = st
        .substrate
        .ensure_seat("littleman-0", &st.work_dir)
        .map_err(|e| TurnError::Transient(e.to_string()))?;
    let job = SeatJobSpec {
        job_id: job_id.to_owned(),
        body: input.prompt.clone(),
        target_repo: None,
        subject: EstateSubject::AgentFabric,
        allowed_paths: Vec::new(),
        check_ids: Vec::new(),
        compensation_plan: None,
    };
    st.substrate
        .submit(&seat, &job, now_ms)
        .map_err(|e| TurnError::Permanent(e.to_string()))?;
    Ok(seat)
}

async fn drive_sqlite_turn(
    st: &mut RuntimeState,
    input: &TurnInput,
    job_id: &str,
) -> Result<String, TurnError> {
    let tr = |e: autumn_harvest_sqlite::SqliteError| TurnError::Transient(e.to_string());
    let failures_before = sqlite::state(&st.rt, &st.agent_id)
        .await
        .map_err(tr)?
        .map_or(0, |s| s.failures);
    let key = format!("pool:{job_id}");
    if sqlite::send(
        &mut st.rt,
        &st.agent_id,
        &AgentOp::message_with_id(&input.prompt, &key),
        Some(&key),
    )
    .map_err(tr)?
    .signal_delivered
    {
        sqlite::drive(&mut st.rt, &st.agent_id).await.map_err(tr)?;
    }
    let after = sqlite::state(&st.rt, &st.agent_id)
        .await
        .map_err(tr)?
        .ok_or_else(|| TurnError::Transient("the assistant produced no state".into()))?;
    if after.failures > failures_before {
        return Err(TurnError::Transient(
            after.last_error.unwrap_or_else(|| "turn failed".into()),
        ));
    }
    after
        .last_answer
        .ok_or_else(|| TurnError::Transient("turn ended without answer".into()))
}

/// Outcome of one [`PoolWorker::step`] poll cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// `/v1/mesh/pool/recv` returned no usable deliveries (or skipped an untokened poison entry).
    Idle,
    /// Valid delivery executed one turn, sent its reply, and called `ack`.
    Settled,
    /// Redelivery matched the `(TaskId, attempt)` cache, re-sent the reply, and called `ack`.
    ReplayedFromCache,
    /// Unauthorized, malformed, or non-retryably rejected delivery was settled with `term`.
    Termed,
    /// Transient turn or send failure called `nack` with backoff delay.
    Nacked,
}

#[derive(Clone)]
struct CachedReply {
    outcome: &'static str,
    body: String,
    refs: Map<String, Value>,
}

/// Bounded `littleman` pool consumer worker (`REQ-LG-POOL-02..07`).
pub struct PoolWorker<R: TurnRunner> {
    config: PoolConfig,
    client: PoolClient,
    runner: R,
    cache: BTreeMap<(TaskId, u32), CachedReply>,
    cache_order: VecDeque<(TaskId, u32)>,
}

impl<R: TurnRunner> fmt::Debug for PoolWorker<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolWorker")
            .field("peer_id", &self.config.peer_id)
            .finish_non_exhaustive()
    }
}

impl<R: TurnRunner> PoolWorker<R> {
    /// Creates a new [`PoolWorker`] with an empty idempotency cache.
    #[must_use]
    pub const fn new(config: PoolConfig, client: PoolClient, runner: R) -> Self {
        Self {
            config,
            client,
            runner,
            cache: BTreeMap::new(),
            cache_order: VecDeque::new(),
        }
    }

    /// Polls Cortex once (`free_slots: 1`) and processes any returned delivery.
    ///
    /// Per-delivery poison tokens, malformed payloads, and non-auth HTTP errors are
    /// logged and settled (or skipped so the lease expires) without stopping the worker.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError`] when `/v1/mesh/pool/recv` fails or Cortex returns `401`/`403`.
    pub async fn step(&mut self) -> Result<StepOutcome, PoolError> {
        let batch = self
            .client
            .recv(&self.config.peer_id, self.config.wait_ms)
            .await?;
        let Some(item) = batch.into_iter().next() else {
            return Ok(StepOutcome::Idle);
        };
        let raw = match item {
            RecvItem::Valid(raw) => raw,
            RecvItem::Malformed { raw_token, reason } => {
                return self.settle_poison(raw_token.as_deref(), &reason).await;
            }
        };
        let Ok(token) = DeliveryToken::new(raw.delivery_token.as_str()) else {
            return self
                .settle_poison(
                    Some(raw.delivery_token.as_str()),
                    "refused: invalid delivery_token",
                )
                .await;
        };
        let input = match validate_delivery(&raw, &self.config) {
            Ok(valid) => valid,
            Err(reason) => return self.term_token(token.as_str(), &reason).await,
        };
        let key = (input.task_id.clone(), input.attempt);
        if let Some(mut hit) = self.cache.get(&key).cloned() {
            hit.refs.insert("stream_seq".into(), json!(raw.stream_seq));
            return self
                .send_and_settle(&key, &token, &hit, StepOutcome::ReplayedFromCache)
                .await;
        }
        match self.run_with_extend(&token, raw.lease_ms, &input).await {
            Ok(out) => {
                let reply = build_ok_reply(&input, &out, raw.stream_seq);
                self.put_cache(key.clone(), reply.clone());
                self.send_and_settle(&key, &token, &reply, StepOutcome::Settled)
                    .await
            }
            Err(TurnError::Transient(why)) => {
                tracing::warn!(
                    task_id = %input.task_id,
                    reason = %truncate_chars(&why, 256),
                    "transient turn failure; nacking"
                );
                self.nack(&token).await
            }
            Err(TurnError::Permanent(why)) => {
                let reply = build_failed_reply(&input, raw.stream_seq, &why);
                self.put_cache(key.clone(), reply.clone());
                self.send_and_settle(&key, &token, &reply, StepOutcome::Settled)
                    .await
            }
        }
    }

    /// Runs [`Self::step`] in a loop until `stop` resolves.
    ///
    /// # Errors
    ///
    /// Propagates startup, configuration, or authentication (`401`/`403`) [`PoolError`] failures.
    pub async fn run_until(
        &mut self,
        stop: impl Future<Output = ()> + Send,
    ) -> Result<(), PoolError> {
        tokio::pin!(stop);
        loop {
            tokio::select! {
                () = &mut stop => return Ok(()),
                res = self.step() => if let Err(err) = res {
                    if err.is_fatal() {
                        return Err(err);
                    }
                    tracing::warn!(
                        reason = %truncate_chars(&err.to_string(), 256),
                        "pool poll error; backing off"
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    async fn best_effort_settle(&self, action: &str, body: &Value) -> Result<bool, PoolError> {
        match self.client.settle(action, body).await {
            Ok(()) => Ok(true),
            Err(err) if err.is_fatal() => Err(err),
            Err(err) => {
                tracing::warn!(
                    action,
                    reason = %truncate_chars(&err.to_string(), 256),
                    "pool lease settlement failed; continuing"
                );
                Ok(false)
            }
        }
    }

    async fn term_token(&self, token_str: &str, reason: &str) -> Result<StepOutcome, PoolError> {
        let body = json!({
            "peer_id": self.config.peer_id.as_str(),
            "delivery_token": token_str,
            "reason": truncate_chars(reason, 256),
        });
        let _ = self.best_effort_settle("term", &body).await?;
        Ok(StepOutcome::Termed)
    }

    async fn settle_poison(
        &self,
        raw_token: Option<&str>,
        reason: &str,
    ) -> Result<StepOutcome, PoolError> {
        tracing::warn!(
            token_len = raw_token.map_or(0, str::len),
            reason = %truncate_chars(reason, 256),
            "poison pool delivery; settling or letting lease expire"
        );
        if let Some(tok) = raw_token.filter(|t| {
            !t.is_empty()
                && t.len() <= MAX_RAW_SETTLE_TOKEN_BYTES
                && !t.bytes().any(|b| b.is_ascii_control())
        }) {
            return self.term_token(tok, reason).await;
        }
        Ok(StepOutcome::Idle)
    }

    async fn run_with_extend(
        &self,
        token: &DeliveryToken,
        lease_ms: u32,
        input: &TurnInput,
    ) -> Result<TurnOutput, TurnError> {
        let mut ticker =
            tokio::time::interval(Duration::from_millis(u64::from((lease_ms / 3).max(25))));
        ticker.tick().await;
        let ext =
            json!({ "peer_id": self.config.peer_id.as_str(), "delivery_token": token.as_str() });
        let fut = self.runner.run_turn(input);
        tokio::pin!(fut);
        loop {
            tokio::select! {
                out = &mut fut => return out,
                _ = ticker.tick() => { let _ = self.client.settle("extend", &ext).await; }
            }
        }
    }

    fn put_cache(&mut self, key: (TaskId, u32), reply: CachedReply) {
        if !self.cache.contains_key(&key) {
            if self.cache_order.len() >= MAX_CACHED_RESULTS
                && let Some(oldest) = self.cache_order.pop_front()
            {
                self.cache.remove(&oldest);
            }
            self.cache_order.push_back(key.clone());
        }
        self.cache.insert(key, reply);
    }

    fn remove_cache(&mut self, key: &(TaskId, u32)) {
        if self.cache.remove(key).is_some() {
            self.cache_order.retain(|k| k != key);
        }
    }

    async fn nack(&self, token: &DeliveryToken) -> Result<StepOutcome, PoolError> {
        let body = json!({
            "peer_id": self.config.peer_id.as_str(),
            "delivery_token": token.as_str(),
            "delay_ms": self.config.nack_delay_ms,
        });
        let _ = self.best_effort_settle("nack", &body).await?;
        Ok(StepOutcome::Nacked)
    }

    async fn send_and_settle(
        &mut self,
        key: &(TaskId, u32),
        token: &DeliveryToken,
        reply: &CachedReply,
        success: StepOutcome,
    ) -> Result<StepOutcome, PoolError> {
        let task_id = &key.0;
        let subject = format!("result {task_id} {}", reply.outcome);
        match self
            .client
            .send_reply(&self.config, &subject, &reply.body, &reply.refs)
            .await
        {
            Ok(()) => {
                let ack = json!({
                    "peer_id": self.config.peer_id.as_str(),
                    "delivery_tokens": [token.as_str()],
                });
                let _ = self.best_effort_settle("ack", &ack).await?;
                Ok(success)
            }
            Err(err) if err.is_fatal() => Err(err),
            Err(PoolError::Transport {
                retryable: true,
                message: msg,
                ..
            }) => {
                tracing::warn!(
                    %task_id,
                    reason = %truncate_chars(&msg, 256),
                    "transient reply send failure; keeping cached reply and nacking"
                );
                self.nack(token).await
            }
            Err(err) => {
                let reason = format!("send rejected: {}", truncate_chars(&err.to_string(), 200));
                tracing::warn!(%task_id, %reason, "non-retryable reply send failure; terming");
                self.remove_cache(key);
                self.term_token(token.as_str(), &reason).await
            }
        }
    }
}

fn validate_delivery(raw: &RawDelivery, config: &PoolConfig) -> Result<TurnInput, String> {
    let sender = raw.message.from_session.as_deref().unwrap_or_default();
    if sender != config.allowed_sender.as_str() {
        return Err(format!("refused: sender `{sender}` is not allowed"));
    }
    if raw.message.subject.as_deref().unwrap_or_default().len() > MAX_DELIVERY_SUBJECT_BYTES {
        return Err("refused: subject exceeds size cap".into());
    }
    let body = raw.message.body.as_deref().unwrap_or_default();
    if body.len() > MAX_DELIVERY_BODY_BYTES {
        return Err("refused: body exceeds 12 KiB size cap".into());
    }
    let refs_val = raw.message.refs.as_ref().unwrap_or(&Value::Null);
    if serde_json::to_vec(refs_val)
        .map_err(|e| format!("refused: invalid refs: {e}"))?
        .len()
        > MAX_DELIVERY_REFS_BYTES
    {
        return Err("refused: refs exceed 16 KiB size cap".into());
    }
    let (prompt, body_ghost, body_task) = parse_body_envelope(body)?;
    let refs_ghost = parse_optional_ghost(refs_val.get("ghost_binding"))?;
    if let (Some(a), Some(b)) = (&body_ghost, &refs_ghost)
        && a != b
    {
        return Err("refused: conflicting ghost_binding in body and refs".into());
    }
    let raw_task = refs_val
        .get("task_id")
        .and_then(Value::as_str)
        .or(body_task.as_deref())
        .ok_or("refused: missing task_id")?;
    let task_id = TaskId::new(raw_task).map_err(|e| format!("refused: invalid task_id: {e}"))?;
    let attempt = parse_attempt(refs_val.get("attempt"), raw.delivery_attempt)?;
    Ok(TurnInput {
        task_id,
        attempt,
        shell_id: config.shell_id.clone(),
        prompt,
        ghost_binding: body_ghost.or(refs_ghost),
    })
}

fn parse_body_envelope(
    body: &str,
) -> Result<(String, Option<GhostBinding>, Option<String>), String> {
    if body.trim_start().starts_with('{') {
        let map: Map<String, Value> =
            serde_json::from_str(body).map_err(|e| format!("refused: malformed JSON body: {e}"))?;
        let prompt = map
            .get("prompt")
            .or_else(|| map.get("task"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or("refused: missing prompt")?
            .to_owned();
        let ghost = parse_optional_ghost(map.get("ghost_binding"))?;
        return Ok((
            prompt,
            ghost,
            map.get("task_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ));
    }
    if body.trim().is_empty() {
        return Err("refused: empty prompt".into());
    }
    Ok((body.to_owned(), None, None))
}

fn parse_optional_ghost(val: Option<&Value>) -> Result<Option<GhostBinding>, String> {
    match val {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value::<GhostBinding>(v.clone())
            .map(Some)
            .map_err(|e| format!("refused: invalid ghost_binding: {e}")),
    }
}

fn parse_attempt(val: Option<&Value>, fallback: u32) -> Result<u32, String> {
    let Some(v) = val.filter(|v| !v.is_null()) else {
        return Ok(fallback.max(1));
    };
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| v.as_str().and_then(|s| s.parse::<u32>().ok()))
        .filter(|&n| n >= 1)
        .ok_or_else(|| "refused: invalid attempt".into())
}

fn truncate_chars(text: &str, max_chars: usize) -> &str {
    text.char_indices()
        .nth(max_chars)
        .map_or(text, |(idx, _)| &text[..idx])
}

fn base_refs(input: &TurnInput, stream_seq: u64, outcome: &str) -> Map<String, Value> {
    let mut refs = Map::new();
    refs.insert("task_id".into(), json!(input.task_id.as_str())); // escapement-serve/src/pool.rs:960
    refs.insert("attempt".into(), json!(input.attempt)); // escapement-serve/src/pool.rs:978
    refs.insert("stream_seq".into(), json!(stream_seq)); // escapement-serve/src/pool.rs:987
    refs.insert("outcome".into(), json!(outcome)); // escapement-serve/src/pool.rs:996
    if let Some(b) = &input.ghost_binding {
        refs.insert("ghost_id".into(), json!(b.ghost_id()));
        refs.insert("molt_epoch".into(), json!(b.molt_epoch()));
    }
    refs
}

fn build_ok_reply(input: &TurnInput, out: &TurnOutput, stream_seq: u64) -> CachedReply {
    let mut refs = base_refs(input, stream_seq, "ok"); // escapement-serve/src/pool.rs:999
    refs.insert("transcript_sha256".into(), json!(&out.transcript_sha256));
    let make = |ans: &str| {
        let mut obj = Map::new();
        obj.insert("answer".into(), json!(ans));
        obj.insert("shell_id".into(), json!(input.shell_id.as_str()));
        obj.insert("transcript_sha256".into(), json!(&out.transcript_sha256));
        if let Some(b) = &input.ghost_binding {
            obj.insert("ghost_id".into(), json!(b.ghost_id()));
            obj.insert("molt_epoch".into(), json!(b.molt_epoch()));
        }
        Value::Object(obj).to_string()
    };
    let mut body = make(&out.answer);
    if body.len() > MAX_REPLY_BODY_BYTES {
        let mut cap = out
            .answer
            .len()
            .min(MAX_REPLY_BODY_BYTES.saturating_sub(512));
        loop {
            while !out.answer.is_char_boundary(cap) {
                cap = cap.saturating_sub(1);
            }
            body = make(&out.answer[..cap]);
            if body.len() <= MAX_REPLY_BODY_BYTES || cap == 0 {
                break;
            }
            cap = cap.saturating_sub(128);
        }
    }
    CachedReply {
        outcome: "ok",
        body,
        refs,
    }
}

fn build_failed_reply(input: &TurnInput, stream_seq: u64, error: &str) -> CachedReply {
    let short_err = truncate_chars(error, MAX_ERROR_CHARS);
    let mut refs = base_refs(input, stream_seq, "failed"); // escapement-serve/src/pool.rs:1021
    refs.insert("transient".into(), json!(false)); // escapement-serve/src/pool.rs:1033
    refs.insert("error".into(), json!(short_err)); // escapement-serve/src/pool.rs:1040
    let body = json!({ "error": short_err, "shell_id": input.shell_id.as_str() }).to_string();
    CachedReply {
        outcome: "failed",
        body,
        refs,
    }
}
