//! Integration proofs for `lg pool` (`specs/0005`, `REQ-LG-POOL-01..07`).

use std::collections::VecDeque;
use std::error::Error;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use autumn_harvest_agent::model::BoxFuture;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::post;
use axum::{Json, Router};
use littleguy::pool::{
    MAX_DELIVERY_BODY_BYTES, PoolClient, PoolConfig, PoolError, PoolToken, PoolWorker,
    RuntimeTurnRunner, StepOutcome, TurnError, TurnInput, TurnOutput, TurnRunner,
};
use littleguy_core::{Config, OwnerName, PoolPeerId, RoleName, ShellId};
use littleguy_models::OfflineModel;
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const LG: &str = env!("CARGO_BIN_EXE_lg");
const SECRET: &str = "super-secret-bearer-token-998877";
const PEER_ID: &str = "11111111-1111-1111-1111-111111111111";

#[derive(Default)]
struct StubData {
    deliveries: VecDeque<Value>,
    recv_raw: VecDeque<(StatusCode, String)>,
    recv_fallback_raw: Option<(StatusCode, String)>,
    send_statuses: VecDeque<StatusCode>,
    term_statuses: VecDeque<StatusCode>,
    recvs: Vec<(Option<String>, Value)>,
    acks: Vec<String>,
    extends: Vec<String>,
    nacks: Vec<(String, u32)>,
    terms: Vec<(String, String)>,
    sends: Vec<Value>,
}

#[derive(Clone, Default)]
struct StubCortex(Arc<Mutex<StubData>>);

async fn stub_post(
    State(stub): State<StubCortex>,
    headers: HeaderMap,
    uri: Uri,
    Json(b): Json<Value>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let reply =
        |code: StatusCode, body: String| (code, [("content-type", "application/json")], body);
    let authed = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {SECRET}"));
    if !authed {
        return reply(StatusCode::UNAUTHORIZED, "{}".to_owned());
    }
    let mut g = stub.0.lock().unwrap_or_else(PoisonError::into_inner);
    let res = match uri.path() {
        "/v1/mesh/pool/recv" => {
            g.recvs.push((uri.query().map(str::to_owned), b));
            if let Some((code, raw)) = g.recv_raw.pop_front() {
                reply(code, raw)
            } else if let Some(item) = g.deliveries.pop_front() {
                reply(StatusCode::OK, Value::Array(vec![item]).to_string())
            } else if let Some((code, raw)) = g.recv_fallback_raw.clone() {
                reply(code, raw)
            } else {
                reply(StatusCode::OK, "[]".to_owned())
            }
        }
        "/v1/mesh/pool/ack" => {
            if let Some(arr) = b["delivery_tokens"].as_array() {
                g.acks
                    .extend(arr.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            reply(StatusCode::NO_CONTENT, String::new())
        }
        "/v1/mesh/pool/extend" => {
            if let Some(tok) = b["delivery_token"].as_str() {
                g.extends.push(tok.to_owned());
            }
            reply(StatusCode::NO_CONTENT, String::new())
        }
        "/v1/mesh/pool/nack" => {
            let tok = b["delivery_token"].as_str().unwrap_or_default().to_owned();
            let ms = u32::try_from(b["delay_ms"].as_u64().unwrap_or(0)).unwrap_or(0);
            g.nacks.push((tok, ms));
            reply(StatusCode::NO_CONTENT, String::new())
        }
        "/v1/mesh/pool/term" => {
            let tok = b["delivery_token"].as_str().unwrap_or_default().to_owned();
            let why = b["reason"].as_str().unwrap_or_default().to_owned();
            g.terms.push((tok, why));
            let code = g
                .term_statuses
                .pop_front()
                .unwrap_or(StatusCode::NO_CONTENT);
            let body = if code.is_success() {
                String::new()
            } else {
                json!({ "error": "term rejected" }).to_string()
            };
            reply(code, body)
        }
        "/v1/mesh/send" => {
            g.sends.push(b);
            let code = g.send_statuses.pop_front().unwrap_or(StatusCode::OK);
            let body = if code.is_success() {
                json!({ "delivered": 1 }).to_string()
            } else {
                json!({ "error": "rejected" }).to_string()
            };
            reply(code, body)
        }
        _ => reply(StatusCode::NOT_FOUND, "{}".to_owned()),
    };
    drop(g);
    res
}

struct Rig {
    _dir: tempfile::TempDir,
    stub: StubCortex,
    server: tokio::task::JoinHandle<()>,
    model: Arc<OfflineModel>,
    cfg: PoolConfig,
    client: PoolClient,
    runner: RuntimeTurnRunner,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn write_token_file(dir: &Path, mode: u32) -> TestResult<PathBuf> {
    let path = dir.join("pool.token");
    std::fs::write(&path, format!("{SECRET}\n"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
    Ok(path)
}

async fn start_rig(deliveries: Vec<Value>) -> TestResult<Rig> {
    let dir = tempfile::tempdir()?;
    let token = PoolToken::load(&write_token_file(dir.path(), 0o600)?)?;
    let stub = StubCortex(Arc::new(Mutex::new(StubData {
        deliveries: deliveries.into(),
        ..StubData::default()
    })));
    let app = Router::new()
        .fallback(post(stub_post))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let cfg = PoolConfig {
        cortex_url: url,
        token,
        peer_id: PoolPeerId::new(PEER_ID)?,
        role: RoleName::new("service-pool-littleman-1")?,
        allowed_sender: RoleName::new("service-escapement-pool")?,
        wait_ms: 250,
        nack_delay_ms: 2_500,
        shell_id: ShellId::new("shell-alpha")?,
    };
    let client = PoolClient::new(&cfg)?;
    let model = Arc::new(OfflineModel::new());
    let lg_cfg = Config::for_owner(OwnerName::new("tester")?);
    let runner = RuntimeTurnRunner::open(
        &dir.path().join("lg.db"),
        &lg_cfg,
        Arc::clone(&model) as Arc<_>,
        cfg.shell_id.clone(),
    )?;
    Ok(Rig {
        _dir: dir,
        stub,
        server,
        model,
        cfg,
        client,
        runner,
    })
}

fn delivery(tok: &str, seq: u64, task_id: &str, body: &str) -> Value {
    json!({
        "delivery_token": tok,
        "stream_seq": seq,
        "lease_ms": 90,
        "delivery_attempt": 1,
        "message": {
            "kind": "msg",
            "from_peer": "22222222-2222-2222-2222-222222222222",
            "from_session": "service-escapement-pool",
            "subject": format!("task {task_id}"),
            "body": body,
            "refs": { "task_id": task_id, "attempt": 1 }
        }
    })
}

struct SlowRunner<R>(R, Duration);
impl<R: TurnRunner> TurnRunner for SlowRunner<R> {
    fn run_turn<'a>(
        &'a self,
        input: &'a TurnInput,
    ) -> BoxFuture<'a, Result<TurnOutput, TurnError>> {
        Box::pin(async move {
            tokio::time::sleep(self.1).await;
            self.0.run_turn(input).await
        })
    }
}

struct FailingRunner;
impl TurnRunner for FailingRunner {
    fn run_turn<'a>(&'a self, _: &'a TurnInput) -> BoxFuture<'a, Result<TurnOutput, TurnError>> {
        Box::pin(async { Err(TurnError::Transient("gateway overloaded".to_owned())) })
    }
}

#[tokio::test]
async fn happy_path_runs_turn_extends_lease_sends_reply_and_acks() -> TestResult {
    let payload = json!({
        "prompt": "summarize GHOST status",
        "ghost_binding": {
            "ghost_id": "sadr",
            "manifestation": "agent",
            "mitc_principal": "ctodie",
            "spiffe_seat": "spiffe://unsigned.gg/agent/platform/sadr",
            "molt_epoch": 3
        }
    });
    let rig = start_rig(vec![delivery(
        "tok-happy-1",
        42,
        "task-happy-1",
        &payload.to_string(),
    )])
    .await?;
    let mut worker = PoolWorker::new(
        rig.cfg.clone(),
        rig.client.clone(),
        SlowRunner(&rig.runner, Duration::from_millis(80)),
    );
    assert_eq!(worker.step().await?, StepOutcome::Settled);
    assert_eq!(rig.model.calls(), 1);

    let g = rig.stub.0.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(g.recvs[0].0.as_deref(), Some("wait_ms=250"));
    assert_eq!(g.recvs[0].1, json!({ "peer_id": PEER_ID, "free_slots": 1 }));
    assert!(g.extends.iter().any(|t| t == "tok-happy-1"));
    assert_eq!(g.acks, vec!["tok-happy-1"]);
    assert_eq!((g.nacks.len(), g.terms.len(), g.sends.len()), (0, 0, 1));
    let sent = &g.sends[0];
    assert_eq!(sent["peer_id"], PEER_ID);
    assert_eq!(sent["to"]["role"], "service-escapement-pool");
    assert_eq!(sent["message"]["kind"], "msg");
    assert_eq!(sent["message"]["from_peer"], PEER_ID);
    assert_eq!(sent["message"]["from_session"], "service-pool-littleman-1");
    assert_eq!(sent["message"]["subject"], "result task-happy-1 ok");
    let refs = &sent["message"]["refs"];
    assert_eq!(refs["task_id"], "task-happy-1");
    assert_eq!(refs["attempt"], 1);
    assert_eq!(refs["stream_seq"], 42);
    assert_eq!(refs["outcome"], "ok");
    assert_eq!(refs["ghost_id"], "sadr");
    assert_eq!(refs["molt_epoch"], 3);
    let sha = refs["transcript_sha256"].as_str().ok_or("missing sha")?;
    assert_eq!(sha.len(), 64);
    let body: Value = serde_json::from_str(sent["message"]["body"].as_str().ok_or("no body")?)?;
    assert_eq!(
        body["answer"],
        "littleguy (offline): you said 'summarize GHOST status'"
    );
    assert_eq!(body["shell_id"], "shell-alpha");
    assert_eq!(body["ghost_id"], "sadr");
    assert_eq!(body["molt_epoch"], 3);
    assert_eq!(body["transcript_sha256"], sha);
    drop(g);
    Ok(())
}

#[tokio::test]
async fn wrong_sender_is_refused_with_term_and_runs_no_turn() -> TestResult {
    let mut msg = delivery("tok-rogue-1", 7, "t-rogue", "unauthorized");
    msg["message"]["from_session"] = json!("session-rogue-lane");
    let rig = start_rig(vec![msg]).await?;
    let mut worker = PoolWorker::new(rig.cfg.clone(), rig.client.clone(), &rig.runner);
    assert_eq!(worker.step().await?, StepOutcome::Termed);
    assert_eq!(rig.model.calls(), 0);
    let g = rig.stub.0.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!((g.sends.len(), g.acks.len(), g.terms.len()), (0, 0, 1));
    assert_eq!(g.terms[0].0, "tok-rogue-1");
    assert!(g.terms[0].1.contains("sender"));
    drop(g);
    Ok(())
}

#[tokio::test]
async fn malformed_ghost_binding_and_oversized_body_are_termed_without_turn() -> TestResult {
    let bad = json!({
        "prompt": "embody invalid crew",
        "ghost_binding": {
            "ghost_id": "crew-bad",
            "manifestation": "crew",
            "mitc_principal": "ctodie",
            "spiffe_seat": "spiffe://unsigned.gg/agent/platform/crew-bad",
            "molt_epoch": 1
        }
    });
    let big = "x".repeat(MAX_DELIVERY_BODY_BYTES + 1);
    let rig = start_rig(vec![
        delivery("tok-bad-ghost", 9, "t-bad-ghost", &bad.to_string()),
        delivery("tok-oversized", 10, "t-big", &big),
    ])
    .await?;
    let mut worker = PoolWorker::new(rig.cfg.clone(), rig.client.clone(), &rig.runner);
    assert_eq!(worker.step().await?, StepOutcome::Termed);
    assert_eq!(worker.step().await?, StepOutcome::Termed);
    assert_eq!(rig.model.calls(), 0);
    let g = rig.stub.0.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!((g.sends.len(), g.acks.len(), g.terms.len()), (0, 0, 2));
    assert_eq!(g.terms[0].0, "tok-bad-ghost");
    assert!(g.terms[0].1.contains("ghost_binding"));
    assert_eq!(g.terms[1].0, "tok-oversized");
    drop(g);
    Ok(())
}

#[tokio::test]
async fn null_ghost_binding_is_treated_as_absent_and_runs_unbound_turn() -> TestResult {
    let body = json!({ "prompt": "unbound null parity", "ghost_binding": null });
    let rig = start_rig(vec![delivery("tok-null", 15, "t-null", &body.to_string())]).await?;
    let mut worker = PoolWorker::new(rig.cfg.clone(), rig.client.clone(), &rig.runner);
    assert_eq!(worker.step().await?, StepOutcome::Settled);
    assert_eq!(rig.model.calls(), 1);
    let g = rig.stub.0.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(g.acks, vec!["tok-null"]);
    let refs = &g.sends[0]["message"]["refs"];
    assert_eq!(refs["outcome"], "ok");
    assert!(refs.get("ghost_id").is_none() && refs.get("molt_epoch").is_none());
    drop(g);
    Ok(())
}

#[tokio::test]
async fn redelivery_resends_cached_reply_without_running_a_second_turn() -> TestResult {
    let rig = start_rig(vec![
        delivery("tok-dup-1", 77, "t-dup", "hello"),
        delivery("tok-dup-2", 77, "t-dup", "hello"),
    ])
    .await?;
    let mut worker = PoolWorker::new(rig.cfg.clone(), rig.client.clone(), &rig.runner);
    assert_eq!(worker.step().await?, StepOutcome::Settled);
    assert_eq!(rig.model.calls(), 1);
    assert_eq!(worker.step().await?, StepOutcome::ReplayedFromCache);
    assert_eq!(rig.model.calls(), 1);
    let g = rig.stub.0.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(g.acks, vec!["tok-dup-1", "tok-dup-2"]);
    assert_eq!(g.sends[0]["message"], g.sends[1]["message"]);
    drop(g);
    Ok(())
}

#[tokio::test]
async fn transient_turn_failure_nacks_with_delay() -> TestResult {
    let rig = start_rig(vec![delivery("tok-nack-1", 88, "t-nack", "retry me")]).await?;
    let mut worker = PoolWorker::new(rig.cfg.clone(), rig.client.clone(), FailingRunner);
    assert_eq!(worker.step().await?, StepOutcome::Nacked);
    let g = rig.stub.0.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(g.nacks, vec![("tok-nack-1".to_owned(), 2_500)]);
    drop(g);
    Ok(())
}

#[test]
fn token_file_mode_other_than_0600_or_wrong_owner_refuses_to_start_and_redacts() -> TestResult {
    let dir = tempfile::tempdir()?;
    let uid = rustix::process::geteuid().as_raw();
    let bad_mode = write_token_file(dir.path(), 0o644)?;
    assert!(matches!(
        PoolToken::load_for_uid(&bad_mode, uid),
        Err(PoolError::TokenFilePermissions { mode: 0o644 })
    ));
    let out = Command::new(LG)
        .args(["pool", "--cortex-url", "http://127.0.0.1:9", "--token-file"])
        .arg(&bad_mode)
        .args([
            "--peer-id",
            PEER_ID,
            "--wait-ms",
            "100",
            "--shell-id",
            "shell-cli",
        ])
        .env("LG_LOG", "warn")
        .output()?;
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("0600") && !stderr.contains(SECRET));

    let good = write_token_file(dir.path(), 0o600)?;
    let link = dir.path().join("token.link");
    std::os::unix::fs::symlink(&good, &link)?;
    assert!(matches!(
        PoolToken::load_for_uid(&link, uid),
        Err(PoolError::TokenFileNotRegular)
    ));
    assert!(matches!(
        PoolToken::load_for_uid(&good, uid.saturating_add(1)),
        Err(PoolError::TokenFileOwner { .. })
    ));
    let token = PoolToken::load(&good)?;
    assert_eq!(format!("{token:?}"), "PoolToken([REDACTED])");
    let bad = dir.path().join("bad.token");
    std::fs::write(&bad, format!("{SECRET} space\n"))?;
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o600))?;
    let err = PoolToken::load(&bad)
        .err()
        .ok_or("expected invalid token")?;
    assert!(!err.to_string().contains(SECRET) && !format!("{err:?}").contains(SECRET));
    Ok(())
}

struct PermanentFailRunner(String);
impl TurnRunner for PermanentFailRunner {
    fn run_turn<'a>(&'a self, _: &'a TurnInput) -> BoxFuture<'a, Result<TurnOutput, TurnError>> {
        let msg = self.0.clone();
        Box::pin(async move { Err(TurnError::Permanent(msg)) })
    }
}

#[tokio::test]
async fn bad_delivery_token_and_malformed_recv_do_not_stop_worker() -> TestResult {
    let rig = start_rig(vec![
        delivery("bad token spaces", 91, "t-bad-tok", "poison"),
        delivery("", 92, "t-empty-tok", "poison-empty"),
        delivery("tok-after-poison", 93, "t-good", "recover"),
    ])
    .await?;
    rig.stub
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .recv_raw
        .push_back((StatusCode::OK, "not-valid-json".to_owned()));
    let mut worker = PoolWorker::new(rig.cfg.clone(), rig.client.clone(), &rig.runner);
    assert!(matches!(worker.step().await, Err(PoolError::Decode(_))));
    assert_eq!(worker.step().await?, StepOutcome::Termed);
    assert_eq!(worker.step().await?, StepOutcome::Idle);
    assert_eq!(worker.step().await?, StepOutcome::Settled);
    assert_eq!(rig.model.calls(), 1);
    let g = rig.stub.0.lock().unwrap_or_else(PoisonError::into_inner);
    assert!(g.terms.iter().any(|(tok, _)| tok == "bad token spaces"));
    assert_eq!(g.acks, vec!["tok-after-poison"]);
    drop(g);
    Ok(())
}

#[tokio::test]
async fn non_retryable_422_on_send_settles_delivery_and_handles_next() -> TestResult {
    let rig = start_rig(vec![
        delivery("tok-422-1", 101, "t-422-1", "first"),
        delivery("tok-422-2", 102, "t-422-2", "second"),
    ])
    .await?;
    rig.stub
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .send_statuses
        .extend([StatusCode::UNPROCESSABLE_ENTITY, StatusCode::OK]);
    let mut worker = PoolWorker::new(rig.cfg.clone(), rig.client.clone(), &rig.runner);
    assert_eq!(worker.step().await?, StepOutcome::Termed);
    assert_eq!(worker.step().await?, StepOutcome::Settled);
    assert_eq!(rig.model.calls(), 2);
    let g = rig.stub.0.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(g.terms.len(), 1);
    assert_eq!(g.terms[0].0, "tok-422-1");
    assert!(
        g.terms[0].1.contains("send rejected") && g.terms[0].1.contains("422"),
        "unexpected term reason: {}",
        g.terms[0].1
    );
    assert_eq!(g.acks, vec!["tok-422-2"]);
    drop(g);
    Ok(())
}

#[tokio::test]
async fn permanent_failure_caps_refs_error_on_char_boundary() -> TestResult {
    let rig = start_rig(vec![delivery("tok-perm-1", 110, "t-perm-1", "fail")]).await?;
    let long_err = "é".repeat(1_250);
    let mut worker = PoolWorker::new(
        rig.cfg.clone(),
        rig.client.clone(),
        PermanentFailRunner(long_err),
    );
    assert_eq!(worker.step().await?, StepOutcome::Settled);
    let g = rig.stub.0.lock().unwrap_or_else(PoisonError::into_inner);
    let sent = &g.sends[0]["message"];
    let refs_err = sent["refs"]["error"].as_str().ok_or("missing refs.error")?;
    let body: Value = serde_json::from_str(sent["body"].as_str().ok_or("missing body")?)?;
    let body_err = body["error"].as_str().ok_or("missing body.error")?;
    assert_eq!(refs_err.chars().count(), 1_024);
    assert_eq!(refs_err, body_err);
    drop(g);
    Ok(())
}

#[test]
fn symlink_and_oversize_token_file_are_refused() -> TestResult {
    let dir = tempfile::tempdir()?;
    let good = write_token_file(dir.path(), 0o600)?;
    let link = dir.path().join("symlink.token");
    std::os::unix::fs::symlink(&good, &link)?;
    assert!(matches!(
        PoolToken::load(&link),
        Err(PoolError::TokenFileNotRegular)
    ));

    let oversize = dir.path().join("oversize.token");
    let huge = "t".repeat(5_000);
    std::fs::write(&oversize, format!("{huge}\n"))?;
    std::fs::set_permissions(&oversize, std::fs::Permissions::from_mode(0o600))?;
    let err = PoolToken::load(&oversize)
        .err()
        .ok_or("expected oversize token file to fail")?;
    assert!(!err.to_string().contains(&huge));
    let out = Command::new(LG)
        .args(["pool", "--cortex-url", "http://127.0.0.1:9", "--token-file"])
        .arg(&oversize)
        .args(["--peer-id", PEER_ID, "--shell-id", "shell-cli"])
        .env("LG_LOG", "warn")
        .output()?;
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains(&huge));
    Ok(())
}

#[test]
fn malformed_or_missing_peer_id_is_refused_at_startup() -> TestResult {
    assert!(PoolPeerId::new("not-a-uuid").is_err());
    assert!(PoolPeerId::new("11111111_1111_1111_1111_111111111111").is_err());
    assert!(PoolPeerId::new("11111111-1111-1111-1111-11111111111g").is_err());
    assert!(PoolPeerId::new(PEER_ID).is_ok());

    let dir = tempfile::tempdir()?;
    let tok = write_token_file(dir.path(), 0o600)?;
    let missing = Command::new(LG)
        .args(["pool", "--cortex-url", "http://127.0.0.1:9", "--token-file"])
        .arg(&tok)
        .args(["--shell-id", "shell-cli"])
        .env("LG_LOG", "warn")
        .output()?;
    assert!(!missing.status.success());

    let bad = Command::new(LG)
        .args(["pool", "--cortex-url", "http://127.0.0.1:9", "--token-file"])
        .arg(&tok)
        .args(["--peer-id", "not-a-uuid", "--shell-id", "shell-cli"])
        .env("LG_LOG", "warn")
        .output()?;
    assert!(!bad.status.success());
    Ok(())
}

#[tokio::test]
async fn term_403_on_poison_token_does_not_stop_worker() -> TestResult {
    let rig = start_rig(vec![
        delivery("bad token 403", 131, "t-poison-403", "poison"),
        delivery("tok-after-403", 132, "t-good-403", "recover-after-403"),
    ])
    .await?;
    rig.stub
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .term_statuses
        .push_back(StatusCode::FORBIDDEN);
    let mut worker = PoolWorker::new(rig.cfg.clone(), rig.client.clone(), &rig.runner);
    assert_eq!(worker.step().await?, StepOutcome::Termed);
    assert_eq!(worker.step().await?, StepOutcome::Settled);
    assert_eq!(rig.model.calls(), 1);
    let g = rig.stub.0.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(g.terms.len(), 1);
    assert_eq!(g.terms[0].0, "bad token 403");
    assert_eq!(g.acks, vec!["tok-after-403"]);
    drop(g);
    Ok(())
}

#[tokio::test]
async fn term_failure_after_422_send_keeps_cached_reply_for_redelivery() -> TestResult {
    let mut redelivered = delivery("tok-422-keep-2", 142, "t-422-keep", "compute-once");
    redelivered["delivery_attempt"] = json!(2);
    let rig = start_rig(vec![
        delivery("tok-422-keep-1", 141, "t-422-keep", "compute-once"),
        redelivered,
    ])
    .await?;
    {
        let mut g = rig.stub.0.lock().unwrap_or_else(PoisonError::into_inner);
        g.send_statuses
            .extend([StatusCode::UNPROCESSABLE_ENTITY, StatusCode::OK]);
        g.term_statuses.push_back(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let mut worker = PoolWorker::new(rig.cfg.clone(), rig.client.clone(), &rig.runner);
    assert_eq!(worker.step().await?, StepOutcome::Termed);
    assert_eq!(worker.step().await?, StepOutcome::ReplayedFromCache);
    assert_eq!(rig.model.calls(), 1);
    let g = rig.stub.0.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(g.terms.len(), 1);
    assert_eq!(g.terms[0].0, "tok-422-keep-1");
    assert_eq!(g.acks, vec!["tok-422-keep-2"]);
    assert_eq!(g.sends.len(), 2);
    drop(g);
    Ok(())
}

#[tokio::test]
async fn non_array_recv_body_returns_decode_error_and_backs_off() -> TestResult {
    let rig = start_rig(Vec::new()).await?;
    rig.stub
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .recv_fallback_raw = Some((StatusCode::OK, "{}".to_owned()));
    let mut worker = PoolWorker::new(rig.cfg.clone(), rig.client.clone(), &rig.runner);
    assert!(matches!(worker.step().await, Err(PoolError::Decode(_))));
    rig.stub
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .recvs
        .clear();
    worker
        .run_until(tokio::time::sleep(Duration::from_millis(180)))
        .await?;
    let recvs_len = rig
        .stub
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .recvs
        .len();
    assert!(
        (1..=2).contains(&recvs_len),
        "expected backoff to bound recv calls to 1..=2, got {recvs_len}"
    );
    Ok(())
}

#[tokio::test]
async fn retryable_5xx_on_send_nacks_and_replays_cached_reply() -> TestResult {
    let mut redelivered = delivery("tok-503-2", 152, "t-503", "compute-once");
    redelivered["delivery_attempt"] = json!(2);
    let rig = start_rig(vec![
        delivery("tok-503-1", 151, "t-503", "compute-once"),
        redelivered,
    ])
    .await?;
    rig.stub
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .send_statuses
        .extend([StatusCode::SERVICE_UNAVAILABLE, StatusCode::OK]);
    let mut worker = PoolWorker::new(rig.cfg.clone(), rig.client.clone(), &rig.runner);
    assert_eq!(worker.step().await?, StepOutcome::Nacked);
    assert_eq!(worker.step().await?, StepOutcome::ReplayedFromCache);
    assert_eq!(rig.model.calls(), 1);
    let g = rig.stub.0.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(g.nacks, vec![("tok-503-1".to_owned(), 2_500)]);
    assert_eq!(g.acks, vec!["tok-503-2"]);
    assert_eq!(g.sends.len(), 2);
    assert_eq!(g.sends[1]["message"]["refs"]["stream_seq"], json!(152));
    drop(g);
    Ok(())
}

#[test]
fn fifo_and_directory_token_paths_are_refused() -> TestResult {
    let dir = tempfile::tempdir()?;
    let subdir = dir.path().join("token.dir");
    std::fs::create_dir(&subdir)?;
    std::fs::set_permissions(&subdir, std::fs::Permissions::from_mode(0o600))?;
    assert!(matches!(
        PoolToken::load(&subdir),
        Err(PoolError::TokenFileNotRegular)
    ));
    std::fs::set_permissions(&subdir, std::fs::Permissions::from_mode(0o700))?;

    let fifo = dir.path().join("token.fifo");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )?;
    assert!(matches!(
        PoolToken::load(&fifo),
        Err(PoolError::TokenFileNotRegular)
    ));
    Ok(())
}
