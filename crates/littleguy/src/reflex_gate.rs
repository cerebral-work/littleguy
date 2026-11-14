//! Pre-tool `reflexd` `/v1/classify` gate policy (`specs/0008`, `REQ-LG-REFLEX-01..04`).
//!
//! [`ReflexToolPolicy`] classifies each candidate [`ToolCall`] against `reflexd`
//! before execution. `GREEN` and `YELLOW` delegate to the inner [`ToolPolicy`],
//! `RED` requests approval in `lg serve` or denies in headless `lg pool`, and
//! `NEVER`, unknown tiers, non-2xx statuses, timeouts, or decode errors fail closed.

use std::sync::Arc;
use std::time::Duration;

use autumn_harvest_agent::{
    BoxFuture, RunInfo, Tool, ToolCall, ToolDecision, ToolEffect, ToolPolicy,
};
use littleguy_stores::{
    AdapterError, HttpMethod, HttpTransport, ReqwestHttpTransport, SqliteStores, StoreHttpRequest,
    Url, join_url_segments, parse_clean_base_url, sha256_hex,
};
use serde::{Deserialize, Serialize};

/// Per-request timeout on `POST /v1/classify` (`REQ-LG-REFLEX-02`, `REQ-LG-REFLEX-04`).
pub const REFLEX_CLASSIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// Validates `--reflex-classify-url` as a clean `http://` or `https://` URL (`REQ-LG-REFLEX-01`).
///
/// When `raw` has a bare `/` path (for example `http://127.0.0.1:7717`), appends
/// `v1/classify` via [`join_url_segments`]. Explicit paths such as
/// `http://127.0.0.1:7717/v1/classify` are preserved unchanged.
///
/// # Errors
///
/// Returns [`AdapterError::InvalidConfig`] when `raw` is malformed, uses a non-HTTP(S)
/// scheme, embeds credentials, or carries a query string or fragment.
pub fn parse_reflex_classify_url(raw: &str) -> Result<Url, AdapterError> {
    let parsed = parse_clean_base_url(raw, "reflex_classify_url")?;
    if parsed.path() == "/" {
        return join_url_segments(&parsed, &["v1", "classify"]);
    }
    Ok(parsed)
}

/// Maps a [`ToolCall`] and optional [`Tool`] metadata onto a `ves-domain` kebab-case `Verb`.
#[must_use]
pub fn map_tool_verb(call: &ToolCall, tool: Option<&dyn Tool>) -> &'static str {
    match call.name.as_str() {
        "calendar_create" => "schedule",
        "write_file" | "mail_draft" => "draft",
        "delete_file" => "file-internal",
        "mail_send" => "send-external",
        _ => match tool.map(Tool::effect) {
            Some(ToolEffect::ReadOnly) => "read",
            Some(ToolEffect::Internal) => "draft",
            Some(ToolEffect::Write) => "file-internal",
            Some(ToolEffect::External) | None => "send-external",
        },
    }
}

/// Parameters nested inside [`ClassifyRequest`] (`ves_domain::verb::VerbParams`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifyParams {
    /// Tool target name inspected by `reflexd` posture and protected-target rules.
    pub target: String,
}

/// Outbound JSON body for `POST /v1/classify` (`ves_domain::http::ClassifyBody`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifyRequest {
    /// Self-declared agent identifier (such as `assistant/alice`).
    pub agent: String,
    /// Kebab-case `ves-domain` verb token.
    pub verb: String,
    /// Verb parameters carrying `target`.
    pub params: ClassifyParams,
    /// 64-character lowercase hex SHA-256 of the serialized tool arguments.
    pub payload_sha256: String,
}

/// Inbound JSON body from `POST /v1/classify` (`ves_domain::wire::Classification`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifyResponse {
    /// Gate tier (`GREEN`, `YELLOW`, `RED`, `NEVER`).
    pub tier: String,
    /// Policy identifier reported by `reflexd`, when present.
    #[serde(default)]
    pub policy: Option<String>,
    /// Classification basis reported by `reflexd`, when present.
    #[serde(default)]
    pub basis: Option<String>,
}

/// Bounded HTTP client for `reflexd` `POST /v1/classify`.
#[derive(Debug, Clone)]
pub struct ReflexClassifyClient {
    endpoint: Url,
    transport: Arc<dyn HttpTransport>,
}

impl ReflexClassifyClient {
    /// Builds a client targeting `raw_url` over [`ReqwestHttpTransport`].
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] when `raw_url` fails [`parse_reflex_classify_url`]
    /// or the HTTP client fails to initialize.
    pub fn new(raw_url: &str) -> Result<Self, AdapterError> {
        let endpoint = parse_reflex_classify_url(raw_url)?;
        let transport = Arc::new(ReqwestHttpTransport::new()?);
        Ok(Self {
            endpoint,
            transport,
        })
    }

    /// Builds a client targeting `raw_url` over a custom [`HttpTransport`].
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError`] when `raw_url` fails [`parse_reflex_classify_url`].
    pub fn with_transport(
        raw_url: &str,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, AdapterError> {
        let endpoint = parse_reflex_classify_url(raw_url)?;
        Ok(Self {
            endpoint,
            transport,
        })
    }

    /// Resolved `POST /v1/classify` endpoint URL.
    #[must_use]
    pub const fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Sends `request` to `reflexd` under [`REFLEX_CLASSIFY_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// Returns a diagnostic reason string on timeout, transport failure, non-2xx HTTP
    /// status, or malformed JSON response.
    pub async fn classify(&self, request: &ClassifyRequest) -> Result<ClassifyResponse, String> {
        let http_req = StoreHttpRequest::new(HttpMethod::Post, self.endpoint.clone())
            .with_timeout(REFLEX_CLASSIFY_TIMEOUT)
            .with_json_body(request)
            .map_err(|err| err.to_string())?;
        let response = tokio::time::timeout(REFLEX_CLASSIFY_TIMEOUT, self.transport.send(http_req))
            .await
            .map_err(|_| {
                format!(
                    "reflex classify timed out after {}s",
                    REFLEX_CLASSIFY_TIMEOUT.as_secs()
                )
            })?
            .map_err(|err| err.to_string())?
            .ensure_success(&self.endpoint)
            .map_err(|err| err.to_string())?;
        response
            .decode_json::<ClassifyResponse>("reflex classify response")
            .map_err(|err| err.to_string())
    }
}

/// Pre-tool [`ToolPolicy`] backed by `reflexd` `/v1/classify` (`REQ-LG-REFLEX-02..04`).
#[derive(Debug, Clone)]
pub struct ReflexToolPolicy {
    client: ReflexClassifyClient,
    agent_name: String,
    allow_approval: bool,
    inner: Option<Arc<dyn ToolPolicy>>,
    approval_store: Option<Arc<SqliteStores>>,
}

impl ReflexToolPolicy {
    /// Creates a policy for `agent_name`.
    ///
    /// Set `allow_approval` to `true` in interactive `lg serve` so `RED` verdicts
    /// return [`ToolDecision::RequireApproval`], and `false` in headless `lg pool`
    /// so `RED` verdicts fail closed with [`ToolDecision::Deny`].
    #[must_use]
    pub fn new(
        client: ReflexClassifyClient,
        agent_name: impl Into<String>,
        allow_approval: bool,
    ) -> Self {
        Self {
            client,
            agent_name: agent_name.into(),
            allow_approval,
            inner: None,
            approval_store: None,
        }
    }

    /// Chains an inner [`ToolPolicy`] evaluated when `reflexd` returns `GREEN` or `YELLOW`.
    #[must_use]
    pub fn with_inner(mut self, inner: Arc<dyn ToolPolicy>) -> Self {
        self.inner = Some(inner);
        self
    }

    /// Attaches [`SqliteStores`] so `RED` verdicts in `lg serve` record call arguments
    /// before returning [`ToolDecision::RequireApproval`].
    #[must_use]
    pub fn with_approval_store(mut self, store: Arc<SqliteStores>) -> Self {
        self.approval_store = Some(store);
        self
    }

    async fn evaluate<'a>(
        &'a self,
        call: &'a ToolCall,
        tool: Option<&'a dyn Tool>,
        info: &'a RunInfo,
    ) -> ToolDecision {
        let Ok(arg_bytes) = serde_json::to_vec(&call.arguments) else {
            return ToolDecision::Deny {
                reason: format!(
                    "reflex gate denied `{}` (fail-closed): failed to serialize tool arguments",
                    call.name
                ),
            };
        };
        let req = ClassifyRequest {
            agent: self.agent_name.clone(),
            verb: map_tool_verb(call, tool).to_owned(),
            params: ClassifyParams {
                target: call.name.clone(),
            },
            payload_sha256: sha256_hex(&arg_bytes),
        };
        let resp = match self.client.classify(&req).await {
            Ok(resp) => resp,
            Err(why) => {
                tracing::warn!(
                    tool = %call.name,
                    reason = %why,
                    "reflex classify failed; denying tool call fail-closed"
                );
                return ToolDecision::Deny {
                    reason: format!("reflex gate denied `{}` (fail-closed): {why}", call.name),
                };
            }
        };
        let detail = resp
            .basis
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| resp.policy.as_deref().filter(|s| !s.trim().is_empty()))
            .unwrap_or("reflex policy");
        match resp.tier.trim().to_ascii_lowercase().as_str() {
            "green" | "yellow" => match &self.inner {
                Some(inner) => inner.decide(call, tool, info).await,
                None => ToolDecision::Allow,
            },
            "red" => {
                let reason = format!("reflex classified `{}` as RED ({detail})", call.name);
                if !self.allow_approval {
                    return ToolDecision::Deny { reason };
                }
                if let Some(stores) = &self.approval_store
                    && let Err(error) =
                        stores.record_approval(&call.id, &call.name, &call.arguments, &reason)
                {
                    tracing::error!(%error, tool = %call.name, "failed to record RED approval");
                    return ToolDecision::Deny {
                        reason: String::from(
                            "the call could not be recorded for approval, so it did not run",
                        ),
                    };
                }
                ToolDecision::RequireApproval { reason }
            }
            "never" => ToolDecision::Deny {
                reason: format!("reflex classified `{}` as NEVER ({detail})", call.name),
            },
            _ => ToolDecision::Deny {
                reason: format!(
                    "reflex gate denied `{}` (fail-closed): unknown tier `{}`",
                    call.name, resp.tier
                ),
            },
        }
    }
}

impl ToolPolicy for ReflexToolPolicy {
    fn decide<'a>(
        &'a self,
        call: &'a ToolCall,
        tool: Option<&'a dyn Tool>,
        info: &'a RunInfo,
    ) -> BoxFuture<'a, ToolDecision> {
        Box::pin(self.evaluate(call, tool, info))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url_normalizes_root_and_rejects_dirty_urls() {
        let bare = parse_reflex_classify_url("http://127.0.0.1:7717").unwrap();
        assert_eq!(bare.as_str(), "http://127.0.0.1:7717/v1/classify");

        let explicit = parse_reflex_classify_url("https://reflex.internal/v1/classify").unwrap();
        assert_eq!(explicit.as_str(), "https://reflex.internal/v1/classify");

        for bad in [
            "",
            "ftp://127.0.0.1:7717",
            "http://user:pass@127.0.0.1:7717",
            "http://127.0.0.1:7717/v1/classify?x=1",
            "http://127.0.0.1:7717/v1/classify#frag",
        ] {
            assert!(
                parse_reflex_classify_url(bad).is_err(),
                "expected `{bad}` to fail clean URL validation"
            );
        }
    }
}
