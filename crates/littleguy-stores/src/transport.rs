//! Pluggable HTTP transport for durable store adapters (`specs/0003`).
//!
//! [`HttpTransport`] separates URL/payload construction and durability rules
//! from live socket I/O. Production callers wire [`ReqwestHttpTransport`],
//! while unit tests run against [`MockHttpTransport`] with zero live network
//! dependency.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use autumn_harvest_agent::model::BoxFuture;
use autumn_harvest_agent::{AgentError, ErrorKind};
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use url::Url;

use crate::StoreError;

/// Default per-request HTTP timeout for Reverie and XERJ store calls.
pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// HTTP verb of a [`StoreHttpRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    /// `GET` request.
    Get,
    /// `POST` request.
    Post,
    /// `PUT` request.
    Put,
    /// `DELETE` request.
    Delete,
}

impl HttpMethod {
    /// Uppercase wire token of the HTTP method.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed outbound HTTP request built without string interpolation (`REQ-LG-STORE-01..02`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreHttpRequest {
    /// HTTP verb.
    pub method: HttpMethod,
    /// Validated target URL built via [`Url`] path-segment and query-pair APIs.
    pub url: Url,
    /// Ordered HTTP headers `(name, value)`.
    pub headers: Vec<(String, String)>,
    /// Optional serialized request body bytes.
    pub body: Option<Vec<u8>>,
    /// Per-request timeout bound.
    pub timeout: Duration,
}

impl StoreHttpRequest {
    /// Creates an empty request for `method` and `url` with [`DEFAULT_HTTP_TIMEOUT`].
    #[must_use]
    pub const fn new(method: HttpMethod, url: Url) -> Self {
        Self {
            method,
            url,
            headers: Vec::new(),
            body: None,
            timeout: DEFAULT_HTTP_TIMEOUT,
        }
    }

    /// Sets the timeout bound on this request.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Appends an HTTP header to this request.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Serializes `payload` as UTF-8 JSON and sets `Content-Type: application/json`.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Decode`] when `serde_json` fails to serialize `payload`.
    pub fn with_json_body<T: Serialize>(mut self, payload: &T) -> Result<Self, AdapterError> {
        let bytes = serde_json::to_vec(payload).map_err(|err| AdapterError::Decode {
            context: "serialize request JSON body",
            message: err.to_string(),
        })?;
        self.headers
            .push(("Content-Type".to_owned(), "application/json".to_owned()));
        self.body = Some(bytes);
        Ok(self)
    }

    /// Parses the JSON body back as [`serde_json::Value`], for deterministic test assertions.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Decode`] if no body is present or the body is invalid JSON.
    pub fn parse_json_body(&self) -> Result<serde_json::Value, AdapterError> {
        let Some(bytes) = &self.body else {
            return Err(AdapterError::Decode {
                context: "request has no body",
                message: "expected JSON body bytes".to_owned(),
            });
        };
        serde_json::from_slice(bytes).map_err(|err| AdapterError::Decode {
            context: "decode request JSON body",
            message: err.to_string(),
        })
    }
}

/// Typed HTTP response returned by an [`HttpTransport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreHttpResponse {
    /// Numeric HTTP status code (`200`, `404`, `503`, and so on).
    pub status: u16,
    /// Raw response body bytes.
    pub body: Vec<u8>,
}

impl StoreHttpResponse {
    /// Constructs a response from `status` and raw `body` bytes.
    #[must_use]
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    /// Constructs a response by serializing `value` as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Decode`] if serialization fails.
    pub fn json<T: Serialize>(status: u16, value: &T) -> Result<Self, AdapterError> {
        let body = serde_json::to_vec(value).map_err(|err| AdapterError::Decode {
            context: "serialize mock JSON response",
            message: err.to_string(),
        })?;
        Ok(Self { status, body })
    }

    /// Whether `self.status` is in `200..300`.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }

    /// Lossy UTF-8 rendering of `self.body` truncated to 512 characters for diagnostics.
    #[must_use]
    pub fn body_preview(&self) -> String {
        let text = String::from_utf8_lossy(&self.body);
        text.chars().take(512).collect()
    }

    /// Requires a `2xx` status code, returning [`AdapterError::HttpStatus`] otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::HttpStatus`] when `!self.is_success()`.
    pub fn ensure_success(self, request_url: &Url) -> Result<Self, AdapterError> {
        if self.is_success() {
            return Ok(self);
        }
        let body = self.body_preview();
        Err(AdapterError::HttpStatus {
            status: self.status,
            url: request_url.as_str().to_owned(),
            body,
        })
    }

    /// Deserializes `self.body` as JSON of type `T`.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Decode`] when `self.body` does not decode as `T`.
    pub fn decode_json<T: DeserializeOwned>(
        &self,
        context: &'static str,
    ) -> Result<T, AdapterError> {
        serde_json::from_slice(&self.body).map_err(|err| AdapterError::Decode {
            context,
            message: err.to_string(),
        })
    }
}

/// Errors produced by Reverie and XERJ store adapters and [`HttpTransport`] implementations.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// Client configuration failed validation.
    #[error("invalid adapter config `{field}`: {reason}")]
    InvalidConfig {
        /// Configuration field name.
        field: &'static str,
        /// Explanation of the validation failure.
        reason: String,
    },

    /// A caller-supplied identifier (`topic_key`, `project`, `index`, `commit_sha`) is invalid.
    #[error("invalid {kind} `{value}`: {reason}")]
    InvalidIdentifier {
        /// Identifier category.
        kind: &'static str,
        /// Rejected value.
        value: String,
        /// Why validation rejected it.
        reason: String,
    },

    /// Network or client transport failed to complete the request.
    #[error("HTTP transport failure: {message}")]
    Transport {
        /// Underlying transport error text.
        message: String,
    },

    /// Remote server returned a non-2xx HTTP status code.
    #[error("HTTP {status} from `{url}`: {body}")]
    HttpStatus {
        /// HTTP status code.
        status: u16,
        /// Request URL.
        url: String,
        /// Truncated response body preview.
        body: String,
    },

    /// JSON encoding or decoding failed.
    #[error("JSON failure ({context}): {message}")]
    Decode {
        /// Operation context.
        context: &'static str,
        /// Underlying serde error message.
        message: String,
    },

    /// Local SQLite store failed to open or migrate.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// Engine [`AgentError`] bubbled up from local durable state.
    #[error("agent store error: {0}")]
    Agent(#[from] AgentError),
}

impl AdapterError {
    /// Maps an [`AdapterError`] to an `autumn-harvest` [`AgentError`] with the
    /// appropriate retry classification.
    #[must_use]
    pub fn into_agent_error(self) -> AgentError {
        match self {
            Self::Agent(err) => err,
            Self::InvalidConfig { .. } => AgentError::new(ErrorKind::Config, self.to_string()),
            Self::InvalidIdentifier { .. } => AgentError::new(ErrorKind::Tool, self.to_string()),
            Self::HttpStatus { status, .. } if (400..500).contains(&status) && status != 429 => {
                AgentError::new(ErrorKind::Tool, self.to_string())
            }
            Self::Transport { .. } | Self::HttpStatus { .. } | Self::Store(_) => {
                AgentError::new(ErrorKind::Unavailable, self.to_string())
            }
            Self::Decode { .. } => AgentError::new(ErrorKind::Decode, self.to_string()),
        }
    }
}

/// Pluggable async HTTP client abstraction (`REQ-LG-STORE-01`).
pub trait HttpTransport: Send + Sync + fmt::Debug {
    /// Executes one [`StoreHttpRequest`] and resolves with [`StoreHttpResponse`].
    fn send(
        &self,
        request: StoreHttpRequest,
    ) -> BoxFuture<'_, Result<StoreHttpResponse, AdapterError>>;
}

/// Production [`HttpTransport`] backed by `reqwest 0.13` and `rustls` (`ring`).
#[derive(Debug, Clone)]
pub struct ReqwestHttpTransport {
    client: reqwest::Client,
}

impl ReqwestHttpTransport {
    /// Builds a [`ReqwestHttpTransport`] with the `ring` TLS provider installed.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Transport`] if `reqwest::Client::builder` fails.
    pub fn new() -> Result<Self, AdapterError> {
        install_ring_tls();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|err| AdapterError::Transport {
                message: err.to_string(),
            })?;
        Ok(Self { client })
    }

    /// Wraps an existing `reqwest::Client`.
    #[must_use]
    pub fn with_client(client: reqwest::Client) -> Self {
        install_ring_tls();
        Self { client }
    }

    /// Builds a [`ReqwestHttpTransport`] configured with a per-read idle timeout for SSE streams.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Transport`] if `reqwest::Client::builder` fails.
    pub fn new_streaming(read_timeout: Duration) -> Result<Self, AdapterError> {
        install_ring_tls();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(read_timeout)
            .build()
            .map_err(|err| AdapterError::Transport {
                message: err.to_string(),
            })?;
        Ok(Self { client })
    }

    /// Streams newline-delimited SSE lines from `url` until EOF, `on_line` returns `false`,
    /// or an HTTP/transport error occurs.
    ///
    /// Lines exceeding `max_line_bytes` are discarded up to the next `\n` so an oversized frame
    /// cannot exhaust process memory.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::HttpStatus`] on non-2xx responses and [`AdapterError::Transport`]
    /// on connection or chunk read errors.
    pub async fn stream_sse_lines(
        &self,
        url: Url,
        headers: Vec<(String, String)>,
        max_line_bytes: usize,
        on_connected: &(dyn Fn() + Send + Sync),
        on_line: &(dyn Fn(&str) -> bool + Send + Sync),
    ) -> Result<(), AdapterError> {
        let mut builder = self
            .client
            .get(url.clone())
            .header("Accept", "text/event-stream");
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        let mut response = builder
            .send()
            .await
            .map_err(|err| AdapterError::Transport {
                message: err.to_string(),
            })?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = response
                .bytes()
                .await
                .map(|b| String::from_utf8_lossy(&b).chars().take(256).collect())
                .unwrap_or_default();
            return Err(AdapterError::HttpStatus {
                status,
                url: url.to_string(),
                body,
            });
        }
        on_connected();
        let mut buf: Vec<u8> = Vec::new();
        let mut discarding = false;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| AdapterError::Transport {
                message: err.to_string(),
            })?
        {
            for &byte in &chunk {
                if byte == b'\n' {
                    if discarding {
                        discarding = false;
                        buf.clear();
                        continue;
                    }
                    if buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                    let line = String::from_utf8_lossy(&buf);
                    let keep_going = on_line(&line);
                    buf.clear();
                    if !keep_going {
                        return Ok(());
                    }
                } else if !discarding {
                    if buf.len() >= max_line_bytes {
                        discarding = true;
                        buf.clear();
                    } else {
                        buf.push(byte);
                    }
                }
            }
        }
        Ok(())
    }
}

impl HttpTransport for ReqwestHttpTransport {
    fn send(
        &self,
        request: StoreHttpRequest,
    ) -> BoxFuture<'_, Result<StoreHttpResponse, AdapterError>> {
        Box::pin(async move {
            let method = match request.method {
                HttpMethod::Get => reqwest::Method::GET,
                HttpMethod::Post => reqwest::Method::POST,
                HttpMethod::Put => reqwest::Method::PUT,
                HttpMethod::Delete => reqwest::Method::DELETE,
            };
            let mut builder = self
                .client
                .request(method, request.url.clone())
                .timeout(request.timeout);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let response = builder
                .send()
                .await
                .map_err(|err| AdapterError::Transport {
                    message: err.to_string(),
                })?;
            let status = response.status().as_u16();
            let body = response
                .bytes()
                .await
                .map_err(|err| AdapterError::Transport {
                    message: err.to_string(),
                })?
                .to_vec();
            Ok(StoreHttpResponse { status, body })
        })
    }
}

fn install_ring_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Synchronous responder callback used by [`MockHttpTransport`] in offline tests.
pub type MockRouteHandler =
    Arc<dyn Fn(&StoreHttpRequest) -> Result<StoreHttpResponse, AdapterError> + Send + Sync>;

struct MockRouteRule {
    method: HttpMethod,
    path: String,
    handler: MockRouteHandler,
}

#[derive(Default)]
struct MockState {
    recorded: Vec<StoreHttpRequest>,
    queued: VecDeque<Result<StoreHttpResponse, AdapterError>>,
    routes: Vec<MockRouteRule>,
}

/// Deterministic, 100% offline [`HttpTransport`] for unit and integration tests.
///
/// Precedence per call:
/// 1. Pop the front entry of `queued` responses if non-empty.
/// 2. Otherwise match the first registered `(method, path)` route rule.
/// 3. Otherwise return `200 OK` with `{}`.
#[derive(Clone, Default)]
pub struct MockHttpTransport {
    state: Arc<Mutex<MockState>>,
}

impl fmt::Debug for MockHttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("MockHttpTransport")
            .field("recorded_requests", &guard.recorded.len())
            .field("queued_responses", &guard.queued.len())
            .field("routes", &guard.routes.len())
            .finish()
    }
}

impl MockHttpTransport {
    /// Creates an empty offline mock transport.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueues a one-shot response (or simulated transport error) returned on the next request.
    pub fn enqueue(&self, result: Result<StoreHttpResponse, AdapterError>) {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        guard.queued.push_back(result);
    }

    /// Enqueues a serialized JSON response with `status`.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterError::Decode`] if `value` fails to serialize.
    pub fn enqueue_json<T: Serialize>(&self, status: u16, value: &T) -> Result<(), AdapterError> {
        let response = StoreHttpResponse::json(status, value)?;
        self.enqueue(Ok(response));
        Ok(())
    }

    /// Registers a persistent route handler matching `(method, exact_path)`.
    pub fn on_route<F>(&self, method: HttpMethod, path: impl Into<String>, handler: F)
    where
        F: Fn(&StoreHttpRequest) -> Result<StoreHttpResponse, AdapterError> + Send + Sync + 'static,
    {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        guard.routes.push(MockRouteRule {
            method,
            path: path.into(),
            handler: Arc::new(handler),
        });
    }

    /// Returns a snapshot of all requests sent through this mock transport, oldest first.
    #[must_use]
    pub fn recorded_requests(&self) -> Vec<StoreHttpRequest> {
        let guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        guard.recorded.clone()
    }

    /// Clears recorded requests while keeping registered routes.
    pub fn clear_recorded(&self) {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        guard.recorded.clear();
    }
}

impl HttpTransport for MockHttpTransport {
    fn send(
        &self,
        request: StoreHttpRequest,
    ) -> BoxFuture<'_, Result<StoreHttpResponse, AdapterError>> {
        let outcome = {
            let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            guard.recorded.push(request.clone());
            guard.queued.pop_front().unwrap_or_else(|| {
                guard
                    .routes
                    .iter()
                    .find(|r| r.method == request.method && r.path == request.url.path())
                    .map_or_else(
                        || Ok(StoreHttpResponse::new(200, b"{}".to_vec())),
                        |rule| (rule.handler)(&request),
                    )
            })
        };
        Box::pin(std::future::ready(outcome))
    }
}

/// Validates and parses an HTTP(S) base URL with no embedded user credentials,
/// query string, or fragment (`REQ-LG-STORE-02`).
///
/// # Errors
///
/// Returns [`AdapterError::InvalidConfig`] when `raw` is not a clean `http` or
/// `https` hierarchical base URL.
pub fn parse_clean_base_url(raw: &str, field: &'static str) -> Result<Url, AdapterError> {
    let trimmed = raw.trim();
    let parsed = Url::parse(trimmed).map_err(|err| AdapterError::InvalidConfig {
        field,
        reason: format!("invalid URL `{trimmed}`: {err}"),
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(AdapterError::InvalidConfig {
            field,
            reason: format!(
                "unsupported URL scheme `{}` (expected `http` or `https`)",
                parsed.scheme()
            ),
        });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(AdapterError::InvalidConfig {
            field,
            reason: "URL must not embed credentials".to_owned(),
        });
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(AdapterError::InvalidConfig {
            field,
            reason: "base URL must not include a query string or fragment".to_owned(),
        });
    }
    if parsed.cannot_be_a_base() {
        return Err(AdapterError::InvalidConfig {
            field,
            reason: "URL cannot serve as a hierarchical base URL".to_owned(),
        });
    }
    Ok(parsed)
}

/// Appends path segments cleanly onto `base` using [`Url::path_segments_mut`],
/// avoiding any manual `/` concatenation or string interpolation.
///
/// # Errors
///
/// Returns [`AdapterError::InvalidConfig`] when `base` does not support path segments.
pub fn join_url_segments(base: &Url, segments: &[&str]) -> Result<Url, AdapterError> {
    let mut url = base.clone();
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|()| AdapterError::InvalidConfig {
                field: "base_url",
                reason: "base URL does not support path segments".to_owned(),
            })?;
        path.pop_if_empty();
        for seg in segments {
            path.push(seg);
        }
    }
    Ok(url)
}
