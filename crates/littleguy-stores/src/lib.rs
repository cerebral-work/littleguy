//! Durable SQLite stores for the memory and the episode log of littleguy.
//!
//! autumn-harvest-agent 0.10 ships these two stores in process memory only
//! (autumn-harvest#2604). [`SqliteStores`] keeps both in one SQLite file.
//! The daemon puts it beside `lg.db`, so the runtime stays the one writer of
//! `lg.db` (ADR 0001).
//!
//! The rules of each change live in the engine: `memory::apply_to` for a
//! memory edit, and `EpisodeLog` for the episode log. This crate only keeps
//! their results, one transaction per call. So the rules are the same as in
//! every other store.
//!
//! Each edit carries a key, and a key applies once. A memory edit keeps its
//! failure with its key, as the trait asks. A retried tool call then gets the
//! same answer, even when another run changed the scope in between.
//!
//! The file also keeps the key of each report that a delivery sent. The
//! engine delivers a report at least once, so a delivery that checks the
//! key sends it once.
//!
//! The file also keeps each tool call that waits for an approval
//! ([`approvals`]).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use autumn_harvest_agent::episode::{
    Appended, Cleared, Episode, EpisodeLog, EpisodeStore, PassMark,
};
use autumn_harvest_agent::memory::{EditKey, MemoryBlock, MemoryOp, MemoryScope, MemoryStore};
use autumn_harvest_agent::model::BoxFuture;
use autumn_harvest_agent::{AgentError, ErrorKind};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

pub mod approvals;
pub mod reverie;
pub mod transport;
pub mod xerj;

pub use approvals::{ApprovalRecord, Claim, Verdict};
pub use reverie::{
    DEFAULT_REVERIE_URL, DEFAULT_SMART_CONTEXT_LIMIT, MAX_REVERIE_SEARCH_LIMIT,
    MAX_SMART_CONTEXT_LIMIT, ReverieClientConfig, ReverieObservationBuilder, ReverieObservationHit,
    ReverieObservationKind, ReverieObservationPayload, ReverieObservationStore,
    ReverieSavedReceipt, ReverieScope, ReverieSearchQuery, ReverieSmartContext, ReverieTag,
    ReverieWriteMode, validate_topic_key,
};
pub use transport::{
    AdapterError, DEFAULT_HTTP_TIMEOUT, HttpMethod, HttpTransport, MockHttpTransport,
    MockRouteHandler, ReqwestHttpTransport, StoreHttpRequest, StoreHttpResponse,
};
pub use xerj::{
    CYGNUS_XERJ_DEFAULT_URL, CommitWatermark, DEFAULT_PASSAGE_FIELD, MIN_XERJ_MAX_TOKENS,
    WatermarkDriftReason, WatermarkSyncDecision, XerjClientConfig, XerjEmbedMode,
    XerjFusionStrategy, XerjHybridQuery, XerjHybridQueryBuilder, XerjHybridSearchStore,
    XerjLocalJudge, XerjMappingMeta, XerjMemoryRecallHit, XerjMemoryRecallQuery, XerjSearchHit,
    XerjSearchResponse, compute_corpus_content_hash, validate_commit_sha, validate_content_hash,
    validate_xerj_identifier,
};

/// The schema version that [`SqliteStores::open`] writes.
pub const SCHEMA_VERSION: i64 = 4;

/// The most edit keys that one memory scope keeps. A retry comes soon after
/// its first attempt, so the oldest key is safe to forget. The engine keeps
/// the same bound for the episode log.
pub const MAX_EDIT_KEYS: i64 = 4_096;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memory_scopes (
    scope  TEXT PRIMARY KEY,
    blocks TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS memory_edits (
    seq   INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL,
    key   TEXT NOT NULL,
    error TEXT,
    UNIQUE (scope, key)
);
CREATE TABLE IF NOT EXISTS episode_logs (
    scope TEXT PRIMARY KEY,
    log   TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sent_reports (
    key TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS episode_clears (
    scope   TEXT PRIMARY KEY,
    key     TEXT NOT NULL,
    cleared TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS approvals (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    call_id   TEXT NOT NULL,
    tool      TEXT NOT NULL,
    arguments TEXT NOT NULL,
    reason    TEXT NOT NULL,
    signal    TEXT UNIQUE,
    requester TEXT,
    verdict   TEXT,
    closed    INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS xerj_watermarks (
    index_name    TEXT PRIMARY KEY,
    commit_sha    TEXT NOT NULL,
    content_hash  TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
";

/// An error that opens no store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite failed.
    #[error("the memory store failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The file holds a newer schema than this build knows.
    #[error("the memory store has schema {found}, and this build knows {SCHEMA_VERSION} only")]
    Schema {
        /// The version in the file.
        found: i64,
    },
}

/// The memory and the episode log of every agent, in one SQLite file.
///
/// Its `Debug` output shows no entry and no episode, because both can hold
/// personal data.
pub struct SqliteStores {
    conn: Mutex<Connection>,
    template: Vec<MemoryBlock>,
}

impl std::fmt::Debug for SqliteStores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStores").finish_non_exhaustive()
    }
}

impl SqliteStores {
    /// Opens the stores in the file at `path`, and creates the tables.
    ///
    /// A new memory scope starts with [`MemoryBlock::defaults`].
    ///
    /// # Errors
    ///
    /// Returns an error when the file does not open, or holds a newer schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::with_connection(Connection::open(path)?)
    }

    /// Opens stores that live in process memory, for tests.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite cannot open a memory database.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::with_connection(Connection::open_in_memory()?)
    }

    fn with_connection(conn: Connection) -> Result<Self, StoreError> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if found > SCHEMA_VERSION {
            return Err(StoreError::Schema { found });
        }
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self {
            conn: Mutex::new(conn),
            template: MemoryBlock::defaults(),
        })
    }

    /// Runs `f` in one immediate transaction, and commits when it succeeds.
    fn write<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, Failure>,
    ) -> Result<T, AgentError> {
        let mut conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        let run = || -> Result<T, Failure> {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let value = f(&tx)?;
            tx.commit()?;
            Ok(value)
        };
        run().map_err(Failure::into_agent)
    }

    /// Runs `f` on the connection, for a read.
    fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T, Failure>) -> Result<T, AgentError> {
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        f(&conn).map_err(Failure::into_agent)
    }

    /// Whether a delivery sent the report with `key`.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot read.
    pub fn was_sent(&self, key: &str) -> Result<bool, AgentError> {
        self.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM sent_reports WHERE key = ?1",
                    params![key],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
    }

    /// Records that a delivery sent the report with `key`.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot write.
    pub fn mark_sent(&self, key: &str) -> Result<(), AgentError> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO sent_reports (key) VALUES (?1) ON CONFLICT (key) DO NOTHING",
                params![key],
            )?;
            Ok(())
        })
    }

    /// Applies one memory edit once for its key.
    pub(crate) fn apply_now(
        &self,
        scope: &MemoryScope,
        op: &MemoryOp,
        key: &EditKey,
        labels: &BTreeSet<String>,
    ) -> Result<MemoryBlock, AgentError> {
        self.write(|tx| {
            let mut blocks = self.blocks(tx, scope)?;
            let stored: Option<Option<String>> = tx
                .query_row(
                    "SELECT error FROM memory_edits WHERE scope = ?1 AND key = ?2",
                    params![scope.as_str(), key.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            match stored {
                // An edit fails only by the rules of `apply_op`, and each such
                // error is a `Tool` error.
                Some(Some(message)) => Ok(Err(AgentError::new(ErrorKind::Tool, message))),
                Some(None) => Ok(current(&blocks, op)),
                None => {
                    let result = autumn_harvest_agent::memory::apply_to(&mut blocks, op, labels);
                    tx.execute(
                        "INSERT INTO memory_edits (scope, key, error) VALUES (?1, ?2, ?3)",
                        params![
                            scope.as_str(),
                            key.as_str(),
                            result.as_ref().err().map(AgentError::message)
                        ],
                    )?;
                    tx.execute(
                        "DELETE FROM memory_edits WHERE scope = ?1 AND seq <= \
                         (SELECT MAX(seq) FROM memory_edits WHERE scope = ?1) - ?2",
                        params![scope.as_str(), MAX_EDIT_KEYS],
                    )?;
                    if result.is_ok() {
                        tx.execute(
                            "INSERT INTO memory_scopes (scope, blocks) VALUES (?1, ?2) \
                             ON CONFLICT (scope) DO UPDATE SET blocks = excluded.blocks",
                            params![scope.as_str(), encode(&blocks)?],
                        )?;
                    }
                    Ok(result)
                }
            }
        })?
    }

    /// Evaluates whether `key` was already applied or fails locally before
    /// emitting a remote Reverie/XERJ write-through.
    pub(crate) fn preview_memory_edit(
        &self,
        scope: &MemoryScope,
        op: &MemoryOp,
        key: &EditKey,
        labels: &BTreeSet<String>,
    ) -> Result<MemoryEditPreview, AgentError> {
        self.write(|tx| {
            let mut blocks = self.blocks(tx, scope)?;
            let stored: Option<Option<String>> = tx
                .query_row(
                    "SELECT error FROM memory_edits WHERE scope = ?1 AND key = ?2",
                    params![scope.as_str(), key.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            match stored {
                Some(Some(message)) => Ok(MemoryEditPreview::Cached(Err(AgentError::new(
                    ErrorKind::Tool,
                    message,
                )))),
                Some(None) => Ok(MemoryEditPreview::Cached(current(&blocks, op))),
                None => match autumn_harvest_agent::memory::apply_to(&mut blocks, op, labels) {
                    Ok(updated_block) => Ok(MemoryEditPreview::ReadyToSync { updated_block }),
                    Err(err) => {
                        tx.execute(
                            "INSERT INTO memory_edits (scope, key, error) VALUES (?1, ?2, ?3)",
                            params![scope.as_str(), key.as_str(), err.message()],
                        )?;
                        tx.execute(
                            "DELETE FROM memory_edits WHERE scope = ?1 AND seq <= \
                             (SELECT MAX(seq) FROM memory_edits WHERE scope = ?1) - ?2",
                            params![scope.as_str(), MAX_EDIT_KEYS],
                        )?;
                        Ok(MemoryEditPreview::FailedNew(err))
                    }
                },
            }
        })
    }

    /// Checks whether an episode append key was already applied before remote sync.
    pub(crate) fn preview_episode_append(
        &self,
        scope: &MemoryScope,
        key: &EditKey,
        episode: &Episode,
        max_episodes: u32,
    ) -> Result<EpisodeAppendPreview, AgentError> {
        self.read(|conn| {
            let log = load_log(conn, scope)?;
            let mut candidate = log.clone();
            let appended = candidate.append(key, episode.clone(), max_episodes);
            if candidate == log {
                Ok(EpisodeAppendPreview::Cached(appended))
            } else {
                Ok(EpisodeAppendPreview::ReadyToSync)
            }
        })
    }

    /// Checks whether a consolidation pass mark is stale or already applied.
    pub(crate) fn preview_episode_mark(
        &self,
        scope: &MemoryScope,
        mark: &PassMark,
    ) -> Result<EpisodeMarkPreview, AgentError> {
        self.read(|conn| {
            let log = load_log(conn, scope)?;
            let mut candidate = log.clone();
            if !candidate.mark(mark) {
                Ok(EpisodeMarkPreview::StaleGeneration)
            } else if candidate == log {
                Ok(EpisodeMarkPreview::CachedTrue)
            } else {
                Ok(EpisodeMarkPreview::ReadyToSync)
            }
        })
    }

    /// Checks whether an episode clear key was already applied before remote sync.
    pub(crate) fn preview_episode_clear(
        &self,
        scope: &MemoryScope,
        key: &EditKey,
    ) -> Result<EpisodeClearPreview, AgentError> {
        self.read(|conn| {
            let last: Option<(String, String)> = conn
                .query_row(
                    "SELECT key, cleared FROM episode_clears WHERE scope = ?1",
                    params![scope.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((last_key, cleared)) = last
                && last_key == key.as_str()
            {
                return Ok(EpisodeClearPreview::Cached(decode(&cleared)?));
            }
            Ok(EpisodeClearPreview::ReadyToSync)
        })
    }

    /// Reads `(commit_sha, content_hash, updated_at_ms)` from `xerj_watermarks`.
    pub(crate) fn read_xerj_watermark(
        &self,
        index_name: &str,
    ) -> Result<Option<(String, String, u64)>, AgentError> {
        self.read(|conn| {
            let row: Option<(String, String, i64)> = conn
                .query_row(
                    "SELECT commit_sha, content_hash, updated_at_ms FROM xerj_watermarks WHERE index_name = ?1",
                    params![index_name],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            Ok(row.map(|(sha, hash, ts)| (sha, hash, u64::try_from(ts).unwrap_or(0))))
        })
    }

    /// Upserts `(commit_sha, content_hash, updated_at_ms)` into `xerj_watermarks`.
    pub(crate) fn write_xerj_watermark(
        &self,
        index_name: &str,
        commit_sha: &str,
        content_hash: &str,
        updated_at_ms: u64,
    ) -> Result<(), AgentError> {
        let ts = i64::try_from(updated_at_ms).unwrap_or(i64::MAX);
        self.write(|tx| {
            tx.execute(
                "INSERT INTO xerj_watermarks (index_name, commit_sha, content_hash, updated_at_ms) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (index_name) DO UPDATE SET \
                 commit_sha = excluded.commit_sha, \
                 content_hash = excluded.content_hash, \
                 updated_at_ms = excluded.updated_at_ms",
                params![index_name, commit_sha, content_hash, ts],
            )?;
            Ok(())
        })
    }

    /// The blocks of `scope`, or the template for a new scope.
    fn blocks(&self, conn: &Connection, scope: &MemoryScope) -> Result<Vec<MemoryBlock>, Failure> {
        let stored: Option<String> = conn
            .query_row(
                "SELECT blocks FROM memory_scopes WHERE scope = ?1",
                params![scope.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        stored.map_or_else(|| Ok(self.template.clone()), |text| decode(&text))
    }

    /// Changes the episode log of `scope` with `f`, in one transaction.
    fn change_log<T>(
        &self,
        scope: &MemoryScope,
        f: impl FnOnce(&mut EpisodeLog) -> T,
    ) -> Result<T, AgentError> {
        self.write(|tx| {
            let mut log = load_log(tx, scope)?;
            let value = f(&mut log);
            save_log(tx, scope, &log)?;
            Ok(value)
        })
    }
}

impl MemoryStore for SqliteStores {
    fn load<'a>(
        &'a self,
        scope: &'a MemoryScope,
    ) -> BoxFuture<'a, Result<Vec<MemoryBlock>, AgentError>> {
        Box::pin(std::future::ready(
            self.read(|conn| self.blocks(conn, scope)),
        ))
    }

    fn apply<'a>(
        &'a self,
        scope: &'a MemoryScope,
        op: MemoryOp,
        key: &'a EditKey,
    ) -> BoxFuture<'a, Result<MemoryBlock, AgentError>> {
        Box::pin(std::future::ready(self.apply_now(
            scope,
            &op,
            key,
            &BTreeSet::new(),
        )))
    }

    fn apply_labelled<'a>(
        &'a self,
        scope: &'a MemoryScope,
        op: MemoryOp,
        key: &'a EditKey,
        labels: &'a BTreeSet<String>,
    ) -> BoxFuture<'a, Result<MemoryBlock, AgentError>> {
        Box::pin(std::future::ready(self.apply_now(scope, &op, key, labels)))
    }

    fn clear<'a>(&'a self, scope: &'a MemoryScope) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(std::future::ready(self.write(|tx| {
            tx.execute(
                "DELETE FROM memory_scopes WHERE scope = ?1",
                params![scope.as_str()],
            )?;
            tx.execute(
                "DELETE FROM memory_edits WHERE scope = ?1",
                params![scope.as_str()],
            )?;
            Ok(())
        })))
    }
}

impl EpisodeStore for SqliteStores {
    fn load<'a>(&'a self, scope: &'a MemoryScope) -> BoxFuture<'a, Result<EpisodeLog, AgentError>> {
        Box::pin(std::future::ready(self.read(|conn| load_log(conn, scope))))
    }

    fn append<'a>(
        &'a self,
        scope: &'a MemoryScope,
        key: &'a EditKey,
        episode: Episode,
        max_episodes: u32,
    ) -> BoxFuture<'a, Result<Appended, AgentError>> {
        Box::pin(std::future::ready(
            self.change_log(scope, |log| log.append(key, episode, max_episodes)),
        ))
    }

    fn mark<'a>(
        &'a self,
        scope: &'a MemoryScope,
        mark: PassMark,
    ) -> BoxFuture<'a, Result<bool, AgentError>> {
        Box::pin(std::future::ready(
            self.change_log(scope, |log| log.mark(&mark)),
        ))
    }

    fn clear<'a>(
        &'a self,
        scope: &'a MemoryScope,
        key: &'a EditKey,
    ) -> BoxFuture<'a, Result<Cleared, AgentError>> {
        Box::pin(std::future::ready(self.write(|tx| {
            let last: Option<(String, String)> = tx
                .query_row(
                    "SELECT key, cleared FROM episode_clears WHERE scope = ?1",
                    params![scope.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((last, cleared)) = last
                && last == key.as_str()
            {
                return decode(&cleared);
            }
            let mut log = load_log(tx, scope)?;
            let cleared = log.clear();
            save_log(tx, scope, &log)?;
            tx.execute(
                "INSERT INTO episode_clears (scope, key, cleared) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (scope) DO UPDATE SET key = excluded.key, cleared = excluded.cleared",
                params![scope.as_str(), key.as_str(), encode(&cleared)?],
            )?;
            Ok(cleared)
        })))
    }
}

/// The block that `op` names, unchanged.
fn current(blocks: &[MemoryBlock], op: &MemoryOp) -> Result<MemoryBlock, AgentError> {
    blocks
        .iter()
        .find(|block| block.label == op.block())
        .cloned()
        .ok_or_else(|| {
            AgentError::new(ErrorKind::Tool, format!("no memory block {:?}", op.block()))
        })
}

fn load_log(conn: &Connection, scope: &MemoryScope) -> Result<EpisodeLog, Failure> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT log FROM episode_logs WHERE scope = ?1",
            params![scope.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    stored.map_or_else(|| Ok(EpisodeLog::default()), |text| decode(&text))
}

fn save_log(tx: &Transaction<'_>, scope: &MemoryScope, log: &EpisodeLog) -> Result<(), Failure> {
    tx.execute(
        "INSERT INTO episode_logs (scope, log) VALUES (?1, ?2) \
         ON CONFLICT (scope) DO UPDATE SET log = excluded.log",
        params![scope.as_str(), encode(log)?],
    )?;
    Ok(())
}

fn encode(value: &impl serde::Serialize) -> Result<String, Failure> {
    serde_json::to_string(value).map_err(Failure::Json)
}

fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, Failure> {
    serde_json::from_str(text).map_err(Failure::Json)
}

/// A failure inside one store call.
#[derive(Debug)]
enum Failure {
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
}

impl From<rusqlite::Error> for Failure {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl Failure {
    /// The error that the engine sees. A SQLite failure can pass, so a
    /// retry may help. A value that does not decode stays broken.
    fn into_agent(self) -> AgentError {
        match self {
            Self::Sqlite(error) => AgentError::new(
                ErrorKind::Unavailable,
                format!("the memory store failed: {error}"),
            ),
            Self::Json(error) => AgentError::new(
                ErrorKind::Decode,
                format!("the memory store holds a value that does not decode: {error}"),
            ),
        }
    }
}

#[derive(Debug)]
pub(crate) enum MemoryEditPreview {
    Cached(Result<MemoryBlock, AgentError>),
    FailedNew(AgentError),
    ReadyToSync { updated_block: MemoryBlock },
}

#[derive(Debug)]
pub(crate) enum EpisodeAppendPreview {
    Cached(Appended),
    ReadyToSync,
}

#[derive(Debug)]
pub(crate) enum EpisodeMarkPreview {
    StaleGeneration,
    CachedTrue,
    ReadyToSync,
}

#[derive(Debug)]
pub(crate) enum EpisodeClearPreview {
    Cached(Cleared),
    ReadyToSync,
}

#[cfg(test)]
mod tests;
