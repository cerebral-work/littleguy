//! Durable Reverie observation and smart-context adapter (`specs/0003`, `REQ-LG-REVERIE-01..04`).
//!
//! Connects `littleguy` memory scopes and episode logs to the Reverie daemon
//! (`http://127.0.0.1:7437` by default) over [`HttpTransport`]:
//! - `GET /search?q=...` for full-text and tag-filtered observation search,
//! - `GET /context/smart?project=...` for session smart-context hydration,
//! - `POST /observations` for typed observation writes (`topic_key` + `project` + flow-label tags),
//! - [`MemoryStore`] and [`EpisodeStore`] implementations backed by local SQLite
//!   [`EditKey`] idempotency and taint-preserving write-through.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use autumn_harvest_agent::AgentError;
use autumn_harvest_agent::episode::{
    Appended, Cleared, Episode, EpisodeLog, EpisodeStore, PassMark,
};
use autumn_harvest_agent::memory::{EditKey, MemoryBlock, MemoryOp, MemoryScope, MemoryStore};
use autumn_harvest_agent::model::BoxFuture;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::transport::{
    AdapterError, DEFAULT_HTTP_TIMEOUT, HttpMethod, HttpTransport, StoreHttpRequest,
    join_url_segments, parse_clean_base_url,
};
use crate::{
    EpisodeAppendPreview, EpisodeClearPreview, EpisodeMarkPreview, MemoryEditPreview, SqliteStores,
};

/// Default local Reverie HTTP daemon endpoint (`:7437`).
pub const DEFAULT_REVERIE_URL: &str = "http://127.0.0.1:7437";

/// Default `limit` passed to `/context/smart`.
pub const DEFAULT_SMART_CONTEXT_LIMIT: u32 = 15;

/// Upper bound on `limit` passed to `/context/smart`.
pub const MAX_SMART_CONTEXT_LIMIT: u32 = 50;

/// Upper bound on `limit` passed to `/search`.
pub const MAX_REVERIE_SEARCH_LIMIT: u32 = 20;

/// Reverie observation visibility scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReverieScope {
    /// Shared project memory (`project`).
    #[default]
    Project,
    /// Operator personal memory (`personal`).
    Personal,
}

impl ReverieScope {
    /// Canonical wire token (`"project"` or `"personal"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Personal => "personal",
        }
    }
}

/// Reverie observation taxonomy (`mem_save` `type` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReverieObservationKind {
    /// Architectural or policy decision.
    Decision,
    /// System structure note.
    Architecture,
    /// Root cause and fix note.
    Bugfix,
    /// Reusable implementation pattern.
    Pattern,
    /// Configuration fact.
    Config,
    /// Research or probe finding.
    Discovery,
    /// Operational lesson learned.
    Learning,
    /// Manual or general agent memory note.
    #[default]
    Manual,
}

impl ReverieObservationKind {
    /// Canonical `snake_case` wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Architecture => "architecture",
            Self::Bugfix => "bugfix",
            Self::Pattern => "pattern",
            Self::Config => "config",
            Self::Discovery => "discovery",
            Self::Learning => "learning",
            Self::Manual => "manual",
        }
    }
}

/// Reverie `topic_key` write mode (`append` vs `upsert`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReverieWriteMode {
    /// Appends a new entry in the topic series (default for episodes and handoffs).
    #[default]
    Append,
    /// Replaces the latest state marker under `topic_key`.
    Upsert,
}

impl ReverieWriteMode {
    /// Canonical `snake_case` wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Upsert => "upsert",
        }
    }
}

/// Faceted tag attached to a [`ReverieObservationPayload`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReverieTag {
    /// Facet name (for example `"Subject"`, `"Domain"`, `"FlowLabel"`).
    pub facet: String,
    /// Tag value.
    pub value: String,
}

impl ReverieTag {
    /// Constructs a validated `(facet, value)` tag.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidIdentifier`] if `facet` or `value` is empty
    /// or contains control characters.
    pub fn new(facet: impl Into<String>, value: impl Into<String>) -> Result<Self, AdapterError> {
        let facet = facet.into();
        let value = value.into();
        if facet.trim().is_empty() || facet.chars().any(char::is_control) {
            return Err(AdapterError::InvalidIdentifier {
                kind: "reverie_tag_facet",
                value: facet,
                reason: "tag facet must be non-empty with no control characters".to_owned(),
            });
        }
        if value.trim().is_empty() || value.chars().any(char::is_control) {
            return Err(AdapterError::InvalidIdentifier {
                kind: "reverie_tag_value",
                value,
                reason: "tag value must be non-empty with no control characters".to_owned(),
            });
        }
        Ok(Self {
            facet: facet.trim().to_owned(),
            value: value.trim().to_owned(),
        })
    }
}

/// Validated configuration for [`ReverieObservationStore`] (`REQ-LG-REVERIE-01`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReverieClientConfig {
    base_url: Url,
    default_project: String,
    default_scope: ReverieScope,
    smart_context_limit: u32,
    timeout: Duration,
    agent_id: Option<String>,
    subject: Option<String>,
}

impl ReverieClientConfig {
    /// Validates `base_url` and `default_project` to construct a [`ReverieClientConfig`].
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidConfig`] if `base_url` is malformed, uses an
    /// unsupported scheme, embeds credentials, or `default_project` is invalid.
    pub fn new(base_url: &str, default_project: impl Into<String>) -> Result<Self, AdapterError> {
        let parsed = parse_clean_base_url(base_url, "reverie.base_url")?;
        let project = validate_project_slug(&default_project.into())?;
        Ok(Self {
            base_url: parsed,
            default_project: project,
            default_scope: ReverieScope::Project,
            smart_context_limit: DEFAULT_SMART_CONTEXT_LIMIT,
            timeout: DEFAULT_HTTP_TIMEOUT,
            agent_id: None,
            subject: None,
        })
    }

    /// Creates a config pointing at [`DEFAULT_REVERIE_URL`] (`http://127.0.0.1:7437`).
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidConfig`] if `default_project` is invalid.
    pub fn localhost_default(default_project: impl Into<String>) -> Result<Self, AdapterError> {
        Self::new(DEFAULT_REVERIE_URL, default_project)
    }

    /// Sets the default [`ReverieScope`].
    #[must_use]
    pub const fn with_scope(mut self, scope: ReverieScope) -> Self {
        self.default_scope = scope;
        self
    }

    /// Sets the per-request HTTP timeout.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Sets the default `limit` for `/context/smart` (`1..=50`).
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidConfig`] when `limit` is outside `1..=50`.
    pub fn with_smart_context_limit(mut self, limit: u32) -> Result<Self, AdapterError> {
        if !(1..=MAX_SMART_CONTEXT_LIMIT).contains(&limit) {
            return Err(AdapterError::InvalidConfig {
                field: "reverie.smart_context_limit",
                reason: format!("limit must be in 1..={MAX_SMART_CONTEXT_LIMIT}, got {limit}"),
            });
        }
        self.smart_context_limit = limit;
        Ok(self)
    }

    /// Attaches an authoring `agent_id` (such as a SPIFFE workload URI or holder name).
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidConfig`] if `agent_id` is empty or has control chars.
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Result<Self, AdapterError> {
        let id = agent_id.into();
        if id.trim().is_empty() || id.chars().any(char::is_control) {
            return Err(AdapterError::InvalidConfig {
                field: "reverie.agent_id",
                reason: "agent_id must be non-empty with no control characters".to_owned(),
            });
        }
        self.agent_id = Some(id.trim().to_owned());
        Ok(self)
    }

    /// Attaches a canonical Cortex lane `subject` slug (for example `"memory-knowledge"`).
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidConfig`] if `subject` is empty or has control chars.
    pub fn with_subject(mut self, subject: impl Into<String>) -> Result<Self, AdapterError> {
        let slug = subject.into();
        if slug.trim().is_empty() || slug.chars().any(char::is_control) {
            return Err(AdapterError::InvalidConfig {
                field: "reverie.subject",
                reason: "subject must be non-empty with no control characters".to_owned(),
            });
        }
        self.subject = Some(slug.trim().to_owned());
        Ok(self)
    }

    /// Validated Reverie base URL.
    #[must_use]
    pub const fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Validated default `project` slug.
    #[must_use]
    pub fn default_project(&self) -> &str {
        &self.default_project
    }

    /// Default observation scope.
    #[must_use]
    pub const fn default_scope(&self) -> ReverieScope {
        self.default_scope
    }

    /// Configured `/context/smart` limit.
    #[must_use]
    pub const fn smart_context_limit(&self) -> u32 {
        self.smart_context_limit
    }
}

/// Validates a hierarchical Reverie `topic_key` (`<stem>/<slug>[/...]`, `REQ-LG-REVERIE-02`).
///
/// Rejects empty strings, single-segment keys without a `/` stem separator,
/// leading or trailing `/`, `.` or `..` path traversal segments, whitespace, and
/// non-ASCII characters outside `[a-z0-9._-]`.
///
/// # Errors
///
/// Returns [`AdapterError::InvalidIdentifier`] when `topic_key` violates the grammar.
pub fn validate_topic_key(topic_key: &str) -> Result<String, AdapterError> {
    if topic_key.is_empty() || topic_key.starts_with('/') || topic_key.ends_with('/') {
        return Err(AdapterError::InvalidIdentifier {
            kind: "topic_key",
            value: topic_key.to_owned(),
            reason: "topic_key must be non-empty without leading or trailing `/`".to_owned(),
        });
    }
    let segments: Vec<&str> = topic_key.split('/').collect();
    if segments.len() < 2 {
        return Err(AdapterError::InvalidIdentifier {
            kind: "topic_key",
            value: topic_key.to_owned(),
            reason: "topic_key must contain at least `<stem>/<slug>`".to_owned(),
        });
    }
    for seg in &segments {
        if seg.is_empty()
            || *seg == "."
            || *seg == ".."
            || !seg
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
        {
            return Err(AdapterError::InvalidIdentifier {
                kind: "topic_key",
                value: topic_key.to_owned(),
                reason: format!(
                    "segment `{seg}` must contain only lowercase ASCII `[a-z0-9._-]` and never `.` or `..`"
                ),
            });
        }
    }
    Ok(topic_key.to_owned())
}

fn validate_project_slug(project: &str) -> Result<String, AdapterError> {
    let trimmed = project.trim();
    if trimmed.is_empty()
        || (trimmed != "*"
            && !trimmed
                .bytes()
                .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-')))
    {
        return Err(AdapterError::InvalidConfig {
            field: "reverie.project",
            reason: format!("invalid project identifier `{project}`"),
        });
    }
    Ok(trimmed.to_owned())
}

fn sanitize_topic_segment(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|ch| {
            let lower = ch.to_ascii_lowercase();
            if matches!(lower, 'a'..='z' | '0'..='9' | '_' | '-') {
                lower
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    let trimmed = out.trim_matches('-').to_owned();
    out.clear();
    if trimmed.is_empty() {
        out.push_str("default");
    } else {
        out.push_str(&trimmed);
    }
    out
}

/// Validated observation write payload serialized as JSON to Reverie (`REQ-LG-REVERIE-02`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverieObservationPayload {
    /// Short searchable title.
    pub title: String,
    /// Observation body text.
    pub content: String,
    /// Owning project name.
    pub project: String,
    /// Hierarchical topic identity (`<stem>/<slug>`).
    pub topic_key: String,
    /// Visibility scope (`project` | `personal`).
    pub scope: ReverieScope,
    /// Observation category (`type` on wire).
    #[serde(rename = "type")]
    pub kind: ReverieObservationKind,
    /// Topic series write mode (`append` | `upsert`).
    pub write_mode: ReverieWriteMode,
    /// Optional authoring agent identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Optional session identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional backing evidence links (SHAs, paths, ticket IDs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_links: Vec<String>,
    /// Optional faceted tags (including preserved `FlowLabel` taint entries).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<ReverieTag>,
}

/// Typed builder for [`ReverieObservationPayload`] (`REQ-LG-REVERIE-02`).
#[derive(Debug, Clone)]
pub struct ReverieObservationBuilder {
    title: String,
    content: String,
    project: String,
    topic_key: String,
    scope: ReverieScope,
    kind: ReverieObservationKind,
    write_mode: ReverieWriteMode,
    agent_id: Option<String>,
    session_id: Option<String>,
    evidence_links: Vec<String>,
    tags: BTreeSet<ReverieTag>,
}

impl ReverieObservationBuilder {
    /// Starts building an observation with required `project`, `topic_key`, `title`, and `content`.
    #[must_use]
    pub fn new(
        project: impl Into<String>,
        topic_key: impl Into<String>,
        title: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            content: content.into(),
            project: project.into(),
            topic_key: topic_key.into(),
            scope: ReverieScope::Project,
            kind: ReverieObservationKind::Manual,
            write_mode: ReverieWriteMode::Append,
            agent_id: None,
            session_id: None,
            evidence_links: Vec::new(),
            tags: BTreeSet::new(),
        }
    }

    /// Sets [`ReverieScope`].
    #[must_use]
    pub const fn scope(mut self, scope: ReverieScope) -> Self {
        self.scope = scope;
        self
    }

    /// Sets [`ReverieObservationKind`].
    #[must_use]
    pub const fn kind(mut self, kind: ReverieObservationKind) -> Self {
        self.kind = kind;
        self
    }

    /// Sets [`ReverieWriteMode`].
    #[must_use]
    pub const fn write_mode(mut self, write_mode: ReverieWriteMode) -> Self {
        self.write_mode = write_mode;
        self
    }

    /// Sets optional `agent_id`.
    #[must_use]
    pub fn agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    /// Sets optional `session_id`.
    #[must_use]
    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Adds an evidence URI, commit SHA, or ticket link.
    #[must_use]
    pub fn evidence_link(mut self, link: impl Into<String>) -> Self {
        self.evidence_links.push(link.into());
        self
    }

    /// Adds a structured `(facet, value)` tag.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidIdentifier`] if the tag is empty or has control chars.
    pub fn tag(
        mut self,
        facet: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, AdapterError> {
        self.tags.insert(ReverieTag::new(facet, value)?);
        Ok(self)
    }

    /// Preserves `autumn-harvest` flow labels as `FlowLabel` tags so taint survives round-trips.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidIdentifier`] if any label is empty or contains control chars.
    pub fn flow_labels<'a>(
        mut self,
        labels: impl IntoIterator<Item = &'a String>,
    ) -> Result<Self, AdapterError> {
        for label in labels {
            self.tags
                .insert(ReverieTag::new("FlowLabel", label.as_str())?);
        }
        Ok(self)
    }

    /// Validates all fields and returns the immutable [`ReverieObservationPayload`].
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] if `project`, `topic_key`, `title`, or `content` is invalid.
    pub fn build(self) -> Result<ReverieObservationPayload, AdapterError> {
        let project = validate_project_slug(&self.project)?;
        if project == "*" {
            return Err(AdapterError::InvalidIdentifier {
                kind: "project",
                value: project,
                reason: "observation writes must name a concrete project, not `*`".to_owned(),
            });
        }
        let topic_key = validate_topic_key(&self.topic_key)?;
        let title = self.title.trim().to_owned();
        if title.is_empty() || title.chars().any(char::is_control) {
            return Err(AdapterError::InvalidIdentifier {
                kind: "title",
                value: self.title,
                reason: "observation title must be non-empty with no control characters".to_owned(),
            });
        }
        let content = self.content.trim().to_owned();
        if content.is_empty() {
            return Err(AdapterError::InvalidIdentifier {
                kind: "content",
                value: String::new(),
                reason: "observation content must not be empty".to_owned(),
            });
        }
        Ok(ReverieObservationPayload {
            title,
            content,
            project,
            topic_key,
            scope: self.scope,
            kind: self.kind,
            write_mode: self.write_mode,
            agent_id: self.agent_id,
            session_id: self.session_id,
            evidence_links: self.evidence_links,
            tags: self.tags.into_iter().collect(),
        })
    }
}

/// Typed query parameters for Reverie `GET /search?q=...` (`REQ-LG-REVERIE-04`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReverieSearchQuery {
    /// Free-text search query (`q`).
    pub query: String,
    /// Optional project filter (`"*"` searches all projects).
    pub project: Option<String>,
    /// Optional scope filter.
    pub scope: Option<ReverieScope>,
    /// Optional `topic_key` prefix filter.
    pub topic_prefix: Option<String>,
    /// Result cap (`1..=20`).
    pub limit: Option<u32>,
}

impl ReverieSearchQuery {
    /// Constructs a search query for `query`.
    #[must_use]
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            project: None,
            scope: None,
            topic_prefix: None,
            limit: None,
        }
    }

    /// Restricts search to `project` (or `"*"` for all projects).
    #[must_use]
    pub fn with_project(mut self, project: impl Into<String>) -> Self {
        self.project = Some(project.into());
        self
    }

    /// Restricts search to `scope`.
    #[must_use]
    pub const fn with_scope(mut self, scope: ReverieScope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Restricts search to `topic_prefix`.
    #[must_use]
    pub fn with_topic_prefix(mut self, topic_prefix: impl Into<String>) -> Self {
        self.topic_prefix = Some(topic_prefix.into());
        self
    }

    /// Caps search hit count (`1..=20`).
    #[must_use]
    pub const fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// One observation hit returned by Reverie `/search`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReverieObservationHit {
    /// Numeric or string observation id.
    pub id: serde_json::Value,
    /// Short title.
    pub title: String,
    /// Observation body or snippet.
    pub content: String,
    /// Project name.
    pub project: String,
    /// Optional `topic_key`.
    pub topic_key: Option<String>,
    /// Structured tags (including `FlowLabel` taints).
    pub tags: Vec<ReverieTag>,
}

impl ReverieObservationHit {
    /// Extracts any `FlowLabel` tags carried by this observation.
    #[must_use]
    pub fn flow_labels(&self) -> BTreeSet<String> {
        self.tags
            .iter()
            .filter(|t| t.facet == "FlowLabel")
            .map(|t| t.value.clone())
            .collect()
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ReverieSearchEnvelope {
    Wrapped { results: Vec<ReverieObservationHit> },
    Direct(Vec<ReverieObservationHit>),
}

/// Smart-context bundle returned by `GET /context/smart?project=...` (`REQ-LG-REVERIE-04`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReverieSmartContext {
    /// Target project name.
    pub project: String,
    /// Formatted markdown context summary returned by `reveried`.
    pub context: String,
    /// Recent structured observations returned alongside the markdown blob.
    pub observations: Vec<ReverieObservationHit>,
}

/// Receipt returned by `POST /observations`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReverieSavedReceipt {
    /// Stored observation id (when returned by server).
    pub id: Option<serde_json::Value>,
    /// Saved `topic_key`.
    pub topic_key: Option<String>,
}

/// Durable Reverie store implementing `autumn-harvest` [`MemoryStore`] and [`EpisodeStore`].
///
/// Its `Debug` output omits memory entries and episodes to protect personal data.
pub struct ReverieObservationStore<T: HttpTransport> {
    config: ReverieClientConfig,
    local: SqliteStores,
    transport: T,
}

impl<T: HttpTransport> fmt::Debug for ReverieObservationStore<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReverieObservationStore")
            .field("base_url", &self.config.base_url.as_str())
            .field("default_project", &self.config.default_project)
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport> ReverieObservationStore<T> {
    /// Opens a durable [`ReverieObservationStore`] backed by the SQLite file at `db_path`
    /// and the supplied [`HttpTransport`].
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Store`] if the SQLite file fails to open or migrate.
    pub fn open(
        db_path: impl AsRef<Path>,
        config: ReverieClientConfig,
        transport: T,
    ) -> Result<Self, AdapterError> {
        let local = SqliteStores::open(db_path)?;
        Ok(Self {
            config,
            local,
            transport,
        })
    }

    /// Opens an in-memory SQLite-backed [`ReverieObservationStore`] for deterministic offline tests.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Store`] if SQLite cannot create an in-memory database.
    pub fn open_in_memory(config: ReverieClientConfig, transport: T) -> Result<Self, AdapterError> {
        let local = SqliteStores::open_in_memory()?;
        Ok(Self {
            config,
            local,
            transport,
        })
    }

    /// Reference to the validated [`ReverieClientConfig`].
    #[must_use]
    pub const fn config(&self) -> &ReverieClientConfig {
        &self.config
    }

    /// Reference to the underlying [`HttpTransport`].
    #[must_use]
    pub const fn transport(&self) -> &T {
        &self.transport
    }

    /// Builds a zero-interpolation `GET /search?q=...` request using [`Url::query_pairs_mut`].
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] if `query` is empty or any filter parameter is invalid.
    pub fn build_search_request(
        &self,
        query: &ReverieSearchQuery,
    ) -> Result<StoreHttpRequest, AdapterError> {
        let trimmed_q = query.query.trim();
        if trimmed_q.is_empty() {
            return Err(AdapterError::InvalidIdentifier {
                kind: "search_query",
                value: query.query.clone(),
                reason: "Reverie search query `q` must not be empty".to_owned(),
            });
        }
        if let Some(limit) = query.limit
            && !(1..=MAX_REVERIE_SEARCH_LIMIT).contains(&limit)
        {
            return Err(AdapterError::InvalidIdentifier {
                kind: "limit",
                value: limit.to_string(),
                reason: format!("search limit must be in 1..={MAX_REVERIE_SEARCH_LIMIT}"),
            });
        }

        let mut url = join_url_segments(&self.config.base_url, &["search"])?;
        let project = match &query.project {
            Some(p) => validate_project_slug(p)?,
            None => self.config.default_project.clone(),
        };
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("q", trimmed_q);
            pairs.append_pair("project", &project);
            if let Some(scope) = query.scope {
                pairs.append_pair("scope", scope.as_str());
            }
            if let Some(prefix) = &query.topic_prefix {
                let clean = prefix.trim();
                if clean.is_empty() || clean.chars().any(char::is_control) {
                    return Err(AdapterError::InvalidIdentifier {
                        kind: "topic_prefix",
                        value: prefix.clone(),
                        reason: "topic_prefix must be non-empty with no control characters"
                            .to_owned(),
                    });
                }
                pairs.append_pair("topic_prefix", clean);
            }
            if let Some(limit) = query.limit {
                pairs.append_pair("limit", &limit.to_string());
            }
        }
        Ok(StoreHttpRequest::new(HttpMethod::Get, url).with_timeout(self.config.timeout))
    }

    /// Builds a zero-interpolation `GET /context/smart?project=...` request.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] if `project` or `limit` fails validation.
    pub fn build_smart_context_request(
        &self,
        project: Option<&str>,
        limit: Option<u32>,
    ) -> Result<StoreHttpRequest, AdapterError> {
        let effective_project = match project {
            Some(p) => validate_project_slug(p)?,
            None => self.config.default_project.clone(),
        };
        let effective_limit = limit.unwrap_or(self.config.smart_context_limit);
        if !(1..=MAX_SMART_CONTEXT_LIMIT).contains(&effective_limit) {
            return Err(AdapterError::InvalidIdentifier {
                kind: "limit",
                value: effective_limit.to_string(),
                reason: format!("smart context limit must be in 1..={MAX_SMART_CONTEXT_LIMIT}"),
            });
        }

        let mut url = join_url_segments(&self.config.base_url, &["context", "smart"])?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("project", &effective_project);
            pairs.append_pair("limit", &effective_limit.to_string());
            pairs.append_pair("scope", self.config.default_scope.as_str());
            if let Some(subject) = &self.config.subject {
                pairs.append_pair("subject", subject);
            }
        }
        Ok(StoreHttpRequest::new(HttpMethod::Get, url).with_timeout(self.config.timeout))
    }

    /// Builds a typed `POST /observations` request from a validated [`ReverieObservationPayload`].
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] if URL construction or JSON serialization fails.
    pub fn build_observation_request(
        &self,
        payload: &ReverieObservationPayload,
    ) -> Result<StoreHttpRequest, AdapterError> {
        let url = join_url_segments(&self.config.base_url, &["observations"])?;
        StoreHttpRequest::new(HttpMethod::Post, url)
            .with_timeout(self.config.timeout)
            .with_json_body(payload)
    }

    /// Executes `GET /search?q=...` and decodes matching [`ReverieObservationHit`] rows.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] on invalid query parameters, transport error, non-2xx
    /// status, or malformed JSON response.
    pub async fn search(
        &self,
        query: &ReverieSearchQuery,
    ) -> Result<Vec<ReverieObservationHit>, AdapterError> {
        let req = self.build_search_request(query)?;
        let req_url = req.url.clone();
        let resp = self.transport.send(req).await?.ensure_success(&req_url)?;
        let decoded: ReverieSearchEnvelope = resp.decode_json("decode Reverie /search response")?;
        Ok(match decoded {
            ReverieSearchEnvelope::Wrapped { results } | ReverieSearchEnvelope::Direct(results) => {
                results
            }
        })
    }

    /// Executes `GET /context/smart?project=...` and decodes the [`ReverieSmartContext`].
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] on validation, transport, HTTP status, or JSON decode failure.
    pub async fn smart_context(
        &self,
        project: Option<&str>,
        limit: Option<u32>,
    ) -> Result<ReverieSmartContext, AdapterError> {
        let req = self.build_smart_context_request(project, limit)?;
        let req_url = req.url.clone();
        let resp = self.transport.send(req).await?.ensure_success(&req_url)?;
        // `reveried /context/smart` may return structured JSON or raw markdown text.
        if let Ok(structured) = resp.decode_json::<ReverieSmartContext>("decode /context/smart") {
            return Ok(structured);
        }
        Ok(ReverieSmartContext {
            project: project.unwrap_or(&self.config.default_project).to_owned(),
            context: String::from_utf8_lossy(&resp.body).into_owned(),
            observations: Vec::new(),
        })
    }

    /// Executes `POST /observations` for `payload`.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] on transport, HTTP status, or JSON decode failure.
    pub async fn save_observation(
        &self,
        payload: &ReverieObservationPayload,
    ) -> Result<ReverieSavedReceipt, AdapterError> {
        let req = self.build_observation_request(payload)?;
        let req_url = req.url.clone();
        let resp = self.transport.send(req).await?.ensure_success(&req_url)?;
        if resp.body.is_empty() {
            return Ok(ReverieSavedReceipt {
                id: None,
                topic_key: Some(payload.topic_key.clone()),
            });
        }
        resp.decode_json("decode Reverie /observations receipt")
    }

    async fn apply_memory_with_sync(
        &self,
        scope: &MemoryScope,
        op: MemoryOp,
        key: &EditKey,
        labels: &BTreeSet<String>,
    ) -> Result<MemoryBlock, AgentError> {
        let updated_block = match self.local.preview_memory_edit(scope, &op, key, labels)? {
            MemoryEditPreview::Cached(outcome) => return outcome,
            MemoryEditPreview::FailedNew(err) => return Err(err),
            MemoryEditPreview::ReadyToSync { updated_block } => updated_block,
        };

        let topic_key = format!(
            "littleguy/memory/{}/{}",
            sanitize_topic_segment(scope.as_str()),
            sanitize_topic_segment(op.block())
        );
        let title = format!(
            "littleguy memory block `{}` ({})",
            op.block(),
            scope.as_str()
        );
        let content = if updated_block.entries.is_empty() {
            String::from("(empty memory block)")
        } else {
            updated_block.entries.join("\n")
        };

        let all_block_labels =
            autumn_harvest_agent::memory::snapshot_labels(std::slice::from_ref(&updated_block));
        let mut builder =
            ReverieObservationBuilder::new(&self.config.default_project, topic_key, title, content)
                .scope(self.config.default_scope)
                .kind(ReverieObservationKind::Manual)
                .write_mode(ReverieWriteMode::Upsert)
                .evidence_link(format!("edit_key:{}", key.as_str()))
                .flow_labels(&all_block_labels)
                .map_err(AdapterError::into_agent_error)?
                .tag("Scope", scope.as_str())
                .map_err(AdapterError::into_agent_error)?;

        if let Some(agent_id) = &self.config.agent_id {
            builder = builder.agent_id(agent_id.clone());
        }
        if let Some(subject) = &self.config.subject {
            builder = builder
                .tag("Subject", subject.clone())
                .map_err(AdapterError::into_agent_error)?;
        }

        let payload = builder.build().map_err(AdapterError::into_agent_error)?;
        self.save_observation(&payload)
            .await
            .map_err(AdapterError::into_agent_error)?;

        self.local.apply_now(scope, &op, key, labels)
    }
}

impl<T: HttpTransport> MemoryStore for ReverieObservationStore<T> {
    fn load<'a>(
        &'a self,
        scope: &'a MemoryScope,
    ) -> BoxFuture<'a, Result<Vec<MemoryBlock>, AgentError>> {
        MemoryStore::load(&self.local, scope)
    }

    fn apply<'a>(
        &'a self,
        scope: &'a MemoryScope,
        op: MemoryOp,
        key: &'a EditKey,
    ) -> BoxFuture<'a, Result<MemoryBlock, AgentError>> {
        Box::pin(async move {
            self.apply_memory_with_sync(scope, op, key, &BTreeSet::new())
                .await
        })
    }

    fn apply_labelled<'a>(
        &'a self,
        scope: &'a MemoryScope,
        op: MemoryOp,
        key: &'a EditKey,
        labels: &'a BTreeSet<String>,
    ) -> BoxFuture<'a, Result<MemoryBlock, AgentError>> {
        Box::pin(async move { self.apply_memory_with_sync(scope, op, key, labels).await })
    }

    fn clear<'a>(&'a self, scope: &'a MemoryScope) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(async move {
            let topic_key = format!(
                "littleguy/memory/{}/cleared",
                sanitize_topic_segment(scope.as_str())
            );
            let payload = ReverieObservationBuilder::new(
                &self.config.default_project,
                topic_key,
                format!("cleared memory scope `{}`", scope.as_str()),
                format!("Reset all blocks in memory scope `{}`.", scope.as_str()),
            )
            .scope(self.config.default_scope)
            .kind(ReverieObservationKind::Manual)
            .write_mode(ReverieWriteMode::Append)
            .build()
            .map_err(AdapterError::into_agent_error)?;
            self.save_observation(&payload)
                .await
                .map_err(AdapterError::into_agent_error)?;
            MemoryStore::clear(&self.local, scope).await
        })
    }
}

impl<T: HttpTransport> EpisodeStore for ReverieObservationStore<T> {
    fn load<'a>(&'a self, scope: &'a MemoryScope) -> BoxFuture<'a, Result<EpisodeLog, AgentError>> {
        EpisodeStore::load(&self.local, scope)
    }

    fn append<'a>(
        &'a self,
        scope: &'a MemoryScope,
        key: &'a EditKey,
        episode: Episode,
        max_episodes: u32,
    ) -> BoxFuture<'a, Result<Appended, AgentError>> {
        Box::pin(async move {
            match self
                .local
                .preview_episode_append(scope, key, &episode, max_episodes)?
            {
                EpisodeAppendPreview::Cached(appended) => return Ok(appended),
                EpisodeAppendPreview::ReadyToSync => {}
            }

            let topic_key = format!(
                "littleguy/episodes/{}",
                sanitize_topic_segment(scope.as_str())
            );
            let body = if episode.text.trim().is_empty() {
                format!("episode `{}` ended ({:?})", episode.id, episode.stop)
            } else {
                episode.text.clone()
            };
            let mut builder = ReverieObservationBuilder::new(
                &self.config.default_project,
                topic_key,
                format!("episode `{}` ({})", episode.id, scope.as_str()),
                body,
            )
            .scope(self.config.default_scope)
            .kind(ReverieObservationKind::Learning)
            .write_mode(ReverieWriteMode::Append)
            .evidence_link(format!("episode:{}", episode.id))
            .evidence_link(format!("edit_key:{}", key.as_str()))
            .flow_labels(&episode.labels)
            .map_err(AdapterError::into_agent_error)?;

            if let Some(agent_id) = &self.config.agent_id {
                builder = builder.agent_id(agent_id.clone());
            }

            let payload = builder.build().map_err(AdapterError::into_agent_error)?;
            self.save_observation(&payload)
                .await
                .map_err(AdapterError::into_agent_error)?;

            EpisodeStore::append(&self.local, scope, key, episode, max_episodes).await
        })
    }

    fn mark<'a>(
        &'a self,
        scope: &'a MemoryScope,
        mark: PassMark,
    ) -> BoxFuture<'a, Result<bool, AgentError>> {
        Box::pin(async move {
            match self.local.preview_episode_mark(scope, &mark)? {
                EpisodeMarkPreview::StaleGeneration => return Ok(false),
                EpisodeMarkPreview::CachedTrue => return Ok(true),
                EpisodeMarkPreview::ReadyToSync => {}
            }

            let topic_key = format!(
                "littleguy/consolidation/{}",
                sanitize_topic_segment(scope.as_str())
            );
            let content = if mark.facts.is_empty() {
                format!(
                    "Consolidated {} episodes in generation {}.",
                    mark.episodes.len(),
                    mark.generation
                )
            } else {
                mark.facts.join("\n")
            };
            let payload = ReverieObservationBuilder::new(
                &self.config.default_project,
                topic_key,
                format!(
                    "consolidation pass `{}` ({})",
                    mark.key.as_str(),
                    scope.as_str()
                ),
                content,
            )
            .scope(self.config.default_scope)
            .kind(ReverieObservationKind::Learning)
            .write_mode(ReverieWriteMode::Append)
            .evidence_link(format!("pass_key:{}", mark.key.as_str()))
            .build()
            .map_err(AdapterError::into_agent_error)?;

            self.save_observation(&payload)
                .await
                .map_err(AdapterError::into_agent_error)?;

            EpisodeStore::mark(&self.local, scope, mark).await
        })
    }

    fn clear<'a>(
        &'a self,
        scope: &'a MemoryScope,
        key: &'a EditKey,
    ) -> BoxFuture<'a, Result<Cleared, AgentError>> {
        Box::pin(async move {
            if let EpisodeClearPreview::Cached(cleared) =
                self.local.preview_episode_clear(scope, key)?
            {
                return Ok(cleared);
            }

            let topic_key = format!(
                "littleguy/episodes/{}/cleared",
                sanitize_topic_segment(scope.as_str())
            );
            let payload = ReverieObservationBuilder::new(
                &self.config.default_project,
                topic_key,
                format!("episode clear `{}` ({})", key.as_str(), scope.as_str()),
                format!("Cleared episode log for `{}`.", scope.as_str()),
            )
            .scope(self.config.default_scope)
            .kind(ReverieObservationKind::Manual)
            .write_mode(ReverieWriteMode::Append)
            .evidence_link(format!("clear_key:{}", key.as_str()))
            .build()
            .map_err(AdapterError::into_agent_error)?;

            self.save_observation(&payload)
                .await
                .map_err(AdapterError::into_agent_error)?;

            EpisodeStore::clear(&self.local, scope, key).await
        })
    }
}
