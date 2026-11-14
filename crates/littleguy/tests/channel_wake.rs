//! End-to-end proofs for Cortex channel-based wake in `lg serve` and `lg pool` (`specs/0007`, `REQ-LG-WAKE-01..05`).

use std::collections::VecDeque;
use std::error::Error;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use littleguy::pool::{
    PoolClient, PoolConfig, PoolToken, PoolWakeSource, PoolWorker, RuntimeTurnRunner,
};
use littleguy::wake::WakeListener;
use littleguy_core::{Config, OwnerName, PoolPeerId, RoleName, ShellId};
use littleguy_models::OfflineModel;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::{broadcast, oneshot};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const LG: &str = env!("CARGO_BIN_EXE_lg");
const SECRET: &str = "channel-wake-secret-token-778899";
const PEER_ID: &str = "22222222-2222-4222-8222-222222222222";
const WAKE_UUID: &str = "AB570176-0000-4000-8000-000000000004";

#[derive(Default)]
struct WakeStubData {
    deliveries: VecDeque<Value>,
    recvs: Vec<String>,
    acks: Vec<String>,
    sends: Vec<Value>,
    wake_404: bool,
    wake_connects: usize,
}

#[derive(Clone)]
struct WakeStubServer {
    data: Arc<Mutex<WakeStubData>>,
    tx: broadcast::Sender<String>,
}

impl WakeStubServer {
    fn new(wake_404: bool) -> Self {
        let (tx, _) = broadcast::channel(16);
        let data = WakeStubData {
            wake_404,
            ..WakeStubData::default()
        };
        Self {
            data: Arc::new(Mutex::new(data)),
            tx,
        }
    }

    fn enqueue(&self, task_id: &str, prompt: &str, seq: u64) {
        let item = json!({
            "delivery_token": format!("tok-{seq}"),
            "stream_seq": seq,
            "lease_ms": 30_000,
            "delivery_attempt": 1,
            "message": {
                "from_session": "service-escapement-pool",
                "subject": format!("task {task_id}"),
                "body": prompt,
                "refs": {
                    "task_id": task_id,
                    "attempt": 0
                }
            }
        });
        self.data
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .deliveries
            .push_back(item);
    }

    fn push_sse_wake(&self, payload: &str) {
        let _ = self.tx.send(payload.to_owned());
    }
}

async fn start_stub_server(
    stub: WakeStubServer,
) -> TestResult<(String, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let url = format!("http://{addr}");
    let handle = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let stub = stub.clone();
            tokio::spawn(async move {
                let _ = serve_http_conn(stream, stub).await;
            });
        }
    });
    Ok((url, handle))
}

async fn write_json_response(
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    status: &str,
    body: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    write.write_all(head.as_bytes()).await?;
    write.write_all(body.as_bytes()).await?;
    write.flush().await
}

async fn serve_sse_wake_conn(
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    query: &str,
    stub: &WakeStubServer,
) -> TestResult {
    if query != format!("peer_id={PEER_ID}") {
        write_json_response(write, "403 Forbidden", "{}").await?;
        return Ok(());
    }
    let wake_404 = {
        let mut g = stub.data.lock().unwrap_or_else(PoisonError::into_inner);
        g.wake_connects += 1;
        g.wake_404
    };
    if wake_404 {
        write_json_response(write, "404 Not Found", "{\"error\":\"not exposed\"}").await?;
        return Ok(());
    }
    let mut rx = stub.tx.subscribe();
    write
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n: ready\n\n",
        )
        .await?;
    write.flush().await?;
    while let Ok(payload) = rx.recv().await {
        let frame = format!("event: wake\ndata: {payload}\n\n");
        if write.write_all(frame.as_bytes()).await.is_err() || write.flush().await.is_err() {
            break;
        }
    }
    Ok(())
}

fn dispatch_stub_json(
    stub: &WakeStubServer,
    path: &str,
    query: &str,
    parsed: Value,
) -> (&'static str, String) {
    let mut g = stub.data.lock().unwrap_or_else(PoisonError::into_inner);
    match path {
        "/v1/mesh/pool/recv" => {
            g.recvs.push(query.to_owned());
            let item = g.deliveries.pop_front();
            drop(g);
            item.map_or_else(
                || ("200 OK", String::from("[]")),
                |entry| ("200 OK", Value::Array(vec![entry]).to_string()),
            )
        }
        "/v1/mesh/pool/ack" => {
            if let Some(arr) = parsed["delivery_tokens"].as_array() {
                g.acks
                    .extend(arr.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            drop(g);
            ("204 No Content", String::new())
        }
        "/v1/mesh/pool/extend" => {
            drop(g);
            ("204 No Content", String::new())
        }
        "/v1/mesh/send" => {
            g.sends.push(parsed);
            drop(g);
            ("200 OK", String::from("{\"delivered\":1}"))
        }
        _ => {
            drop(g);
            ("404 Not Found", String::from("{}"))
        }
    }
}

async fn serve_http_conn(stream: TcpStream, stub: WakeStubServer) -> TestResult {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut req_line = String::new();
    if reader.read_line(&mut req_line).await? == 0 {
        return Ok(());
    }
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();

    let mut auth_ok = false;
    let mut content_len = 0_usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            if key == "authorization" && val == format!("Bearer {SECRET}") {
                auth_ok = true;
            } else if key == "content-length" {
                content_len = val.parse::<usize>().unwrap_or(0);
            }
        }
    }
    let mut body_bytes = vec![0_u8; content_len];
    if content_len > 0 {
        reader.read_exact(&mut body_bytes).await?;
    }

    if !auth_ok {
        write_json_response(&mut write, "401 Unauthorized", "{}").await?;
        return Ok(());
    }

    let (path, query) = target
        .split_once('?')
        .map_or((target.as_str(), ""), |(p, q)| (p, q));

    if method == "GET" && path == "/v1/mesh/pool/wake" {
        return serve_sse_wake_conn(&mut write, query, &stub).await;
    }

    let parsed: Value = if body_bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body_bytes)?
    };
    let (status, resp_body) = dispatch_stub_json(&stub, path, query, parsed);
    write_json_response(&mut write, status, &resp_body).await?;
    Ok(())
}

fn write_token_file(dir: &Path) -> TestResult<std::path::PathBuf> {
    let token_path = dir.join("pool.token");
    std::fs::write(&token_path, format!("{SECRET}\n"))?;
    std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600))?;
    Ok(token_path)
}

fn build_pool_worker(
    dir: &Path,
    cortex_url: String,
    wait_ms: u32,
) -> TestResult<(PoolConfig, PoolWorker<RuntimeTurnRunner>)> {
    let token_path = write_token_file(dir)?;
    let token = PoolToken::load(&token_path)?;
    let cfg = PoolConfig {
        cortex_url,
        token,
        peer_id: PoolPeerId::new(PEER_ID)?,
        role: RoleName::new("service-pool-littleman-1")?,
        allowed_sender: RoleName::new("service-escapement-pool")?,
        wait_ms,
        nack_delay_ms: 1_000,
        shell_id: ShellId::new("ceres/littleman-pool-1")?,
    };
    let client = PoolClient::new(&cfg)?;
    let app_cfg = Config::for_owner(OwnerName::new("mark")?);
    let runner = RuntimeTurnRunner::open(
        &dir.join("pool.db"),
        &app_cfg,
        Arc::new(OfflineModel::new()),
        cfg.shell_id.clone(),
    )?;
    let worker = PoolWorker::new(cfg.clone(), client, runner);
    Ok((cfg, worker))
}

async fn send_unix_wake(path: &Path, uuid: &str) -> TestResult<String> {
    let mut stream = UnixStream::connect(path).await?;
    let frame = format!("{{\"message_id\":\"{uuid}\"}}\n");
    stream.write_all(frame.as_bytes()).await?;
    stream.shutdown().await?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await?;
    Ok(reply)
}

async fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    pred()
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_worker_parks_without_polling_and_wakes_immediately_on_unix_socket() -> TestResult {
    let dir = tempfile::tempdir()?;
    let stub = WakeStubServer::new(false);
    let (url, server_task) = start_stub_server(stub.clone()).await?;
    let (_, mut worker) = build_pool_worker(dir.path(), url, 5_000)?;

    let wake_sock = dir
        .path()
        .join("lane-wake")
        .join("ceres-littleman-pool-1.sock");
    let listener = WakeListener::bind(&wake_sock).await?;
    let wake_source = PoolWakeSource::new(true);
    let notify = wake_source.notify_handle();
    let sock_task = tokio::spawn(async move {
        listener
            .notify_until(notify, std::future::pending::<()>())
            .await;
    });

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let worker_task = tokio::spawn(async move {
        worker
            .run_until_with_wake(&wake_source, 60_000, async move {
                let _ = stop_rx.await;
            })
            .await
    });

    // Initial reconciliation pass runs once (`wait_ms=0`), then parks with zero extra polls.
    assert!(
        wait_until(Duration::from_secs(2), || {
            stub.data
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recvs
                .len()
                == 1
        })
        .await
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        stub.data
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recvs,
        vec![String::from("wait_ms=0")],
        "parked worker must issue zero extra HTTP polls"
    );

    // Enqueue a delivery and poke the Unix wake socket: worker wakes immediately and settles it.
    stub.enqueue("task-wake-1", "check wake path", 1);
    let reply = send_unix_wake(&wake_sock, WAKE_UUID).await?;
    assert_eq!(reply, "ok\n");

    assert!(
        wait_until(Duration::from_secs(3), || {
            stub.data
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .acks
                == vec![String::from("tok-1")]
        })
        .await
    );

    let _ = stop_tx.send(());
    worker_task.await??;
    sock_task.abort();
    server_task.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_sse_wake_stream_triggers_immediate_recv_and_404_falls_back_cleanly() -> TestResult {
    // 1. Live SSE `/v1/mesh/pool/wake` stream wakes parked worker immediately.
    let dir = tempfile::tempdir()?;
    let stub = WakeStubServer::new(false);
    let (url, server_task) = start_stub_server(stub.clone()).await?;
    let (cfg, mut worker) = build_pool_worker(dir.path(), url, 5_000)?;

    let wake_source = PoolWakeSource::new(false);
    let sse_task = wake_source.spawn_sse_listener(&cfg, std::future::pending::<()>())?;
    assert!(wait_until(Duration::from_secs(2), || wake_source.is_live()).await);

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let worker_task = tokio::spawn(async move {
        worker
            .run_until_with_wake(&wake_source, 60_000, async move {
                let _ = stop_rx.await;
            })
            .await
    });

    assert!(
        wait_until(Duration::from_secs(2), || {
            !stub
                .data
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recvs
                .is_empty()
        })
        .await
    );
    stub.enqueue("task-sse-1", "sse push delivery", 7);
    stub.push_sse_wake("{\"pool\":\"littleman\",\"stream_seq\":7}");

    assert!(
        wait_until(Duration::from_secs(3), || {
            stub.data
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .acks
                == vec![String::from("tok-7")]
        })
        .await
    );
    let _ = stop_tx.send(());
    worker_task.await??;
    sse_task.abort();
    server_task.abort();

    // 2. When `/v1/mesh/pool/wake` returns 404 (older Cortex server), worker falls back to `wait_ms` polling.
    let dir2 = tempfile::tempdir()?;
    let stub404 = WakeStubServer::new(true);
    stub404.enqueue("task-fallback-1", "legacy cortex fallback", 9);
    let (url404, server404_task) = start_stub_server(stub404.clone()).await?;
    let (cfg404, mut worker404) = build_pool_worker(dir2.path(), url404, 50)?;
    let wake404 = PoolWakeSource::new(false);
    let sse404_task = wake404.spawn_sse_listener(&cfg404, std::future::pending::<()>())?;

    let (stop404_tx, stop404_rx) = oneshot::channel::<()>();
    let wake404_worker = wake404.clone();
    let worker404_task = tokio::spawn(async move {
        worker404
            .run_until_with_wake(&wake404_worker, 60_000, async move {
                let _ = stop404_rx.await;
            })
            .await
    });

    assert!(
        wait_until(Duration::from_secs(3), || {
            stub404
                .data
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .acks
                == vec![String::from("tok-9")]
        })
        .await
    );
    assert!(!wake404.is_live());

    let _ = stop404_tx.send(());
    worker404_task.await??;
    sse404_task.abort();
    server404_task.abort();
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lg_serve_binary_accepts_wake_socket_and_deduplicates_uuid() -> TestResult {
    let dir = tempfile::tempdir()?;
    let cfg_path = dir.path().join("lg.toml");
    std::fs::write(&cfg_path, "owner = \"mark\"\n[tools]\nfiles = false\n")?;
    let db_path = dir.path().join("lg.db");
    let sock_path = dir.path().join("lg.sock");
    let wake_path = dir.path().join("lane-wake").join("estate-w5Z-pG.sock");
    let call_log = dir.path().join("calls.log");

    let child = Command::new(LG)
        .args([
            "--socket",
            sock_path.to_str().ok_or("bad sock path")?,
            "serve",
            "--config",
            cfg_path.to_str().ok_or("bad cfg path")?,
            "--db",
            db_path.to_str().ok_or("bad db path")?,
            "--model",
            "offline",
            "--wake-socket",
            wake_path.to_str().ok_or("bad wake path")?,
            "--offline-call-log",
            call_log.to_str().ok_or("bad call log")?,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let _guard = ChildGuard(child);

    assert!(
        wait_until(Duration::from_secs(5), || wake_path.exists()
            && sock_path.exists())
        .await
    );

    let first_reply = send_unix_wake(&wake_path, WAKE_UUID).await?;
    assert_eq!(first_reply, "ok\n");

    let expected_prompt = "New cortex mesh message ab570176-0000-4000-8000-000000000004. Read it with cortex__mesh_read.";
    assert!(
        wait_until(Duration::from_secs(3), || {
            std::fs::read_to_string(&call_log).is_ok_and(|s| s.contains(expected_prompt))
        })
        .await
    );
    let lines_after_first = std::fs::read_to_string(&call_log)?.lines().count();
    assert_eq!(lines_after_first, 1);

    // Duplicate wake with the same UUID replies `ok\n` and runs zero additional model turns.
    let dup_reply = send_unix_wake(&wake_path, WAKE_UUID).await?;
    assert_eq!(dup_reply, "ok\n");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let lines_after_dup = std::fs::read_to_string(&call_log)?.lines().count();
    assert_eq!(lines_after_dup, 1);

    Ok(())
}
