//! `blackwall-serve` custody bundle builder and importer (`specs/0008`, `REQ-LG-CUSTODY-01..04`).
//!
//! [`BlackwallCustodyClient`] constructs a content-addressed [`CustodyBundle`] whose
//! [`CustodyRunRecord`] matches `blackwall_store::RunRecord` byte-for-byte under
//! `serde_json::to_string_pretty` and `blake3`, uploads it to
//! `{blackwall_url}/v1/projects/{project}/runs/import`, and verifies that the server
//! returns the identical 64-hex `run_hash`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use littleguy_core::BlackwallProject;
use littleguy_stores::{
    AdapterError, HttpMethod, HttpTransport, ReqwestHttpTransport, StoreHttpRequest, Url,
    join_url_segments, parse_clean_base_url,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::pool::TurnError;
use crate::secret_file::{SecretFile, SecretFileError};

/// Default `--blackwall-project` slug when none is explicitly configured (`REQ-LG-CUSTODY-01`).
pub const DEFAULT_BLACKWALL_PROJECT: &str = "vespertin";

/// `parent_world` value recorded in [`CustodyContext`] (`REQ-LG-CUSTODY-02`).
pub const BLACKWALL_PARENT_WORLD: &str = "littleguy-pool";

/// `custody` pointer label emitted in pool task replies (`ves_domain::escapement::TaskResult`).
pub const BLACKWALL_CUSTODY_LABEL: &str = "blackwall-serve";

/// Wire version of `blackwall_store::CustodyBundle` (`CUSTODY_BUNDLE_VERSION`).
pub const CUSTODY_BUNDLE_VERSION: u32 = 1;

/// Per-request timeout on `POST /v1/projects/{project}/runs/import` (`REQ-LG-CUSTODY-02`).
pub const BLACKWALL_IMPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors returned while configuring [`BlackwallCustodyClient`] or building a [`CustodyBundle`].
#[derive(Debug, Error)]
pub enum CustodyError {
    /// `--blackwall-url` failed clean base URL validation.
    #[error("invalid blackwall base URL: {0}")]
    InvalidUrl(String),
    /// `--blackwall-token-file` failed [`SecretFile::load`] validation.
    #[error("invalid blackwall token file: {0}")]
    TokenFile(#[from] SecretFileError),
    /// Initializing the HTTP transport failed.
    #[error("blackwall HTTP client initialization failed: {0}")]
    Transport(String),
    /// Serializing a [`CustodyRunRecord`] failed.
    #[error("failed to serialize custody run record: {0}")]
    Encode(String),
}

/// Canonical run trace record matching `blackwall_store::RunRecord` field order (`REQ-LG-CUSTODY-02`).
///
/// Optional/default fields (`usage`, `unjailed`, `failure`, `fallback_hops`) are omitted
/// when empty/false/`None` in `blackwall_store::RunRecord`, so declaring `prompt`, `model`,
/// `stdout`, `stderr`, `exit_code`, and `project` in this exact order produces identical
/// `serde_json::to_string_pretty` bytes and `blake3` hex digests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustodyRunRecord {
    /// Prompt text that initiated the run.
    pub prompt: String,
    /// Model identifier that executed the run.
    pub model: String,
    /// Assistant answer captured as run stdout.
    pub stdout: String,
    /// Run stderr (`""` on clean pool turns).
    pub stderr: String,
    /// Run exit code (`0` on clean pool turns).
    pub exit_code: i32,
    /// Target project slug (`vespertin` or `org/repo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

impl CustodyRunRecord {
    /// Serializes this record with `serde_json::to_string_pretty` matching `blackwall_store::RunRecord::to_json`.
    ///
    /// # Errors
    ///
    /// Returns [`CustodyError::Encode`] if JSON serialization fails.
    pub fn canonical_json(&self) -> Result<String, CustodyError> {
        serde_json::to_string_pretty(self).map_err(|err| CustodyError::Encode(err.to_string()))
    }

    /// Computes the 64-character lowercase `blake3` hex digest of [`Self::canonical_json`].
    ///
    /// # Errors
    ///
    /// Returns [`CustodyError::Encode`] if JSON serialization fails.
    pub fn blake3_hex(&self) -> Result<String, CustodyError> {
        let json = self.canonical_json()?;
        Ok(blake3::hash(json.as_bytes()).to_hex().to_string())
    }
}

/// Persisted settle state of retained run output (`blackwall_store::RetainedOutputState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CustodySettleState {
    /// Retained output is stored in custody and not yet settled.
    #[default]
    Unsettled,
}

/// Settle context linking a `run_hash` to its parent world (`blackwall_store::Context`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustodyContext {
    /// 64-character BLAKE3 hex hash of [`CustodyRunRecord`].
    pub run: String,
    /// Parent world label (`"littleguy-pool"`).
    pub parent_world: String,
    /// Changeset tree OID (`""` when no filesystem changeset is attached).
    pub changeset: String,
    /// Persisted settle state (`{"status": "unsettled"}`).
    pub settle_state: CustodySettleState,
}

/// Self-contained custody bundle uploaded to `blackwall-serve` (`blackwall_store::CustodyBundle`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustodyBundle {
    /// Wire format version ([`CUSTODY_BUNDLE_VERSION`]).
    pub version: u32,
    /// BLAKE3 hex hash of `record` (`serde_json::to_string_pretty`).
    pub run_hash: String,
    /// Run trace record.
    pub record: CustodyRunRecord,
    /// Settle bridge context.
    pub context: CustodyContext,
    /// Git blob payloads (`{}` when `changeset` is empty).
    #[serde(default)]
    pub blobs: BTreeMap<String, String>,
}

impl CustodyBundle {
    /// Constructs a version `1` [`CustodyBundle`] for a completed pool turn.
    ///
    /// # Errors
    ///
    /// Returns [`CustodyError::Encode`] if canonical JSON serialization fails.
    pub fn for_turn(
        prompt: &str,
        model: &str,
        answer: &str,
        project: &BlackwallProject,
    ) -> Result<Self, CustodyError> {
        let record = CustodyRunRecord {
            prompt: prompt.to_owned(),
            model: model.to_owned(),
            stdout: answer.to_owned(),
            stderr: String::new(),
            exit_code: 0,
            project: Some(project.as_str().to_owned()),
        };
        let run_hash = record.blake3_hex()?;
        Ok(Self {
            version: CUSTODY_BUNDLE_VERSION,
            run_hash: run_hash.clone(),
            record,
            context: CustodyContext {
                run: run_hash,
                parent_world: BLACKWALL_PARENT_WORLD.to_owned(),
                changeset: String::new(),
                settle_state: CustodySettleState::Unsettled,
            },
            blobs: BTreeMap::new(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct RunImportResponse {
    run_hash: String,
}

/// Authenticated HTTP client for `blackwall-serve` custody bundle imports (`REQ-LG-CUSTODY-01..03`).
#[derive(Clone)]
pub struct BlackwallCustodyClient {
    base_url: Url,
    token: SecretFile,
    default_project: BlackwallProject,
    transport: Arc<dyn HttpTransport>,
}

impl fmt::Debug for BlackwallCustodyClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlackwallCustodyClient")
            .field("base_url", &self.base_url.as_str())
            .field("token", &self.token)
            .field("default_project", &self.default_project)
            .finish_non_exhaustive()
    }
}

impl BlackwallCustodyClient {
    /// Validates `raw_url`, loads `token_file` via [`SecretFile::load`], and builds a live client.
    ///
    /// # Errors
    ///
    /// Returns [`CustodyError`] when `raw_url` fails clean base URL validation, `token_file`
    /// fails `0600` regular-file checks, or [`ReqwestHttpTransport`] initialization fails.
    pub fn open(
        raw_url: &str,
        token_file: &Path,
        default_project: BlackwallProject,
    ) -> Result<Self, CustodyError> {
        let base_url = parse_clean_base_url(raw_url, "blackwall_url")
            .map_err(|err| CustodyError::InvalidUrl(err.to_string()))?;
        let token = SecretFile::load(token_file)?;
        let transport = Arc::new(
            ReqwestHttpTransport::new().map_err(|err| CustodyError::Transport(err.to_string()))?,
        );
        Ok(Self {
            base_url,
            token,
            default_project,
            transport,
        })
    }

    /// Builds a client over a custom [`HttpTransport`] (for deterministic offline tests).
    ///
    /// # Errors
    ///
    /// Returns [`CustodyError::InvalidUrl`] when `raw_url` fails clean base URL validation.
    pub fn with_transport(
        raw_url: &str,
        token: SecretFile,
        default_project: BlackwallProject,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, CustodyError> {
        let base_url = parse_clean_base_url(raw_url, "blackwall_url")
            .map_err(|err| CustodyError::InvalidUrl(err.to_string()))?;
        Ok(Self {
            base_url,
            token,
            default_project,
            transport,
        })
    }

    /// Default [`BlackwallProject`] configured at worker startup.
    #[must_use]
    pub const fn default_project(&self) -> &BlackwallProject {
        &self.default_project
    }

    /// Resolves `{blackwall_url}/v1/projects/{project}/runs/import` for 1- or 2-segment slugs.
    ///
    /// # Errors
    ///
    /// Returns [`CustodyError::InvalidUrl`] if URL segment joining fails.
    pub fn import_url(&self, project: &BlackwallProject) -> Result<Url, CustodyError> {
        let mut segments = Vec::with_capacity(6);
        segments.extend(["v1", "projects"]);
        segments.extend(project.path_segments());
        segments.extend(["runs", "import"]);
        join_url_segments(&self.base_url, &segments)
            .map_err(|err| CustodyError::InvalidUrl(err.to_string()))
    }

    /// Uploads a completed turn's [`CustodyBundle`] to `blackwall-serve` and verifies `run_hash`.
    ///
    /// # Errors
    ///
    /// Returns [`TurnError::Transient`] on HTTP `429`, `5xx`, timeouts, or network transport
    /// failures so [`crate::pool::PoolWorker`] calls `nack`, and [`TurnError::Permanent`] on
    /// `4xx`, malformed JSON, or `run_hash` mismatches.
    pub async fn import_turn(
        &self,
        prompt: &str,
        model: &str,
        answer: &str,
        project_override: Option<&BlackwallProject>,
    ) -> Result<String, TurnError> {
        let project = project_override.unwrap_or(&self.default_project);
        let bundle = CustodyBundle::for_turn(prompt, model, answer, project)
            .map_err(|err| TurnError::Permanent(err.to_string()))?;
        let url = self
            .import_url(project)
            .map_err(|err| TurnError::Permanent(err.to_string()))?;
        let request = StoreHttpRequest::new(HttpMethod::Post, url.clone())
            .with_timeout(BLACKWALL_IMPORT_TIMEOUT)
            .with_header(
                "Authorization",
                format!("Bearer {}", self.token.expose_secret()),
            )
            .with_json_body(&bundle)
            .map_err(|err| TurnError::Permanent(err.to_string()))?;
        let sent = tokio::time::timeout(BLACKWALL_IMPORT_TIMEOUT, self.transport.send(request))
            .await
            .map_err(|_| {
                TurnError::Transient(format!(
                    "blackwall custody import timed out after {}s",
                    BLACKWALL_IMPORT_TIMEOUT.as_secs()
                ))
            })?;
        let response = match sent {
            Ok(resp) => resp,
            Err(AdapterError::Transport { message }) => {
                return Err(TurnError::Transient(format!(
                    "blackwall custody transport error: {message}"
                )));
            }
            Err(AdapterError::HttpStatus { status, url, body })
                if status == 429 || status >= 500 =>
            {
                return Err(TurnError::Transient(format!(
                    "blackwall custody HTTP {status} from {url}: {body}"
                )));
            }
            Err(other) => {
                return Err(TurnError::Permanent(format!(
                    "blackwall custody error: {other}"
                )));
            }
        };
        if response.status == 429 || response.status >= 500 {
            return Err(TurnError::Transient(format!(
                "blackwall custody HTTP {} from {}: {}",
                response.status,
                url,
                response.body_preview()
            )));
        }
        if !response.is_success() {
            return Err(TurnError::Permanent(format!(
                "blackwall custody HTTP {} from {}: {}",
                response.status,
                url,
                response.body_preview()
            )));
        }
        let imported: RunImportResponse = response
            .decode_json("blackwall runs/import response")
            .map_err(|err| TurnError::Permanent(format!("blackwall custody decode: {err}")))?;
        if imported.run_hash != bundle.run_hash {
            return Err(TurnError::Permanent(format!(
                "blackwall custody run_hash mismatch: local `{}`, remote `{}`",
                bundle.run_hash, imported.run_hash
            )));
        }
        Ok(bundle.run_hash)
    }
}
