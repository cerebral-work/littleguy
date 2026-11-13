//! Cortex `littleman` pool consumer (`specs/0005`, `REQ-LG-POOL-01..07`).
//!
//! Pulls bounded tasks from `POST /v1/mesh/pool/recv`, checks sender identity,
//! payload bounds, and any `GhostBinding`, runs one turn per `(task_id, attempt)`
//! behind [`TurnRunner`], posts the reply to `POST /v1/mesh/send`, and settles
//! the lease with `ack`, `nack`, or `term`.

use std::fmt;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use autumn_harvest_agent::AgentModel;
use autumn_harvest_agent::model::BoxFuture;
use littleguy_core::{Config, GhostBinding, PoolPeerId, RoleName, SeatReceipt, ShellId, TaskId};
use littleguy_stores::{HttpTransport, ReqwestHttpTransport, parse_clean_base_url};
use thiserror::Error;

/// Default allowed sender role on Cortex (`escapement-serve` pool publisher).
pub const DEFAULT_ALLOWED_SENDER: &str = "service-escapement-pool";

/// Default Cortex role reported by this worker (`escapement-serve/src/pool.rs:182`).
pub const DEFAULT_MEMBER_ROLE: &str = "service-pool-littleman-1";

/// Default peer UUID when `--peer-id` is omitted.
pub const DEFAULT_PEER_ID: &str = "00000000-0000-0000-0000-000000000001";

/// Default long-poll wait in milliseconds for `/v1/mesh/pool/recv`.
pub const DEFAULT_WAIT_MS: u32 = 5_000;

/// Default redelivery delay in milliseconds for `/v1/mesh/pool/nack`.
pub const DEFAULT_NACK_DELAY_MS: u32 = 5_000;

/// Maximum inbound delivery body bytes (`escapement-serve/src/pool.rs:44` `POOL_BODY_MAX`).
pub const MAX_DELIVERY_BODY_BYTES: usize = 12 * 1024;

/// Maximum inbound delivery subject bytes (`escapement-serve/src/pool.rs:48` `POOL_SUBJECT_MAX`).
pub const MAX_DELIVERY_SUBJECT_BYTES: usize = 512;

/// Maximum serialized `refs` bytes (`escapement-serve/src/pool.rs:50` `POOL_REFS_MAX`).
pub const MAX_DELIVERY_REFS_BYTES: usize = 16 * 1024;

/// Maximum reply body bytes (`escapement-serve/src/pool.rs:1005` `MAX_RESULT_BYTES`).
pub const MAX_REPLY_BODY_BYTES: usize = 8 * 1024;

/// Maximum `(TaskId, attempt)` entries held in the worker idempotency cache.
pub const MAX_CACHED_RESULTS: usize = 256;

/// Errors returned while configuring or running `lg pool`.
#[derive(Debug, Error)]
pub enum PoolError {
    /// Metadata lookup or file read failed on `--token-file`.
    #[error("failed to read the pool token file: {0}")]
    TokenFileStat(String),
    /// `--token-file` is a symlink, directory, FIFO, or socket instead of a regular file.
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
        /// Current process UID.
        expected_uid: u32,
    },
    /// The token file contents are empty or contain non-graphic ASCII bytes.
    #[error("the pool token file does not hold a valid bearer token")]
    TokenInvalid,
    /// `--cortex-url` failed clean base URL validation.
    #[error("invalid cortex base URL: {0}")]
    InvalidUrl(String),
    /// An HTTP call to Cortex failed.
    #[error("cortex pool HTTP call failed: {message}")]
    Transport {
        /// Whether the failure is retryable (`429`, `5xx`, or network failure).
        retryable: bool,
        /// Redacted diagnostic message without bearer credentials.
        message: String,
    },
    /// A JSON response from Cortex failed to decode.
    #[error("cortex pool JSON decode failed: {0}")]
    Decode(String),
    /// Opening the local SQLite runtime or seat substrate failed.
    #[error("pool turn runtime failed: {0}")]
    Runtime(String),
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
    /// Loads a bearer token from `path`, requiring a regular file owned by the
    /// current user with mode `0600`.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError`] when `path` is missing, not a regular file, has
    /// mode other than `0600`, has another owner, or holds invalid token bytes.
    pub fn load(path: &Path) -> Result<Self, PoolError> {
        Self::load_for_uid(path, rustix::process::getuid().as_raw())
    }

    /// Loads a bearer token from `path`, checking ownership against `expected_uid`.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError`] when `path` fails any file-type, permission, owner,
    /// or content check.
    pub fn load_for_uid(_path: &Path, _expected_uid: u32) -> Result<Self, PoolError> {
        Err(PoolError::TokenFileStat("not yet implemented".to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
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
    /// Returns [`PoolError::InvalidUrl`] when `config.cortex_url` is malformed or
    /// [`PoolError::Transport`] when the HTTP client fails to initialize.
    pub fn new(config: &PoolConfig) -> Result<Self, PoolError> {
        let transport = ReqwestHttpTransport::new().map_err(|err| PoolError::Transport {
            retryable: false,
            message: err.to_string(),
        })?;
        Self::with_transport(config, Arc::new(transport))
    }

    /// Builds a [`PoolClient`] with a caller-supplied [`HttpTransport`].
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::InvalidUrl`] when `config.cortex_url` is not a clean
    /// `http` or `https` base URL.
    pub fn with_transport(
        config: &PoolConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, PoolError> {
        let parsed = parse_clean_base_url(&config.cortex_url, "cortex_url")
            .map_err(|err| PoolError::InvalidUrl(err.to_string()))?;
        Ok(Self {
            transport,
            cortex_url: parsed.as_str().to_owned(),
            token: config.token.clone(),
        })
    }

    /// Borrows the underlying transport.
    #[must_use]
    pub fn transport(&self) -> &dyn HttpTransport {
        self.transport.as_ref()
    }

    /// Configured Cortex base URL.
    #[must_use]
    pub fn cortex_url(&self) -> &str {
        &self.cortex_url
    }

    /// Redacted token reference.
    #[must_use]
    pub const fn token(&self) -> &PoolToken {
        &self.token
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

/// Production [`TurnRunner`] backed by `LittleguySeatSubstrate` and single-writer `SqliteRuntime`.
pub struct RuntimeTurnRunner {
    shell_id: ShellId,
}

impl fmt::Debug for RuntimeTurnRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeTurnRunner")
            .field("shell_id", &self.shell_id)
            .finish_non_exhaustive()
    }
}

impl RuntimeTurnRunner {
    /// Opens the single-writer SQLite runtime at `db` and registers the seat slot.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::Runtime`] when `db` cannot be opened or the seat
    /// slot cannot be registered.
    pub fn open(
        _db: &Path,
        _config: &Config,
        _model: Arc<dyn AgentModel>,
        shell_id: ShellId,
    ) -> Result<Self, PoolError> {
        Ok(Self { shell_id })
    }
}

impl TurnRunner for RuntimeTurnRunner {
    fn run_turn<'a>(
        &'a self,
        _input: &'a TurnInput,
    ) -> BoxFuture<'a, Result<TurnOutput, TurnError>> {
        Box::pin(async { Err(TurnError::Permanent("not yet implemented".to_owned())) })
    }
}

/// Outcome of one [`PoolWorker::step`] poll cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// `/v1/mesh/pool/recv` returned no deliveries.
    Idle,
    /// Valid delivery executed one turn, sent its reply, and called `ack`.
    Settled,
    /// Redelivery matched the `(TaskId, attempt)` cache, re-sent the reply, and called `ack`.
    ReplayedFromCache,
    /// Unauthorized or malformed delivery was refused with `term` and ran no turn.
    Termed,
    /// Transient turn or send failure called `nack` with backoff delay.
    Nacked,
}

/// Bounded `littleman` pool consumer worker (`REQ-LG-POOL-02..07`).
pub struct PoolWorker<R: TurnRunner> {
    config: PoolConfig,
    client: PoolClient,
    runner: R,
}

impl<R: TurnRunner> fmt::Debug for PoolWorker<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolWorker")
            .field("peer_id", &self.config.peer_id)
            .field("role", &self.config.role)
            .field("allowed_sender", &self.config.allowed_sender)
            .field("shell_id", &self.config.shell_id)
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
        }
    }

    /// Borrows the inner [`TurnRunner`].
    #[must_use]
    pub const fn runner(&self) -> &R {
        &self.runner
    }

    /// Borrows the inner [`PoolClient`].
    #[must_use]
    pub const fn client(&self) -> &PoolClient {
        &self.client
    }

    /// Polls Cortex once (`free_slots: 1`) and processes any returned delivery.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError`] when `/v1/mesh/pool/recv` or lease settlement fails.
    pub async fn step(&mut self) -> Result<StepOutcome, PoolError> {
        let _ = self.client.token().as_str();
        tokio::task::yield_now().await;
        Err(PoolError::Runtime("not yet implemented".to_owned()))
    }

    /// Runs [`Self::step`] in a loop until `stop` resolves.
    ///
    /// # Errors
    ///
    /// Propagates fatal [`PoolError`] values when `recv` fails repeatedly.
    pub async fn run_until(
        &mut self,
        stop: impl Future<Output = ()> + Send,
    ) -> Result<(), PoolError> {
        tokio::pin!(stop);
        tokio::select! {
            () = &mut stop => Ok(()),
            res = self.step() => res.map(|_| ()),
        }
    }
}
