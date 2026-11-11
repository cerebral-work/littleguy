//! Cygnus XERJ (`:9200`) hybrid search, `_meta.commit_sha` watermark, and durable
//! `MemoryStore` / `EpisodeStore` adapter (`specs/0003`, `REQ-LG-XERJ-01..04`).
//!
//! Targets the Cygnus cluster deployment (`http://xerj.tail769bd2.ts.net:9200`,
//! `XERJ_EMBED_MODE=proxy` by default) with zero ad-hoc URL or JSON string
//! interpolation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use autumn_harvest_agent::AgentError;
use autumn_harvest_agent::episode::{
    Appended, Cleared, Episode, EpisodeLog, EpisodeStore, PassMark,
};
use autumn_harvest_agent::memory::{EditKey, MemoryBlock, MemoryOp, MemoryScope, MemoryStore};
use autumn_harvest_agent::model::BoxFuture;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::transport::{
    AdapterError, DEFAULT_HTTP_TIMEOUT, HttpMethod, HttpTransport, StoreHttpRequest,
    join_url_segments, parse_clean_base_url,
};
use crate::{
    EpisodeAppendPreview, EpisodeClearPreview, EpisodeMarkPreview, MemoryEditPreview, SqliteStores,
};

/// Canonical Cygnus cluster XERJ endpoint (`:9200`).
pub const CYGNUS_XERJ_DEFAULT_URL: &str = "http://xerj.tail769bd2.ts.net:9200";

/// Minimum response token budget accepted by XERJ (`max_tokens >= 32`).
pub const MIN_XERJ_MAX_TOKENS: u32 = 32;

/// Default passage field requested from XERJ `_search` (~34x smaller than full `_source`).
pub const DEFAULT_PASSAGE_FIELD: &str = "_passage";

/// Embedding execution mode configured for XERJ (`XERJ_EMBED_MODE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XerjEmbedMode {
    /// Proxy embeddings on Cygnus (`proxy`, estate default).
    #[default]
    Proxy,
    /// Built-in 384-d lexical feature hashing (`lexical`).
    Lexical,
    /// Dedicated GPU/cluster neural embedder (`neural`).
    Neural,
}

impl XerjEmbedMode {
    /// Canonical wire string for `XERJ_EMBED_MODE`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Lexical => "lexical",
            Self::Neural => "neural",
        }
    }
}

/// Rank fusion strategy for XERJ hybrid queries (`rrf` vs `linear`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XerjFusionStrategy {
    /// Reciprocal rank fusion (`rrf`, default).
    #[default]
    Rrf,
    /// Weighted linear score combination (`linear`).
    Linear,
}

impl XerjFusionStrategy {
    /// Canonical wire string (`"rrf"` or `"linear"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rrf => "rrf",
            Self::Linear => "linear",
        }
    }
}

/// Validated client configuration for [`XerjHybridSearchStore`] (`REQ-LG-XERJ-01`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XerjClientConfig {
    base_url: Url,
    memory_namespace: String,
    embed_mode: XerjEmbedMode,
    timeout: Duration,
}

impl XerjClientConfig {
    /// Validates `base_url` and `memory_namespace`, rejecting loopback `:9200`
    /// endpoints (`127.0.0.1`, `localhost`, `::1`) so shared hosts (`ceres`)
    /// never target or spawn a local neural XERJ daemon (`REQ-LG-XERJ-01`).
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidConfig`] if `base_url` is malformed, targets
    /// loopback `:9200`, or `memory_namespace` is invalid.
    pub fn new(base_url: &str, memory_namespace: impl Into<String>) -> Result<Self, AdapterError> {
        let ns = memory_namespace.into();
        Self::build_internal(base_url, &ns, false)
    }

    /// Constructs a [`XerjClientConfig`] pointing at [`CYGNUS_XERJ_DEFAULT_URL`]
    /// with [`XerjEmbedMode::Proxy`].
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidConfig`] if `memory_namespace` is invalid.
    pub fn cygnus_default(memory_namespace: impl Into<String>) -> Result<Self, AdapterError> {
        let ns = memory_namespace.into();
        Self::build_internal(CYGNUS_XERJ_DEFAULT_URL, &ns, false)
    }

    /// Constructs a [`XerjClientConfig`] explicitly allowing a loopback URL for
    /// isolated single-tenant harnesses.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::InvalidConfig`] if `base_url` or `memory_namespace` is invalid.
    pub fn new_allowing_loopback(
        base_url: &str,
        memory_namespace: impl Into<String>,
    ) -> Result<Self, AdapterError> {
        let ns = memory_namespace.into();
        Self::build_internal(base_url, &ns, true)
    }

    fn build_internal(
        base_url: &str,
        memory_namespace: &str,
        allow_loopback: bool,
    ) -> Result<Self, AdapterError> {
        let parsed = parse_clean_base_url(base_url, "xerj.base_url")?;
        if !allow_loopback && is_loopback_xerj_host(&parsed) {
            return Err(AdapterError::InvalidConfig {
                field: "xerj.base_url",
                reason: format!(
                    "loopback XERJ endpoint `{parsed}` is forbidden on shared hosts; use `{CYGNUS_XERJ_DEFAULT_URL}`"
                ),
            });
        }
        let ns = validate_xerj_identifier(memory_namespace, "memory_namespace", false)?;
        Ok(Self {
            base_url: parsed,
            memory_namespace: ns,
            embed_mode: XerjEmbedMode::Proxy,
            timeout: DEFAULT_HTTP_TIMEOUT,
        })
    }

    /// Sets [`XerjEmbedMode`].
    #[must_use]
    pub const fn with_embed_mode(mut self, embed_mode: XerjEmbedMode) -> Self {
        self.embed_mode = embed_mode;
        self
    }

    /// Sets per-request HTTP timeout.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Validated base URL.
    #[must_use]
    pub const fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Validated `/_memory/{namespace}` namespace.
    #[must_use]
    pub fn memory_namespace(&self) -> &str {
        &self.memory_namespace
    }

    /// Configured [`XerjEmbedMode`].
    #[must_use]
    pub const fn embed_mode(&self) -> XerjEmbedMode {
        self.embed_mode
    }
}

fn is_loopback_xerj_host(url: &Url) -> bool {
    match url.host_str() {
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1") => true,
        Some(other) => other.starts_with("127."),
        None => false,
    }
}

/// Validates a XERJ index pattern (`allow_wildcard = true`) or concrete index /
/// namespace / field name (`allow_wildcard = false`).
///
/// # Errors
///
/// Returns [`AdapterError::InvalidIdentifier`] on empty values, `.` / `..`,
/// slashes, query characters, or characters outside `[a-z0-9._-]` (plus `*` when
/// `allow_wildcard` is true).
pub fn validate_xerj_identifier(
    raw: &str,
    kind: &'static str,
    allow_wildcard: bool,
) -> Result<String, AdapterError> {
    if raw.is_empty() || raw == "." || raw == ".." || raw.contains("..") {
        return Err(AdapterError::InvalidIdentifier {
            kind,
            value: raw.to_owned(),
            reason: "identifier must be non-empty and must not contain `.` or `..`".to_owned(),
        });
    }
    let valid = raw.bytes().all(|b| {
        matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-') || (allow_wildcard && b == b'*')
    });
    if !valid {
        return Err(AdapterError::InvalidIdentifier {
            kind,
            value: raw.to_owned(),
            reason: "identifier contains forbidden characters (expected lowercase `[a-z0-9._-]`)"
                .to_owned(),
        });
    }
    Ok(raw.to_owned())
}

/// Validates a lowercase hexadecimal git commit SHA (`40` or `64` hex digits).
///
/// # Errors
///
/// Returns [`AdapterError::InvalidIdentifier`] when `sha` is not 40 or 64 lowercase hex digits.
pub fn validate_commit_sha(sha: &str) -> Result<String, AdapterError> {
    let valid_len = sha.len() == 40 || sha.len() == 64;
    let valid_hex = sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !valid_len || !valid_hex {
        return Err(AdapterError::InvalidIdentifier {
            kind: "commit_sha",
            value: sha.to_owned(),
            reason: "commit_sha must be 40 or 64 lowercase hexadecimal characters".to_owned(),
        });
    }
    Ok(sha.to_owned())
}

/// Validates a 64-character lowercase hexadecimal SHA-256 content hash.
///
/// # Errors
///
/// Returns [`AdapterError::InvalidIdentifier`] when `digest` is not 64 lowercase hex digits.
pub fn validate_content_hash(digest: &str) -> Result<String, AdapterError> {
    let valid = digest.len() == 64
        && digest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !valid {
        return Err(AdapterError::InvalidIdentifier {
            kind: "content_hash",
            value: digest.to_owned(),
            reason: "content_hash must be 64 lowercase hexadecimal SHA-256 characters".to_owned(),
        });
    }
    Ok(digest.to_owned())
}

/// Computes an order-independent, canonical 64-hex SHA-256 content hash over a
/// set of `(logical_path, raw_bytes)` corpus documents (`REQ-LG-XERJ-03`).
#[must_use]
pub fn compute_corpus_content_hash(documents: &[(&str, &[u8])]) -> String {
    let mut per_doc: Vec<(&str, [u8; 32])> = documents
        .iter()
        .map(|(path, bytes)| {
            let doc_digest: [u8; 32] = Sha256::digest(bytes).into();
            (*path, doc_digest)
        })
        .collect();
    per_doc.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(&b.1)));

    let mut root = Sha256::new();
    root.update(b"unsigned.gg/xerj/corpus-watermark/v1\0");
    let count = per_doc.len() as u64;
    root.update(count.to_le_bytes());
    for (path, doc_digest) in per_doc {
        let path_len = path.len() as u64;
        root.update(path_len.to_le_bytes());
        root.update(path.as_bytes());
        root.update(doc_digest);
    }
    let final_bytes = root.finalize();
    let mut hex_out = String::with_capacity(64);
    for byte in final_bytes {
        let _ = write!(hex_out, "{byte:02x}");
    }
    hex_out
}

/// Local second-stage judge configuration for XERJ (`judge` block, zero off-node egress).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct XerjLocalJudge {
    /// Required natural-language question used by XERJ's in-process judge.
    pub query: String,
    /// Optional relevance probability cut (`0.0..=1.0`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f64>,
}

/// Validated BM25 + `semantic` hybrid search specification (`REQ-LG-XERJ-02`).
#[derive(Debug, Clone, PartialEq)]
pub struct XerjHybridQuery {
    index: String,
    query_text: String,
    lexical_field: String,
    semantic_field: String,
    fusion: XerjFusionStrategy,
    lexical_weight: Option<f64>,
    semantic_weight: Option<f64>,
    semantic_k: u32,
    size: u32,
    fields: Vec<String>,
    source_fields: Vec<String>,
    max_tokens: Option<u32>,
    term_filters: BTreeMap<String, String>,
    judge: Option<XerjLocalJudge>,
}

impl XerjHybridQuery {
    /// Target index or wildcard pattern (for example `"soma-*"` or `"unsigned-paas-*"`).
    #[must_use]
    pub fn index(&self) -> &str {
        &self.index
    }

    /// Serializes this query into the exact JSON body sent to `POST /{index}/_search`.
    #[must_use]
    pub fn to_wire_body(&self) -> serde_json::Value {
        let filter_clause = if self.term_filters.is_empty() {
            None
        } else {
            let terms: Vec<serde_json::Value> = self
                .term_filters
                .iter()
                .map(|(field, val)| serde_json::json!({ "term": { field: val } }))
                .collect();
            Some(serde_json::json!({ "bool": { "filter": terms } }))
        };

        let mut lexical_map = serde_json::Map::new();
        lexical_map.insert(
            self.lexical_field.clone(),
            serde_json::Value::String(self.query_text.clone()),
        );
        let match_clause = serde_json::json!({ "match": lexical_map });
        let lexical_subquery = match &filter_clause {
            Some(filter) => serde_json::json!({
                "bool": {
                    "must": [match_clause],
                    "filter": [filter],
                }
            }),
            None => match_clause,
        };

        let mut semantic_inner = serde_json::Map::new();
        semantic_inner.insert(
            "field".to_owned(),
            serde_json::Value::String(self.semantic_field.clone()),
        );
        semantic_inner.insert(
            "query".to_owned(),
            serde_json::Value::String(self.query_text.clone()),
        );
        semantic_inner.insert("k".to_owned(), serde_json::Value::from(self.semantic_k));
        if let Some(filter) = filter_clause {
            semantic_inner.insert("filter".to_owned(), filter);
        }
        let semantic_subquery = serde_json::json!({ "semantic": semantic_inner });

        let mut q1 = serde_json::Map::new();
        q1.insert("query".to_owned(), lexical_subquery);
        if let Some(weight) = self.lexical_weight {
            q1.insert("weight".to_owned(), serde_json::json!(weight));
        }

        let mut q2 = serde_json::Map::new();
        q2.insert("query".to_owned(), semantic_subquery);
        if let Some(weight) = self.semantic_weight {
            q2.insert("weight".to_owned(), serde_json::json!(weight));
        }

        let mut root = serde_json::Map::new();
        root.insert(
            "query".to_owned(),
            serde_json::json!({
                "hybrid": {
                    "queries": [q1, q2],
                    "fusion": self.fusion.as_str(),
                }
            }),
        );
        root.insert("fields".to_owned(), serde_json::json!(self.fields));
        root.insert("_source".to_owned(), serde_json::json!(self.source_fields));
        root.insert("size".to_owned(), serde_json::Value::from(self.size));
        if let Some(max_tokens) = self.max_tokens {
            root.insert("max_tokens".to_owned(), serde_json::Value::from(max_tokens));
        }
        if let Some(judge) = &self.judge {
            root.insert(
                "judge".to_owned(),
                serde_json::to_value(judge).unwrap_or(serde_json::Value::Null),
            );
        }
        serde_json::Value::Object(root)
    }
}

/// Typed builder for [`XerjHybridQuery`] with zero string interpolation (`REQ-LG-XERJ-02`).
#[derive(Debug, Clone)]
pub struct XerjHybridQueryBuilder {
    index: String,
    query_text: String,
    lexical_field: String,
    semantic_field: String,
    fusion: XerjFusionStrategy,
    lexical_weight: Option<f64>,
    semantic_weight: Option<f64>,
    semantic_k: u32,
    size: u32,
    fields: Vec<String>,
    source_fields: Vec<String>,
    max_tokens: Option<u32>,
    term_filters: BTreeMap<String, String>,
    judge: Option<XerjLocalJudge>,
}

impl XerjHybridQueryBuilder {
    /// Starts building a hybrid BM25 + `semantic` query against `index` for `query_text`.
    #[must_use]
    pub fn new(index: impl Into<String>, query_text: impl Into<String>) -> Self {
        Self {
            index: index.into(),
            query_text: query_text.into(),
            lexical_field: "body".to_owned(),
            semantic_field: "semantic_body".to_owned(),
            fusion: XerjFusionStrategy::Rrf,
            lexical_weight: None,
            semantic_weight: None,
            semantic_k: 10,
            size: 10,
            fields: vec![DEFAULT_PASSAGE_FIELD.to_owned()],
            source_fields: vec!["ax_path".to_owned(), "symbol".to_owned(), "kind".to_owned()],
            max_tokens: None,
            term_filters: BTreeMap::new(),
            judge: None,
        }
    }

    /// Overrides the BM25 `match` field name (default `"body"`).
    #[must_use]
    pub fn lexical_field(mut self, field: impl Into<String>) -> Self {
        self.lexical_field = field.into();
        self
    }

    /// Overrides the `semantic_text` field name (default `"semantic_body"`).
    #[must_use]
    pub fn semantic_field(mut self, field: impl Into<String>) -> Self {
        self.semantic_field = field.into();
        self
    }

    /// Sets the rank fusion strategy (`rrf` or `linear`).
    #[must_use]
    pub const fn fusion(mut self, fusion: XerjFusionStrategy) -> Self {
        self.fusion = fusion;
        self
    }

    /// Sets explicit sub-query fusion weights (`lexical_weight`, `semantic_weight`).
    #[must_use]
    pub const fn weights(mut self, lexical_weight: f64, semantic_weight: f64) -> Self {
        self.lexical_weight = Some(lexical_weight);
        self.semantic_weight = Some(semantic_weight);
        self
    }

    /// Sets `k` on the `semantic` nearest-neighbor clause and overall `size`.
    #[must_use]
    pub const fn limits(mut self, semantic_k: u32, size: u32) -> Self {
        self.semantic_k = semantic_k;
        self.size = size;
        self
    }

    /// Replaces the `_source` field projection list.
    #[must_use]
    pub fn source_fields<I, S>(mut self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.source_fields = fields.into_iter().map(Into::into).collect();
        self
    }

    /// Caps the response token budget (`>= 32`).
    #[must_use]
    pub const fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Adds an exact metadata `term` pre-filter applied to both hybrid clauses.
    #[must_use]
    pub fn term_filter(mut self, field: impl Into<String>, value: impl Into<String>) -> Self {
        self.term_filters.insert(field.into(), value.into());
        self
    }

    /// Enables XERJ's local zero-egress relevance judge (`judge`).
    #[must_use]
    pub fn local_judge(mut self, query: impl Into<String>, min_p: Option<f64>) -> Self {
        self.judge = Some(XerjLocalJudge {
            query: query.into(),
            min_p,
        });
        self
    }

    /// Validates all identifiers, weights, and bounds, returning [`XerjHybridQuery`].
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] if `index`, field names, `query_text`, weights,
    /// `max_tokens` (`< 32`), or `judge` parameters fail validation.
    pub fn build(self) -> Result<XerjHybridQuery, AdapterError> {
        let index = validate_xerj_identifier(&self.index, "index", true)?;
        let query_text = self.query_text.trim().to_owned();
        if query_text.is_empty() {
            return Err(AdapterError::InvalidIdentifier {
                kind: "query_text",
                value: self.query_text,
                reason: "hybrid search query_text must not be empty".to_owned(),
            });
        }
        let lexical_field = verify_field_name(&self.lexical_field, "lexical_field")?;
        let semantic_field = verify_field_name(&self.semantic_field, "semantic_field")?;

        if self.semantic_k == 0 || self.size == 0 {
            return Err(AdapterError::InvalidIdentifier {
                kind: "limits",
                value: format!("k={}, size={}", self.semantic_k, self.size),
                reason: "semantic_k and size must both be >= 1".to_owned(),
            });
        }

        for (label, weight) in [
            ("lexical_weight", self.lexical_weight),
            ("semantic_weight", self.semantic_weight),
        ] {
            if let Some(w) = weight
                && (!w.is_finite() || w <= 0.0)
            {
                return Err(AdapterError::InvalidIdentifier {
                    kind: label,
                    value: w.to_string(),
                    reason: "hybrid query weight must be finite and > 0.0".to_owned(),
                });
            }
        }

        if let Some(tokens) = self.max_tokens
            && tokens < MIN_XERJ_MAX_TOKENS
        {
            return Err(AdapterError::InvalidIdentifier {
                kind: "max_tokens",
                value: tokens.to_string(),
                reason: format!("XERJ requires max_tokens >= {MIN_XERJ_MAX_TOKENS}"),
            });
        }

        let mut clean_filters = BTreeMap::new();
        for (field, value) in self.term_filters {
            let clean_field = verify_field_name(&field, "filter_field")?;
            if value.chars().any(char::is_control) {
                return Err(AdapterError::InvalidIdentifier {
                    kind: "filter_value",
                    value,
                    reason: "filter value must not contain control characters".to_owned(),
                });
            }
            clean_filters.insert(clean_field, value);
        }

        let clean_judge = match self.judge {
            Some(j) => {
                let q = j.query.trim().to_owned();
                if q.is_empty() {
                    return Err(AdapterError::InvalidIdentifier {
                        kind: "judge.query",
                        value: j.query,
                        reason: "local judge requires a non-empty `query`".to_owned(),
                    });
                }
                if let Some(p) = j.min_p
                    && (!p.is_finite() || !(0.0..=1.0).contains(&p))
                {
                    return Err(AdapterError::InvalidIdentifier {
                        kind: "judge.min_p",
                        value: p.to_string(),
                        reason: "judge.min_p must lie in 0.0..=1.0".to_owned(),
                    });
                }
                Some(XerjLocalJudge {
                    query: q,
                    min_p: j.min_p,
                })
            }
            None => None,
        };

        Ok(XerjHybridQuery {
            index,
            query_text,
            lexical_field,
            semantic_field,
            fusion: self.fusion,
            lexical_weight: self.lexical_weight,
            semantic_weight: self.semantic_weight,
            semantic_k: self.semantic_k,
            size: self.size,
            fields: self.fields,
            source_fields: self.source_fields,
            max_tokens: self.max_tokens,
            term_filters: clean_filters,
            judge: clean_judge,
        })
    }
}

fn verify_field_name(raw: &str, kind: &'static str) -> Result<String, AdapterError> {
    if raw.is_empty()
        || !raw
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err(AdapterError::InvalidIdentifier {
            kind,
            value: raw.to_owned(),
            reason: "field name must match `[A-Za-z0-9._-]+`".to_owned(),
        });
    }
    Ok(raw.to_owned())
}

/// One fused hit returned by XERJ `POST /{index}/_search`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct XerjSearchHit {
    /// Concrete backing index name.
    #[serde(rename = "_index")]
    pub index: String,
    /// Document id.
    #[serde(rename = "_id")]
    pub id: String,
    /// Fused engine score.
    #[serde(rename = "_score")]
    pub score: Option<f64>,
    /// Optional local judge relevance probability (`0.0..=1.0`).
    #[serde(rename = "_p_relevant")]
    pub p_relevant: Option<f64>,
    /// Projected `_source` fields (for example `ax_path`, `symbol`, `kind`).
    #[serde(rename = "_source")]
    pub source: serde_json::Value,
    /// Extracted passage snippets (`fields._passage`).
    pub fields: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
struct XerjHitsBlock {
    hits: Vec<XerjSearchHit>,
}

/// Decoded response from XERJ `POST /{index}/_search`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct XerjSearchResponse {
    #[serde(rename = "hits")]
    hits_block: XerjHitsBlock,
}

impl XerjSearchResponse {
    /// Ranked hits returned by XERJ.
    #[must_use]
    pub fn hits(&self) -> &[XerjSearchHit] {
        &self.hits_block.hits
    }
}

/// Synchronized corpus/index watermark pinning both git `commit_sha` and
/// canonical SHA-256 `content_hash` (`REQ-LG-XERJ-03`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitWatermark {
    /// Concrete XERJ index name.
    pub index_name: String,
    /// Validated 40- or 64-character lowercase hex git commit SHA.
    pub commit_sha: String,
    /// Validated 64-character lowercase hex SHA-256 corpus content digest.
    pub content_hash: String,
    /// Epoch milliseconds when the watermark was recorded.
    pub updated_at_ms: u64,
}

/// Why an index failed [`XerjHybridSearchStore::evaluate_watermark`] freshness checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatermarkDriftReason {
    /// Remote index does not exist (`404`) or carries no `mappings._meta.commit_sha`.
    MissingRemoteMeta,
    /// Local durable SQLite store has no recorded watermark for this index.
    MissingLocalWatermark,
    /// Remote or local `commit_sha` differs from the target commit.
    CommitShaChanged,
    /// Target SHA-256 `content_hash` drifted despite matching `commit_sha` (dirty tree).
    ContentHashChanged,
    /// Local SQLite watermark and remote index `_meta` disagree.
    LocalRemoteMismatch,
}

/// Result of comparing target `(commit_sha, content_hash)` against local SQLite
/// state and remote XERJ `mappings._meta` (`REQ-LG-XERJ-03`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatermarkSyncDecision {
    /// Both local SQLite and remote XERJ `_meta` match the target commit and content hash.
    UpToDate {
        /// Confirmed watermark.
        watermark: CommitWatermark,
    },
    /// Index must be synchronized before advancing `_meta.commit_sha`.
    NeedsSync {
        /// Previously recorded commit SHA, if any.
        previous_commit_sha: Option<String>,
        /// Previously recorded content hash, if any.
        previous_content_hash: Option<String>,
        /// Target git commit SHA.
        target_commit_sha: String,
        /// Target corpus content hash.
        target_content_hash: String,
        /// Exact drift cause.
        reason: WatermarkDriftReason,
    },
}

/// Remote `_meta` block stored inside `GET/PUT /{index}/_mapping`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct XerjMappingMeta {
    /// Pinned git commit SHA (`_meta.commit_sha`).
    pub commit_sha: Option<String>,
    /// Pinned SHA-256 corpus content hash (`_meta.content_hash`).
    pub content_hash: Option<String>,
    /// Last sync timestamp (`_meta.updated_at_ms`).
    pub updated_at_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct PutMappingBody<'a> {
    #[serde(rename = "_meta")]
    meta: &'a XerjMappingMeta,
}

#[derive(Debug, Serialize)]
struct XerjMemoryWriteBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a str>,
    text: &'a str,
    dedup: bool,
    metadata: serde_json::Value,
}

/// Request body for `POST /_memory/{namespace}/_recall`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct XerjMemoryRecallQuery {
    /// Recall query string.
    pub query: String,
    /// Enable fused BM25 + server-side `semantic` recall.
    pub hybrid: bool,
    /// Fusion strategy (`rrf` or `linear`).
    pub fusion: XerjFusionStrategy,
    /// Number of recalled memories (`>= 1`).
    pub k: u32,
}

impl XerjMemoryRecallQuery {
    /// Builds a hybrid RRF recall query for `query` returning up to `k` memories.
    #[must_use]
    pub fn hybrid_rrf(query: impl Into<String>, k: u32) -> Self {
        Self {
            query: query.into(),
            hybrid: true,
            fusion: XerjFusionStrategy::Rrf,
            k: k.max(1),
        }
    }
}

/// One recalled agent memory entry from `POST /_memory/{namespace}/_recall`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct XerjMemoryRecallHit {
    /// Memory entry identifier.
    pub id: String,
    /// Stored text.
    pub text: String,
    /// Optional relevance score.
    pub score: Option<f64>,
    /// Structured metadata (including `flow_labels`).
    pub metadata: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum XerjRecallEnvelope {
    Wrapped { hits: Vec<XerjMemoryRecallHit> },
    Memories { memories: Vec<XerjMemoryRecallHit> },
    Direct(Vec<XerjMemoryRecallHit>),
}

/// Durable Cygnus XERJ hybrid search, `_meta.commit_sha` watermark, and
/// `MemoryStore` / `EpisodeStore` adapter (`REQ-LG-XERJ-01..04`).
///
/// Its `Debug` output omits stored memory entries and episodes.
pub struct XerjHybridSearchStore<T: HttpTransport> {
    config: XerjClientConfig,
    local: SqliteStores,
    transport: T,
}

impl<T: HttpTransport> fmt::Debug for XerjHybridSearchStore<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XerjHybridSearchStore")
            .field("base_url", &self.config.base_url.as_str())
            .field("memory_namespace", &self.config.memory_namespace)
            .field("embed_mode", &self.config.embed_mode)
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport> XerjHybridSearchStore<T> {
    /// Opens a durable [`XerjHybridSearchStore`] backed by SQLite at `db_path`.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Store`] if SQLite fails to open or migrate.
    pub fn open(
        db_path: impl AsRef<Path>,
        config: XerjClientConfig,
        transport: T,
    ) -> Result<Self, AdapterError> {
        let local = SqliteStores::open(db_path)?;
        Ok(Self {
            config,
            local,
            transport,
        })
    }

    /// Opens an in-memory SQLite-backed [`XerjHybridSearchStore`] for offline tests.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Store`] if SQLite cannot open an in-memory database.
    pub fn open_in_memory(config: XerjClientConfig, transport: T) -> Result<Self, AdapterError> {
        let local = SqliteStores::open_in_memory()?;
        Ok(Self {
            config,
            local,
            transport,
        })
    }

    /// Validated [`XerjClientConfig`].
    #[must_use]
    pub const fn config(&self) -> &XerjClientConfig {
        &self.config
    }

    /// Reference to the underlying [`HttpTransport`].
    #[must_use]
    pub const fn transport(&self) -> &T {
        &self.transport
    }

    /// Builds a zero-interpolation `POST /{index}/_search` request for `query`.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] if URL assembly or JSON serialization fails.
    pub fn build_hybrid_search_request(
        &self,
        query: &XerjHybridQuery,
    ) -> Result<StoreHttpRequest, AdapterError> {
        let url = join_url_segments(&self.config.base_url, &[query.index(), "_search"])?;
        StoreHttpRequest::new(HttpMethod::Post, url)
            .with_timeout(self.config.timeout)
            .with_header("X-Xerj-Embed-Mode", self.config.embed_mode.as_str())
            .with_json_body(&query.to_wire_body())
    }

    /// Executes a hybrid BM25 + `semantic` search on XERJ (`POST /{index}/_search`).
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] on transport, HTTP status, or JSON decode failure.
    pub async fn hybrid_search(
        &self,
        query: &XerjHybridQuery,
    ) -> Result<XerjSearchResponse, AdapterError> {
        let req = self.build_hybrid_search_request(query)?;
        let req_url = req.url.clone();
        let resp = self.transport.send(req).await?.ensure_success(&req_url)?;
        resp.decode_json("decode XERJ _search response")
    }

    /// Builds `GET /{index}/_mapping` to inspect remote `_meta.commit_sha`.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] if `index` is not a concrete index identifier.
    pub fn build_get_mapping_request(&self, index: &str) -> Result<StoreHttpRequest, AdapterError> {
        let clean = validate_xerj_identifier(index, "index", false)?;
        let url = join_url_segments(&self.config.base_url, &[&clean, "_mapping"])?;
        Ok(StoreHttpRequest::new(HttpMethod::Get, url).with_timeout(self.config.timeout))
    }

    /// Builds `PUT /{index}/_mapping` updating `_meta.commit_sha` and `_meta.content_hash`.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] if `index`, `commit_sha`, or `content_hash` is invalid.
    pub fn build_put_watermark_request(
        &self,
        index: &str,
        commit_sha: &str,
        content_hash: &str,
        updated_at_ms: u64,
    ) -> Result<StoreHttpRequest, AdapterError> {
        let clean_index = validate_xerj_identifier(index, "index", false)?;
        let clean_sha = validate_commit_sha(commit_sha)?;
        let clean_hash = validate_content_hash(content_hash)?;
        let url = join_url_segments(&self.config.base_url, &[&clean_index, "_mapping"])?;
        let meta = XerjMappingMeta {
            commit_sha: Some(clean_sha),
            content_hash: Some(clean_hash),
            updated_at_ms: Some(updated_at_ms),
        };
        StoreHttpRequest::new(HttpMethod::Put, url)
            .with_timeout(self.config.timeout)
            .with_json_body(&PutMappingBody { meta: &meta })
    }

    /// Reads the locally persisted SQLite [`CommitWatermark`] for `index`, if present.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] if `index` is invalid or SQLite fails to read.
    pub fn local_watermark(&self, index: &str) -> Result<Option<CommitWatermark>, AdapterError> {
        let clean = validate_xerj_identifier(index, "index", false)?;
        let row = self.local.read_xerj_watermark(&clean)?;
        Ok(row.map(
            |(commit_sha, content_hash, updated_at_ms)| CommitWatermark {
                index_name: clean,
                commit_sha,
                content_hash,
                updated_at_ms,
            },
        ))
    }

    /// Fetches `mappings._meta` from remote XERJ `GET /{index}/_mapping`.
    ///
    /// Returns `Ok(None)` if the index returns `404 Not Found` or has no `_meta` block.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] on invalid `index`, transport error, non-404 error
    /// status, or malformed mapping JSON.
    pub async fn fetch_remote_watermark(
        &self,
        index: &str,
    ) -> Result<Option<XerjMappingMeta>, AdapterError> {
        let req = self.build_get_mapping_request(index)?;
        let req_url = req.url.clone();
        let resp = self.transport.send(req).await?;
        if resp.status == 404 {
            return Ok(None);
        }
        let resp = resp.ensure_success(&req_url)?;
        let raw: serde_json::Value = resp.decode_json("decode XERJ _mapping response")?;
        extract_mapping_meta(&raw, index)
    }

    /// Compares target `(target_commit_sha, target_content_hash)` against both
    /// local SQLite state and remote XERJ `mappings._meta` (`REQ-LG-XERJ-03`).
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] if any identifier/hash is malformed or reading fails.
    pub async fn evaluate_watermark(
        &self,
        index: &str,
        target_commit_sha: &str,
        target_content_hash: &str,
    ) -> Result<WatermarkSyncDecision, AdapterError> {
        let clean_index = validate_xerj_identifier(index, "index", false)?;
        let clean_sha = validate_commit_sha(target_commit_sha)?;
        let clean_hash = validate_content_hash(target_content_hash)?;

        let local = self.local_watermark(&clean_index)?;
        let remote = self.fetch_remote_watermark(&clean_index).await?;

        let Some(remote_meta) = remote else {
            return Ok(WatermarkSyncDecision::NeedsSync {
                previous_commit_sha: local.as_ref().map(|w| w.commit_sha.clone()),
                previous_content_hash: local.as_ref().map(|w| w.content_hash.clone()),
                target_commit_sha: clean_sha,
                target_content_hash: clean_hash,
                reason: WatermarkDriftReason::MissingRemoteMeta,
            });
        };

        let Some(remote_sha) = remote_meta.commit_sha else {
            return Ok(WatermarkSyncDecision::NeedsSync {
                previous_commit_sha: local.as_ref().map(|w| w.commit_sha.clone()),
                previous_content_hash: local.as_ref().map(|w| w.content_hash.clone()),
                target_commit_sha: clean_sha,
                target_content_hash: clean_hash,
                reason: WatermarkDriftReason::MissingRemoteMeta,
            });
        };

        let Some(local_wm) = local else {
            return Ok(WatermarkSyncDecision::NeedsSync {
                previous_commit_sha: Some(remote_sha),
                previous_content_hash: remote_meta.content_hash,
                target_commit_sha: clean_sha,
                target_content_hash: clean_hash,
                reason: WatermarkDriftReason::MissingLocalWatermark,
            });
        };

        if local_wm.commit_sha != remote_sha
            || remote_meta.content_hash.as_deref() != Some(local_wm.content_hash.as_str())
        {
            return Ok(WatermarkSyncDecision::NeedsSync {
                previous_commit_sha: Some(remote_sha),
                previous_content_hash: remote_meta.content_hash,
                target_commit_sha: clean_sha,
                target_content_hash: clean_hash,
                reason: WatermarkDriftReason::LocalRemoteMismatch,
            });
        }

        if local_wm.commit_sha != clean_sha {
            return Ok(WatermarkSyncDecision::NeedsSync {
                previous_commit_sha: Some(local_wm.commit_sha),
                previous_content_hash: Some(local_wm.content_hash),
                target_commit_sha: clean_sha,
                target_content_hash: clean_hash,
                reason: WatermarkDriftReason::CommitShaChanged,
            });
        }

        if local_wm.content_hash != clean_hash {
            return Ok(WatermarkSyncDecision::NeedsSync {
                previous_commit_sha: Some(local_wm.commit_sha),
                previous_content_hash: Some(local_wm.content_hash),
                target_commit_sha: clean_sha,
                target_content_hash: clean_hash,
                reason: WatermarkDriftReason::ContentHashChanged,
            });
        }

        Ok(WatermarkSyncDecision::UpToDate {
            watermark: local_wm,
        })
    }

    /// Updates remote XERJ `PUT /{index}/_mapping` (`_meta.commit_sha` + `_meta.content_hash`)
    /// and persists the confirmed [`CommitWatermark`] in SQLite (`REQ-LG-XERJ-03`).
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] if validation, remote HTTP `PUT`, or SQLite write fails.
    pub async fn advance_watermark(
        &self,
        index: &str,
        commit_sha: &str,
        content_hash: &str,
        updated_at_ms: u64,
    ) -> Result<CommitWatermark, AdapterError> {
        let req =
            self.build_put_watermark_request(index, commit_sha, content_hash, updated_at_ms)?;
        let req_url = req.url.clone();
        self.transport.send(req).await?.ensure_success(&req_url)?;

        let clean_index = validate_xerj_identifier(index, "index", false)?;
        let clean_sha = validate_commit_sha(commit_sha)?;
        let clean_hash = validate_content_hash(content_hash)?;
        self.local
            .write_xerj_watermark(&clean_index, &clean_sha, &clean_hash, updated_at_ms)?;

        Ok(CommitWatermark {
            index_name: clean_index,
            commit_sha: clean_sha,
            content_hash: clean_hash,
            updated_at_ms,
        })
    }

    /// Recalls memories from `POST /_memory/{namespace}/_recall` (`REQ-LG-XERJ-04`).
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] if `query.query` is empty or the HTTP call fails.
    pub async fn recall_memories(
        &self,
        query: &XerjMemoryRecallQuery,
    ) -> Result<Vec<XerjMemoryRecallHit>, AdapterError> {
        if query.query.trim().is_empty() {
            return Err(AdapterError::InvalidIdentifier {
                kind: "recall_query",
                value: query.query.clone(),
                reason: "memory recall query must not be empty".to_owned(),
            });
        }
        let url = join_url_segments(
            &self.config.base_url,
            &["_memory", &self.config.memory_namespace, "_recall"],
        )?;
        let req = StoreHttpRequest::new(HttpMethod::Post, url)
            .with_timeout(self.config.timeout)
            .with_header("X-Xerj-Embed-Mode", self.config.embed_mode.as_str())
            .with_json_body(query)?;
        let req_url = req.url.clone();
        let resp = self.transport.send(req).await?.ensure_success(&req_url)?;
        let decoded: XerjRecallEnvelope = resp.decode_json("decode XERJ _memory/_recall")?;
        Ok(match decoded {
            XerjRecallEnvelope::Wrapped { hits }
            | XerjRecallEnvelope::Memories { memories: hits }
            | XerjRecallEnvelope::Direct(hits) => hits,
        })
    }

    async fn sync_memory_doc(
        &self,
        id: Option<&str>,
        text: &str,
        metadata: serde_json::Value,
    ) -> Result<(), AdapterError> {
        let url = join_url_segments(
            &self.config.base_url,
            &["_memory", &self.config.memory_namespace],
        )?;
        let body = XerjMemoryWriteBody {
            id,
            text,
            dedup: true,
            metadata,
        };
        let req = StoreHttpRequest::new(HttpMethod::Post, url)
            .with_timeout(self.config.timeout)
            .with_header("X-Xerj-Embed-Mode", self.config.embed_mode.as_str())
            .with_json_body(&body)?;
        let req_url = req.url.clone();
        self.transport.send(req).await?.ensure_success(&req_url)?;
        Ok(())
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

        let doc_id = format!("mem:{}:{}", scope.as_str(), op.block());
        let text = if updated_block.entries.is_empty() {
            String::from("(empty memory block)")
        } else {
            updated_block.entries.join("\n")
        };
        let flow_labels =
            autumn_harvest_agent::memory::snapshot_labels(std::slice::from_ref(&updated_block));
        let metadata = serde_json::json!({
            "kind": "memory_block",
            "scope": scope.as_str(),
            "block": op.block(),
            "edit_key": key.as_str(),
            "flow_labels": flow_labels,
        });

        self.sync_memory_doc(Some(&doc_id), &text, metadata)
            .await
            .map_err(AdapterError::into_agent_error)?;

        self.local.apply_now(scope, &op, key, labels)
    }
}

fn extract_mapping_meta(
    raw: &serde_json::Value,
    index: &str,
) -> Result<Option<XerjMappingMeta>, AdapterError> {
    // Handles both `{"<index>":{"mappings":{"_meta":{...}}}}` and direct `{"mappings":{"_meta":{...}}}`.
    let meta_val = raw
        .get(index)
        .and_then(|idx| idx.get("mappings"))
        .and_then(|m| m.get("_meta"))
        .or_else(|| raw.get("mappings").and_then(|m| m.get("_meta")))
        .or_else(|| raw.get("_meta"));

    let Some(meta_val) = meta_val else {
        return Ok(None);
    };
    let parsed: XerjMappingMeta =
        serde_json::from_value(meta_val.clone()).map_err(|err| AdapterError::Decode {
            context: "decode mappings._meta",
            message: err.to_string(),
        })?;
    Ok(Some(parsed))
}

impl<T: HttpTransport> MemoryStore for XerjHybridSearchStore<T> {
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
            let metadata = serde_json::json!({
                "kind": "memory_clear",
                "scope": scope.as_str(),
            });
            self.sync_memory_doc(
                None,
                &format!("cleared memory scope {}", scope.as_str()),
                metadata,
            )
            .await
            .map_err(AdapterError::into_agent_error)?;
            MemoryStore::clear(&self.local, scope).await
        })
    }
}

impl<T: HttpTransport> EpisodeStore for XerjHybridSearchStore<T> {
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

            let doc_id = format!("ep:{}:{}", scope.as_str(), episode.id);
            let text = if episode.text.trim().is_empty() {
                format!("episode {}", episode.id)
            } else {
                episode.text.clone()
            };
            let metadata = serde_json::json!({
                "kind": "episode",
                "scope": scope.as_str(),
                "episode_id": episode.id,
                "edit_key": key.as_str(),
                "at_ms": episode.at_ms,
                "flow_labels": episode.labels,
            });
            self.sync_memory_doc(Some(&doc_id), &text, metadata)
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

            let doc_id = format!("pass:{}:{}", scope.as_str(), mark.key.as_str());
            let text = if mark.facts.is_empty() {
                format!("consolidation pass {}", mark.key.as_str())
            } else {
                mark.facts.join("\n")
            };
            let metadata = serde_json::json!({
                "kind": "consolidation_pass",
                "scope": scope.as_str(),
                "pass_key": mark.key.as_str(),
                "generation": mark.generation,
                "episodes": mark.episodes,
            });
            self.sync_memory_doc(Some(&doc_id), &text, metadata)
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

            let metadata = serde_json::json!({
                "kind": "episode_clear",
                "scope": scope.as_str(),
                "clear_key": key.as_str(),
            });
            self.sync_memory_doc(
                None,
                &format!("cleared episode log {}", scope.as_str()),
                metadata,
            )
            .await
            .map_err(AdapterError::into_agent_error)?;

            EpisodeStore::clear(&self.local, scope, key).await
        })
    }
}
