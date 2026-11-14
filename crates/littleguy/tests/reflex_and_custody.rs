//! Integration proofs for `ReflexToolPolicy` and `BlackwallCustodyClient` (`specs/0008`).

use std::collections::VecDeque;
use std::error::Error;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use autumn_harvest_agent::model::BoxFuture;
use autumn_harvest_agent::{
    AgentError, AgentModel, ChatRequest, ChatResponse, ContentPart, FnTool, RunId, RunInfo,
    StopReason, TokenUsage, Tool, ToolCall, ToolDecision, ToolEffect, ToolPolicy,
};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::post;
use axum::{Json, Router};
use littleguy::custody::{
    BLACKWALL_CUSTODY_LABEL, BLACKWALL_PARENT_WORLD, BlackwallCustodyClient,
    CUSTODY_BUNDLE_VERSION, CustodyBundle,
};
use littleguy::pool::{
    MAX_REPLY_BODY_BYTES, PoolClient, PoolConfig, PoolToken, PoolWorker, RuntimeTurnOptions,
    RuntimeTurnRunner, StepOutcome,
};
use littleguy::reflex_gate::{
    ClassifyResponse, REFLEX_CLASSIFY_TIMEOUT, ReflexClassifyClient, ReflexToolPolicy,
    map_tool_verb,
};
use littleguy::secret_file::SecretFile;
use littleguy_core::{BlackwallProject, Config, OwnerName, PoolPeerId, RoleName, ShellId};
use littleguy_models::OfflineModel;
use littleguy_stores::{
    AdapterError, HttpMethod, HttpTransport, MockHttpTransport, SqliteStores, StoreHttpRequest,
    StoreHttpResponse, sha256_hex,
};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const LG: &str = env!("CARGO_BIN_EXE_lg");
const POOL_TOKEN: &str = "pool-bearer-secret-0008";
const BLACKWALL_TOKEN: &str = "bw-custody-secret-0008";
const PEER_ID: &str = "88888888-8888-4888-8888-888888888888";

fn write_secret_0600(dir: &Path, name: &str, secret: &str) -> TestResult<PathBuf> {
    let path = dir.join(name);
    std::fs::write(&path, format!("{secret}\n"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

fn dummy_run_info() -> RunInfo {
    RunInfo {
        run_id: RunId::new("run-0008"),
        session_id: None,
        steps_used: 0,
        max_steps: 8,
        usage: TokenUsage::new(0, 0),
    }
}

fn make_tool(name: &str, effect: ToolEffect) -> Arc<dyn Tool> {
    Arc::new(
        FnTool::new(name, "test tool", json!({"type": "object"}), |_| async {
            Ok(json!({"ok": true}))
        })
        .effect(effect),
    )
}

#[derive(Debug)]
struct FixedInnerPolicy(ToolDecision);

impl ToolPolicy for FixedInnerPolicy {
    fn decide<'a>(
        &'a self,
        _call: &'a ToolCall,
        _tool: Option<&'a dyn Tool>,
        _info: &'a RunInfo,
    ) -> BoxFuture<'a, ToolDecision> {
        Box::pin(std::future::ready(self.0.clone()))
    }
}

#[derive(Debug)]
struct SlowTransport(Duration);

impl HttpTransport for SlowTransport {
    fn send(
        &self,
        _request: StoreHttpRequest,
    ) -> BoxFuture<'_, Result<StoreHttpResponse, AdapterError>> {
        let wait = self.0;
        Box::pin(async move {
            tokio::time::sleep(wait).await;
            Ok(StoreHttpResponse::new(
                200,
                br#"{"tier":"GREEN","policy":"p","basis":"b"}"#.to_vec(),
            ))
        })
    }
}

#[tokio::test]
async fn reflex_classify_wire_matches_ves_domain_contract() -> TestResult {
    let mock = MockHttpTransport::new();
    let client =
        ReflexClassifyClient::with_transport("http://127.0.0.1:7717", Arc::new(mock.clone()))?;
    assert_eq!(
        client.endpoint().as_str(),
        "http://127.0.0.1:7717/v1/classify"
    );

    mock.enqueue_json(
        200,
        &ClassifyResponse {
            tier: "GREEN".into(),
            policy: Some("vespertin/core:read@v1".into()),
            basis: Some("baseline GREEN".into()),
        },
    )?;

    let policy = ReflexToolPolicy::new(client, "assistant/alice", false);
    let read_tool = make_tool("read_file", ToolEffect::ReadOnly);
    let args = json!({"path": "README.md"});
    let expected_sha = sha256_hex(&serde_json::to_vec(&args)?);
    let call = ToolCall {
        id: "call-1".into(),
        name: "read_file".into(),
        arguments: args,
    };

    let decision = policy
        .decide(&call, Some(read_tool.as_ref()), &dummy_run_info())
        .await;
    assert_eq!(decision, ToolDecision::Allow);

    let recorded = mock.recorded_requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].method, HttpMethod::Post);
    assert_eq!(recorded[0].timeout, REFLEX_CLASSIFY_TIMEOUT);
    let sent_json = recorded[0].parse_json_body()?;
    let root = sent_json.as_object().ok_or("expected JSON object")?;
    assert_eq!(root.len(), 4, "ClassifyBody has deny_unknown_fields");
    assert_eq!(root.get("agent"), Some(&json!("assistant/alice")));
    assert_eq!(root.get("verb"), Some(&json!("read")));
    assert_eq!(root.get("payload_sha256"), Some(&json!(expected_sha)));
    let params = root
        .get("params")
        .and_then(Value::as_object)
        .ok_or("missing params object")?;
    assert_eq!(params.len(), 1);
    assert_eq!(params.get("target"), Some(&json!("read_file")));

    // Check remaining verb mappings.
    let sched_call = ToolCall {
        id: "c2".into(),
        name: "calendar_create".into(),
        arguments: json!({}),
    };
    let ext_tool = make_tool("calendar_create", ToolEffect::External);
    assert_eq!(
        map_tool_verb(&sched_call, Some(ext_tool.as_ref())),
        "schedule"
    );

    let write_call = ToolCall {
        id: "c3".into(),
        name: "custom_patch".into(),
        arguments: json!({}),
    };
    let write_tool = make_tool("custom_patch", ToolEffect::Write);
    assert_eq!(
        map_tool_verb(&write_call, Some(write_tool.as_ref())),
        "file-internal"
    );

    let internal_call = ToolCall {
        id: "c4".into(),
        name: "memory_note".into(),
        arguments: json!({}),
    };
    let internal_tool = make_tool("memory_note", ToolEffect::Internal);
    assert_eq!(
        map_tool_verb(&internal_call, Some(internal_tool.as_ref())),
        "draft"
    );
    assert_eq!(map_tool_verb(&write_call, None), "send-external");
    Ok(())
}

#[tokio::test]
async fn reflex_policy_allows_green_and_yellow_and_delegates_to_inner() -> TestResult {
    let mock = MockHttpTransport::new();
    for tier in ["GREEN", "yellow", "YELLOW"] {
        mock.enqueue_json(
            200,
            &json!({"tier": tier, "policy": "p1", "basis": "delegated"}),
        )?;
    }
    let client =
        ReflexClassifyClient::with_transport("http://127.0.0.1:7717/v1/classify", Arc::new(mock))?;
    let call = ToolCall {
        id: "call-allow".into(),
        name: "web_fetch".into(),
        arguments: json!({"url": "https://example.com"}),
    };
    let tool = make_tool("web_fetch", ToolEffect::ReadOnly);
    let info = dummy_run_info();

    let direct = ReflexToolPolicy::new(client.clone(), "assistant/alice", false);
    assert_eq!(
        direct.decide(&call, Some(tool.as_ref()), &info).await,
        ToolDecision::Allow
    );
    assert_eq!(
        direct.decide(&call, Some(tool.as_ref()), &info).await,
        ToolDecision::Allow
    );

    let wrapped = ReflexToolPolicy::new(client, "assistant/alice", true).with_inner(Arc::new(
        FixedInnerPolicy(ToolDecision::RequireApproval {
            reason: "inner gate held call".into(),
        }),
    ));
    assert_eq!(
        wrapped.decide(&call, Some(tool.as_ref()), &info).await,
        ToolDecision::RequireApproval {
            reason: "inner gate held call".into()
        }
    );
    Ok(())
}

#[tokio::test]
async fn reflex_policy_red_requires_approval_in_serve_and_denies_in_pool() -> TestResult {
    let dir = tempfile::tempdir()?;
    let stores = Arc::new(SqliteStores::open(dir.path().join("stores.db"))?);
    let mock = MockHttpTransport::new();
    mock.enqueue_json(
        200,
        &json!({"tier": "RED", "policy": "vespertin/comms:send-external@v3", "basis": "operator sign-off required"}),
    )?;
    mock.enqueue_json(
        200,
        &json!({"tier": "red", "policy": "vespertin/comms:send-external@v3", "basis": "operator sign-off required"}),
    )?;

    let client = ReflexClassifyClient::with_transport("http://127.0.0.1:7717", Arc::new(mock))?;
    let call = ToolCall {
        id: "call-red-1".into(),
        name: "slack_post".into(),
        arguments: json!({"channel": "C123", "text": "hello"}),
    };
    let tool = make_tool("slack_post", ToolEffect::External);
    let info = dummy_run_info();

    let serve_policy = ReflexToolPolicy::new(client.clone(), "assistant/alice", true)
        .with_approval_store(Arc::clone(&stores));
    let serve_decision = serve_policy.decide(&call, Some(tool.as_ref()), &info).await;
    assert!(matches!(
        serve_decision,
        ToolDecision::RequireApproval { ref reason } if reason.contains("RED") && reason.contains("operator sign-off required")
    ));
    let claimed = stores
        .claim_approval("sig-red-1", "call-red-1", Some("alice"))?
        .ok_or("expected RED call to be recorded for approval")?;
    assert!(claimed.fresh);
    let open = stores.open_approvals()?;
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].tool, "slack_post");
    assert_eq!(
        open[0].arguments,
        json!({"channel": "C123", "text": "hello"})
    );

    let pool_policy = ReflexToolPolicy::new(client, "assistant/pool", false);
    let pool_decision = pool_policy.decide(&call, Some(tool.as_ref()), &info).await;
    assert!(matches!(
        pool_decision,
        ToolDecision::Deny { ref reason } if reason.contains("RED")
    ));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn reflex_policy_fails_closed_on_never_unknown_http_error_and_timeout() -> TestResult {
    let mock = MockHttpTransport::new();
    mock.enqueue_json(
        200,
        &json!({"tier": "NEVER", "policy": "protected", "basis": "protected target marrow"}),
    )?;
    mock.enqueue_json(
        200,
        &json!({"tier": "CHARTREUSE", "policy": "p", "basis": "b"}),
    )?;
    mock.enqueue(Ok(StoreHttpResponse::new(
        503,
        b"reflex unavailable".to_vec(),
    )));
    mock.enqueue(Ok(StoreHttpResponse::new(200, b"not-json".to_vec())));
    mock.enqueue(Err(AdapterError::Transport {
        message: "connection refused".into(),
    }));

    let client = ReflexClassifyClient::with_transport("http://127.0.0.1:7717", Arc::new(mock))?;
    let policy = ReflexToolPolicy::new(client, "assistant/alice", true);
    let call = ToolCall {
        id: "call-fc".into(),
        name: "delete_file".into(),
        arguments: json!({"path": "marrow"}),
    };
    let tool = make_tool("delete_file", ToolEffect::Write);
    let info = dummy_run_info();

    for expected_substr in [
        "NEVER",
        "unknown tier",
        "HTTP 503",
        "JSON failure",
        "connection refused",
    ] {
        let decision = policy.decide(&call, Some(tool.as_ref()), &info).await;
        match decision {
            ToolDecision::Deny { reason } => assert!(
                reason.contains(expected_substr),
                "expected `{expected_substr}` in `{reason}`"
            ),
            other => return Err(format!("expected Deny, got {other:?}").into()),
        }
    }

    // Verify 5s timeout fails closed.
    let slow_client = ReflexClassifyClient::with_transport(
        "http://127.0.0.1:7717",
        Arc::new(SlowTransport(Duration::from_secs(6))),
    )?;
    let slow_policy = ReflexToolPolicy::new(slow_client, "assistant/alice", true);
    let timeout_decision = slow_policy.decide(&call, Some(tool.as_ref()), &info).await;
    match timeout_decision {
        ToolDecision::Deny { reason } => assert!(
            reason.contains("timed out after 5s"),
            "unexpected timeout reason: {reason}"
        ),
        other => return Err(format!("expected timeout Deny, got {other:?}").into()),
    }
    Ok(())
}

#[derive(Debug)]
struct ScriptedToolModel;

impl AgentModel for ScriptedToolModel {
    fn chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AgentError>> {
        let has_tool_result = request.messages.last().is_some_and(|msg| {
            msg.content
                .iter()
                .any(|part| matches!(part, ContentPart::ToolResult { .. }))
        });
        Box::pin(async move {
            if has_tool_result {
                Ok(ChatResponse {
                    content: vec![ContentPart::Text("turn-after-tool".into())],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::new(2, 2),
                })
            } else {
                Ok(ChatResponse {
                    content: vec![ContentPart::ToolCall {
                        id: "tc-1".into(),
                        name: "touch_probe".into(),
                        arguments: json!({"slot": 1}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::new(2, 2),
                })
            }
        })
    }

    fn model_id(&self) -> Option<&str> {
        Some("scripted-tool-model-1")
    }
}

#[derive(Default)]
struct PoolStubState {
    deliveries: VecDeque<Value>,
    sends: Vec<Value>,
    acks: Vec<String>,
    nacks: Vec<(String, u32)>,
    terms: Vec<(String, String)>,
    blackwall_auth: Vec<String>,
    blackwall_paths: Vec<String>,
    blackwall_bundles: Vec<Value>,
    blackwall_replies: VecDeque<(StatusCode, Value)>,
}

const fn json_reply(
    code: StatusCode,
    payload: String,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    (code, [("content-type", "application/json")], payload)
}

fn handle_blackwall_import(
    state: &Mutex<PoolStubState>,
    auth: &str,
    path: &str,
    body: &Value,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let queued = {
        let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
        guard.blackwall_auth.push(auth.to_owned());
        guard.blackwall_paths.push(path.to_owned());
        guard.blackwall_bundles.push(body.clone());
        guard.blackwall_replies.pop_front()
    };
    if auth != format!("Bearer {BLACKWALL_TOKEN}") {
        return json_reply(
            StatusCode::UNAUTHORIZED,
            json!({"error": "bad blackwall token"}).to_string(),
        );
    }
    if let Some((code, payload)) = queued {
        return json_reply(code, payload.to_string());
    }
    let run_hash = body
        .get("run_hash")
        .and_then(Value::as_str)
        .unwrap_or_default();
    json_reply(StatusCode::OK, json!({"run_hash": run_hash}).to_string())
}

async fn handle_pool_and_blackwall(
    State(state): State<Arc<Mutex<PoolStubState>>>,
    headers: HeaderMap,
    uri: Uri,
    Json(body): Json<Value>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();

    if uri.path().starts_with("/v1/projects/") && uri.path().ends_with("/runs/import") {
        return handle_blackwall_import(&state, &auth, uri.path(), &body);
    }

    if auth != format!("Bearer {POOL_TOKEN}") {
        return json_reply(StatusCode::UNAUTHORIZED, "{}".to_owned());
    }
    match uri.path() {
        "/v1/mesh/pool/recv" => {
            let batch: Vec<Value> = state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .deliveries
                .pop_front()
                .into_iter()
                .collect();
            json_reply(StatusCode::OK, Value::Array(batch).to_string())
        }
        "/v1/mesh/send" => {
            state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .sends
                .push(body);
            json_reply(StatusCode::OK, json!({"ok": true}).to_string())
        }
        "/v1/mesh/pool/ack" => {
            if let Some(arr) = body.get("delivery_tokens").and_then(Value::as_array) {
                let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
                for tok in arr.iter().filter_map(Value::as_str) {
                    guard.acks.push(tok.to_owned());
                }
            }
            json_reply(StatusCode::NO_CONTENT, String::new())
        }
        "/v1/mesh/pool/nack" => {
            let tok = body
                .get("delivery_token")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let delay = body
                .get("delay_ms")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0);
            state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .nacks
                .push((tok, delay));
            json_reply(StatusCode::NO_CONTENT, String::new())
        }
        "/v1/mesh/pool/term" => {
            let tok = body
                .get("delivery_token")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let reason = body
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .terms
                .push((tok, reason));
            json_reply(StatusCode::NO_CONTENT, String::new())
        }
        "/v1/mesh/pool/extend" => json_reply(StatusCode::NO_CONTENT, String::new()),
        _ => json_reply(StatusCode::NOT_FOUND, "{}".to_owned()),
    }
}

async fn start_stub_server(
    state: Arc<Mutex<PoolStubState>>,
) -> TestResult<(String, tokio::task::JoinHandle<()>)> {
    let app = Router::new()
        .route("/{*path}", post(handle_pool_and_blackwall))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{addr}"), handle))
}

fn make_delivery(token: &str, seq: u64, task_id: &str, attempt: u32, body: Value) -> Value {
    let body_str = match body {
        Value::String(s) => s,
        other => other.to_string(),
    };
    json!({
        "delivery_token": token,
        "stream_seq": seq,
        "lease_ms": 5000,
        "delivery_attempt": 1,
        "message": {
            "kind": "msg",
            "from_session": "service-escapement-pool",
            "subject": format!("task {task_id}"),
            "body": body_str,
            "refs": {
                "task_id": task_id,
                "attempt": attempt,
            }
        }
    })
}

fn make_pool_config(base_url: &str, dir: &Path) -> TestResult<PoolConfig> {
    let tok_path = write_secret_0600(dir, "pool-token", POOL_TOKEN)?;
    Ok(PoolConfig {
        cortex_url: base_url.to_owned(),
        token: PoolToken::load(&tok_path)?,
        peer_id: PoolPeerId::new(PEER_ID)?,
        role: RoleName::new("service-pool-littleman-1")?,
        allowed_sender: RoleName::new("service-escapement-pool")?,
        wait_ms: 50,
        nack_delay_ms: 2_500,
        shell_id: ShellId::new("shell-0008")?,
    })
}

#[tokio::test]
async fn pool_runtime_enforces_reflex_tool_policy_before_executing_tools() -> TestResult {
    let state = Arc::new(Mutex::new(PoolStubState::default()));
    state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .deliveries
        .extend([
            make_delivery("tok-rfx-1", 1, "t-rfx-deny", 0, json!("run probe")),
            make_delivery("tok-rfx-2", 2, "t-rfx-allow", 0, json!("run probe")),
        ]);
    let (base_url, server) = start_stub_server(Arc::clone(&state)).await?;
    let dir = tempfile::tempdir()?;
    let pool_cfg = make_pool_config(&base_url, dir.path())?;
    let client = PoolClient::new(&pool_cfg)?;

    let mock_reflex = MockHttpTransport::new();
    mock_reflex.enqueue_json(
        200,
        &json!({"tier": "RED", "policy": "p", "basis": "headless red denied"}),
    )?;
    mock_reflex.enqueue_json(
        200,
        &json!({"tier": "GREEN", "policy": "p", "basis": "allowed"}),
    )?;
    let reflex_client =
        ReflexClassifyClient::with_transport("http://127.0.0.1:7717", Arc::new(mock_reflex))?;
    let policy = Arc::new(ReflexToolPolicy::new(
        reflex_client,
        "assistant/pool-owner",
        false,
    ));

    let executed = Arc::new(AtomicUsize::new(0));
    let exec_counter = Arc::clone(&executed);
    let probe_tool: Arc<dyn Tool> = Arc::new(
        FnTool::new(
            "touch_probe",
            "increments counter when allowed",
            json!({"type": "object"}),
            move |_| {
                let c = Arc::clone(&exec_counter);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"ran": true}))
                }
            },
        )
        .effect(ToolEffect::External),
    );

    let config = Config::for_owner(OwnerName::new("pool-owner")?);
    let runner = RuntimeTurnRunner::open_with(
        &dir.path().join("lg.db"),
        &config,
        Arc::new(ScriptedToolModel),
        pool_cfg.shell_id.clone(),
        RuntimeTurnOptions {
            tools: vec![probe_tool],
            workspace: None,
            policy: Some(policy),
            custody: None,
        },
    )?;
    let mut worker = PoolWorker::new(pool_cfg, client, runner);

    assert_eq!(worker.step().await?, StepOutcome::Settled);
    assert_eq!(
        executed.load(Ordering::SeqCst),
        0,
        "RED must deny tool execution in headless pool mode"
    );

    assert_eq!(worker.step().await?, StepOutcome::Settled);
    assert_eq!(
        executed.load(Ordering::SeqCst),
        1,
        "GREEN must permit tool execution"
    );

    server.abort();
    Ok(())
}

#[test]
fn blackwall_token_file_0600_and_debug_redaction() -> TestResult {
    let dir = tempfile::tempdir()?;
    let bad_path = write_secret_0600(dir.path(), "bw-bad", BLACKWALL_TOKEN)?;
    std::fs::set_permissions(&bad_path, std::fs::Permissions::from_mode(0o644))?;
    let project = BlackwallProject::new("vespertin")?;

    let err = BlackwallCustodyClient::open("http://127.0.0.1:8080", &bad_path, project.clone())
        .err()
        .ok_or("expected 0644 token file to be rejected")?;
    let rendered = err.to_string();
    assert!(rendered.contains("0644"), "unexpected error: {rendered}");
    assert!(!rendered.contains(BLACKWALL_TOKEN));

    let good_path = write_secret_0600(dir.path(), "bw-good", BLACKWALL_TOKEN)?;
    let link_path = dir.path().join("bw-link");
    std::os::unix::fs::symlink(&good_path, &link_path)?;
    assert!(
        BlackwallCustodyClient::open("http://127.0.0.1:8080", &link_path, project.clone()).is_err()
    );

    let client = BlackwallCustodyClient::open("http://127.0.0.1:8080", &good_path, project)?;
    let dbg = format!("{client:?}");
    assert!(dbg.contains("REDACTED"));
    assert!(!dbg.contains(BLACKWALL_TOKEN));
    Ok(())
}

#[test]
fn cli_rejects_bad_reflex_url_and_mismatched_or_insecure_blackwall_flags() -> TestResult {
    let dir = tempfile::tempdir()?;
    let pool_tok = write_secret_0600(dir.path(), "pool.tok", POOL_TOKEN)?;
    let bw_good = write_secret_0600(dir.path(), "bw.tok", BLACKWALL_TOKEN)?;
    let bw_bad_mode = write_secret_0600(dir.path(), "bw-0644.tok", BLACKWALL_TOKEN)?;
    std::fs::set_permissions(&bw_bad_mode, std::fs::Permissions::from_mode(0o644))?;

    let pool_tok_s = pool_tok.to_str().ok_or("utf8 path")?;
    let bw_good_s = bw_good.to_str().ok_or("utf8 path")?;
    let bw_bad_s = bw_bad_mode.to_str().ok_or("utf8 path")?;

    let base_pool = [
        "pool",
        "--cortex-url",
        "http://127.0.0.1:9",
        "--token-file",
        pool_tok_s,
        "--peer-id",
        PEER_ID,
        "--shell-id",
        "shell-cli-0008",
    ];

    for extra in [
        vec!["--reflex-classify-url", "ftp://127.0.0.1:7717"],
        vec!["--reflex-classify-url", "http://127.0.0.1:7717?query=1"],
        vec!["--blackwall-url", "http://127.0.0.1:7878"],
        vec!["--blackwall-token-file", bw_good_s],
        vec![
            "--blackwall-url",
            "http://127.0.0.1:7878",
            "--blackwall-token-file",
            bw_bad_s,
        ],
        vec![
            "--blackwall-url",
            "http://127.0.0.1:7878",
            "--blackwall-token-file",
            bw_good_s,
            "--blackwall-project",
            "../evil",
        ],
        vec![
            "--blackwall-url",
            "http://127.0.0.1:7878",
            "--blackwall-token-file",
            bw_good_s,
            "--blackwall-project",
            "a/b/c",
        ],
    ] {
        let out = Command::new(LG)
            .args(base_pool)
            .args(&extra)
            .env("LG_LOG", "warn")
            .output()?;
        assert!(!out.status.success(), "expected failure for args {extra:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains(BLACKWALL_TOKEN));
        assert!(!stderr.contains(POOL_TOKEN));
    }

    let serve_bad = Command::new(LG)
        .args([
            "serve",
            "--socket",
            dir.path().join("lg.sock").to_str().unwrap_or_default(),
            "--db",
            dir.path().join("lg.db").to_str().unwrap_or_default(),
            "--reflex-classify-url",
            "http://user:secret@127.0.0.1:7717",
        ])
        .env("LG_LOG", "warn")
        .output()?;
    assert!(!serve_bad.status.success());
    Ok(())
}

#[tokio::test]
async fn custody_bundle_blake3_matches_pretty_run_record_bytes_and_two_segment_url() -> TestResult {
    let state = Arc::new(Mutex::new(PoolStubState::default()));
    state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .deliveries
        .push_back(make_delivery(
            "tok-bw-1",
            10,
            "t-bw-1",
            0,
            json!("verify custody hash"),
        ));
    let (base_url, server) = start_stub_server(Arc::clone(&state)).await?;
    let dir = tempfile::tempdir()?;
    let pool_cfg = make_pool_config(&base_url, dir.path())?;
    let bw_tok = SecretFile::load(&write_secret_0600(dir.path(), "bw.tok", BLACKWALL_TOKEN)?)?;
    let custody = BlackwallCustodyClient::open(
        &base_url,
        &write_secret_0600(dir.path(), "bw2.tok", BLACKWALL_TOKEN)?,
        BlackwallProject::new("vespertin")?,
    )?;
    assert_eq!(bw_tok.expose_secret(), BLACKWALL_TOKEN);

    let config = Config::for_owner(OwnerName::new("custody-owner")?);
    let runner = RuntimeTurnRunner::open_with(
        &dir.path().join("lg.db"),
        &config,
        Arc::new(OfflineModel::new()),
        pool_cfg.shell_id.clone(),
        RuntimeTurnOptions {
            custody: Some(custody),
            ..RuntimeTurnOptions::default()
        },
    )?;
    let mut worker = PoolWorker::new(pool_cfg.clone(), PoolClient::new(&pool_cfg)?, runner);
    assert_eq!(worker.step().await?, StepOutcome::Settled);

    let guard = state.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(
        guard.blackwall_paths,
        vec!["/v1/projects/vespertin/runs/import"]
    );

    // Verify exact CustodyBundle structure and byte-for-byte blake3 RunRecord hash.
    let bundle_val = &guard.blackwall_bundles[0];
    let parsed_bundle: CustodyBundle = serde_json::from_value(bundle_val.clone())?;
    assert_eq!(parsed_bundle.version, CUSTODY_BUNDLE_VERSION);
    assert_eq!(parsed_bundle.context.run, parsed_bundle.run_hash);
    assert_eq!(parsed_bundle.context.parent_world, BLACKWALL_PARENT_WORLD);
    assert_eq!(parsed_bundle.context.changeset, "");
    assert_eq!(
        bundle_val["context"]["settle_state"],
        json!({"status": "unsettled"})
    );
    assert_eq!(bundle_val["blobs"], json!({}));

    let expected_record_pretty = serde_json::to_string_pretty(&parsed_bundle.record)?;
    let expected_blake3 = blake3::hash(expected_record_pretty.as_bytes())
        .to_hex()
        .to_string();
    assert_eq!(parsed_bundle.run_hash.len(), 64);
    assert_eq!(parsed_bundle.run_hash, expected_blake3);
    assert_eq!(parsed_bundle.record.prompt, "verify custody hash");
    assert_eq!(parsed_bundle.record.model, "littleguy-offline-1");
    assert_eq!(
        parsed_bundle.record.stdout,
        "littleguy (offline): you said 'verify custody hash'"
    );
    assert_eq!(parsed_bundle.record.stderr, "");
    assert_eq!(parsed_bundle.record.exit_code, 0);
    assert_eq!(parsed_bundle.record.project.as_deref(), Some("vespertin"));

    // Verify reply body & refs satisfy ves_domain::escapement::TaskResult::outcome().
    let sent_msg = &guard.sends[0]["message"];
    assert_eq!(
        sent_msg["refs"]["run_hash"].as_str(),
        Some(expected_blake3.as_str())
    );
    let reply_body: Value = serde_json::from_str(sent_msg["body"].as_str().ok_or("body str")?)?;
    assert_eq!(reply_body["exit"], json!("ok"));
    assert_eq!(reply_body["run_hash"], json!(expected_blake3));
    assert_eq!(reply_body["custody"], json!(BLACKWALL_CUSTODY_LABEL));
    assert_eq!(reply_body["shell_id"], json!("shell-0008"));
    drop(guard);

    server.abort();
    Ok(())
}

#[tokio::test]
async fn pool_delivery_project_override_routes_two_segments_and_invalid_project_terms() -> TestResult
{
    let state = Arc::new(Mutex::new(PoolStubState::default()));
    state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .deliveries
        .extend([
            make_delivery(
                "tok-bw-2",
                11,
                "t-bw-2",
                0,
                json!({
                    "prompt": "override project",
                    "project": "cerebral-work/vespertin"
                }),
            ),
            make_delivery(
                "tok-bw-bad-proj",
                12,
                "t-bw-bad",
                0,
                json!({
                    "prompt": "bad project",
                    "project": "../escape"
                }),
            ),
            make_delivery(
                "tok-bw-bad-type",
                13,
                "t-bw-bad-type",
                0,
                json!({
                    "prompt": "bad project type",
                    "project": 42
                }),
            ),
        ]);
    let (base_url, server) = start_stub_server(Arc::clone(&state)).await?;
    let dir = tempfile::tempdir()?;
    let pool_cfg = make_pool_config(&base_url, dir.path())?;
    let custody = BlackwallCustodyClient::open(
        &base_url,
        &write_secret_0600(dir.path(), "bw.tok", BLACKWALL_TOKEN)?,
        BlackwallProject::new("vespertin")?,
    )?;
    let model = Arc::new(OfflineModel::new());
    let config = Config::for_owner(OwnerName::new("custody-proj")?);
    let runner = RuntimeTurnRunner::open_with(
        &dir.path().join("lg.db"),
        &config,
        Arc::clone(&model) as Arc<dyn AgentModel>,
        pool_cfg.shell_id.clone(),
        RuntimeTurnOptions {
            custody: Some(custody),
            ..RuntimeTurnOptions::default()
        },
    )?;
    let mut worker = PoolWorker::new(pool_cfg.clone(), PoolClient::new(&pool_cfg)?, runner);

    assert_eq!(worker.step().await?, StepOutcome::Settled);
    assert_eq!(worker.step().await?, StepOutcome::Termed);
    assert_eq!(worker.step().await?, StepOutcome::Termed);
    assert_eq!(
        model.calls(),
        1,
        "invalid project deliveries must never run LLM"
    );

    let guard = state.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(
        guard.blackwall_paths,
        vec!["/v1/projects/cerebral-work/vespertin/runs/import"]
    );
    assert_eq!(guard.terms.len(), 2);
    assert_eq!(guard.terms[0].0, "tok-bw-bad-proj");
    assert!(guard.terms[0].1.contains("invalid project"));
    assert_eq!(guard.terms[1].0, "tok-bw-bad-type");
    assert!(guard.terms[1].1.contains("invalid project"));
    drop(guard);

    server.abort();
    Ok(())
}

#[tokio::test]
async fn pool_custody_503_nacks_and_redelivery_settles_without_second_model_call() -> TestResult {
    let state = Arc::new(Mutex::new(PoolStubState::default()));
    {
        let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
        guard.deliveries.extend([
            make_delivery("tok-503-a", 21, "t-custody-503", 0, json!("retry custody")),
            make_delivery("tok-503-b", 22, "t-custody-503", 0, json!("retry custody")),
        ]);
        guard.blackwall_replies.push_back((
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "temporary outage"}),
        ));
        drop(guard);
    }
    let (base_url, server) = start_stub_server(Arc::clone(&state)).await?;
    let dir = tempfile::tempdir()?;
    let pool_cfg = make_pool_config(&base_url, dir.path())?;
    let custody = BlackwallCustodyClient::open(
        &base_url,
        &write_secret_0600(dir.path(), "bw.tok", BLACKWALL_TOKEN)?,
        BlackwallProject::new("vespertin")?,
    )?;
    let model = Arc::new(OfflineModel::new());
    let config = Config::for_owner(OwnerName::new("custody-retry")?);
    let runner = RuntimeTurnRunner::open_with(
        &dir.path().join("lg.db"),
        &config,
        Arc::clone(&model) as Arc<dyn AgentModel>,
        pool_cfg.shell_id.clone(),
        RuntimeTurnOptions {
            custody: Some(custody),
            ..RuntimeTurnOptions::default()
        },
    )?;
    let mut worker = PoolWorker::new(pool_cfg.clone(), PoolClient::new(&pool_cfg)?, runner);

    assert_eq!(worker.step().await?, StepOutcome::Nacked);
    {
        let guard = state.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(guard.nacks, vec![("tok-503-a".to_owned(), 2_500)]);
        assert_eq!(
            guard.sends.len(),
            0,
            "503 must never report ok without custody"
        );
        assert_eq!(guard.acks, Vec::<String>::new());
        drop(guard);
    }

    assert_eq!(worker.step().await?, StepOutcome::Settled);
    assert_eq!(
        model.calls(),
        1,
        "redelivered custody retry must reuse durable SQLite turn answer"
    );
    let guard = state.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(guard.acks, vec!["tok-503-b"]);
    assert_eq!(guard.sends.len(), 1);
    let reply_body: Value =
        serde_json::from_str(guard.sends[0]["message"]["body"].as_str().ok_or("body")?)?;
    assert_eq!(reply_body["exit"], json!("ok"));
    assert_eq!(reply_body["custody"], json!(BLACKWALL_CUSTODY_LABEL));
    assert_eq!(reply_body["run_hash"].as_str().map(str::len), Some(64));
    drop(guard);

    server.abort();
    Ok(())
}

#[tokio::test]
async fn pool_custody_400_and_hash_mismatch_settle_failed_with_exit_err() -> TestResult {
    let state = Arc::new(Mutex::new(PoolStubState::default()));
    {
        let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
        guard.deliveries.extend([
            make_delivery("tok-400", 31, "t-custody-400", 0, json!("bad bundle")),
            make_delivery(
                "tok-mismatch",
                32,
                "t-custody-mismatch",
                0,
                json!("wrong hash"),
            ),
        ]);
        guard
            .blackwall_replies
            .push_back((StatusCode::BAD_REQUEST, json!({"error": "integrity"})));
        guard
            .blackwall_replies
            .push_back((StatusCode::OK, json!({"run_hash": "0".repeat(64)})));
        drop(guard);
    }
    let (base_url, server) = start_stub_server(Arc::clone(&state)).await?;
    let dir = tempfile::tempdir()?;
    let pool_cfg = make_pool_config(&base_url, dir.path())?;
    let custody = BlackwallCustodyClient::open(
        &base_url,
        &write_secret_0600(dir.path(), "bw.tok", BLACKWALL_TOKEN)?,
        BlackwallProject::new("vespertin")?,
    )?;
    let config = Config::for_owner(OwnerName::new("custody-fail")?);
    let runner = RuntimeTurnRunner::open_with(
        &dir.path().join("lg.db"),
        &config,
        Arc::new(OfflineModel::new()),
        pool_cfg.shell_id.clone(),
        RuntimeTurnOptions {
            custody: Some(custody),
            ..RuntimeTurnOptions::default()
        },
    )?;
    let mut worker = PoolWorker::new(pool_cfg.clone(), PoolClient::new(&pool_cfg)?, runner);

    assert_eq!(worker.step().await?, StepOutcome::Settled);
    assert_eq!(worker.step().await?, StepOutcome::Settled);

    let guard = state.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(guard.nacks, Vec::<(String, u32)>::new());
    assert_eq!(guard.acks, vec!["tok-400", "tok-mismatch"]);
    assert_eq!(guard.sends.len(), 2);

    for (idx, expected_err) in [(0_usize, "HTTP 400"), (1_usize, "run_hash mismatch")] {
        let msg = &guard.sends[idx]["message"];
        assert_eq!(msg["refs"]["outcome"], json!("failed"));
        assert_eq!(msg["refs"]["transient"], json!(false));
        let body: Value = serde_json::from_str(msg["body"].as_str().ok_or("body")?)?;
        assert_eq!(body["exit"], json!("err"));
        assert_eq!(body["shell_id"], json!("shell-0008"));
        let err_text = body["error"].as_str().ok_or("missing body.error")?;
        assert!(
            err_text.contains(expected_err),
            "expected `{expected_err}` in `{err_text}`"
        );
    }
    drop(guard);

    server.abort();
    Ok(())
}

#[derive(Debug)]
struct HugeAnswerModel(String);

impl AgentModel for HugeAnswerModel {
    fn chat<'a>(
        &'a self,
        _request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AgentError>> {
        let text = self.0.clone();
        Box::pin(async move {
            Ok(ChatResponse {
                content: vec![ContentPart::Text(text)],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::new(4, 4096),
            })
        })
    }

    fn model_id(&self) -> Option<&str> {
        Some("huge-answer-model")
    }
}

#[tokio::test]
async fn pool_reply_truncates_oversized_answer_preserving_custody_and_exit_metadata() -> TestResult
{
    let state = Arc::new(Mutex::new(PoolStubState::default()));
    state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .deliveries
        .push_back(make_delivery(
            "tok-huge-1",
            41,
            "t-huge-1",
            0,
            json!("produce >8KiB"),
        ));
    let (base_url, server) = start_stub_server(Arc::clone(&state)).await?;
    let dir = tempfile::tempdir()?;
    let pool_cfg = make_pool_config(&base_url, dir.path())?;
    let custody = BlackwallCustodyClient::open(
        &base_url,
        &write_secret_0600(dir.path(), "bw.tok", BLACKWALL_TOKEN)?,
        BlackwallProject::new("cerebral-work/vespertin")?,
    )?;
    let full_answer = "é\n".repeat(4_500);
    assert!(full_answer.len() > MAX_REPLY_BODY_BYTES);

    let config = Config::for_owner(OwnerName::new("custody-trunc")?);
    let runner = RuntimeTurnRunner::open_with(
        &dir.path().join("lg.db"),
        &config,
        Arc::new(HugeAnswerModel(full_answer.clone())),
        pool_cfg.shell_id.clone(),
        RuntimeTurnOptions {
            custody: Some(custody),
            ..RuntimeTurnOptions::default()
        },
    )?;
    let mut worker = PoolWorker::new(pool_cfg.clone(), PoolClient::new(&pool_cfg)?, runner);
    assert_eq!(worker.step().await?, StepOutcome::Settled);

    let guard = state.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(
        guard.blackwall_paths,
        vec!["/v1/projects/cerebral-work/vespertin/runs/import"]
    );
    // Blackwall receives the FULL untruncated stdout in CustodyBundle!
    assert_eq!(
        guard.blackwall_bundles[0]["record"]["stdout"].as_str(),
        Some(full_answer.as_str())
    );
    let expected_hash = guard.blackwall_bundles[0]["run_hash"]
        .as_str()
        .ok_or("missing bundle run_hash")?
        .to_owned();

    let sent_msg = &guard.sends[0]["message"];
    let raw_body = sent_msg["body"].as_str().ok_or("missing sent body")?;
    assert!(
        raw_body.len() <= MAX_REPLY_BODY_BYTES,
        "reply body {} exceeded {MAX_REPLY_BODY_BYTES}",
        raw_body.len()
    );
    let parsed: Value = serde_json::from_str(raw_body)?;
    assert_eq!(parsed["exit"], json!("ok"));
    assert_eq!(parsed["run_hash"], json!(expected_hash));
    assert_eq!(parsed["custody"], json!(BLACKWALL_CUSTODY_LABEL));
    assert_eq!(parsed["shell_id"], json!("shell-0008"));
    assert_eq!(parsed["transcript_sha256"].as_str().map(str::len), Some(64));
    assert_eq!(sent_msg["refs"]["run_hash"], json!(expected_hash));
    let truncated_ans = parsed["answer"].as_str().ok_or("missing answer")?;
    assert_ne!(truncated_ans, "");
    assert!(full_answer.starts_with(truncated_ans));
    drop(guard);

    server.abort();
    Ok(())
}
