//! Subprocess integration proofs for `--api-key-file` / `LG_API_KEY_FILE` (`specs/0006`, `REQ-LG-CONT-01`).

use std::error::Error;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const LG: &str = env!("CARGO_BIN_EXE_lg");
const POOL_TOKEN: &str = "cortex-pool-token-445566";
const MODEL_KEY: &str = "sk-mounted-api-key-889900";
const PEER_ID: &str = "22222222-2222-2222-2222-222222222222";

#[derive(Default)]
struct RigState {
    delivered: bool,
    llm_auth_headers: Vec<String>,
    pool_sends: Vec<Value>,
}

fn write_secret_0600(dir: &Path, name: &str, value: &str) -> TestResult<std::path::PathBuf> {
    let path = dir.join(name);
    std::fs::write(&path, format!("{value}\n"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

async fn handle_post(
    State(state): State<Arc<Mutex<RigState>>>,
    headers: HeaderMap,
    uri: Uri,
    Json(body): Json<Value>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let reply =
        |code: StatusCode, payload: String| (code, [("content-type", "application/json")], payload);
    if uri.path() == "/v1/chat/completions" {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .llm_auth_headers
            .push(auth.clone());
        if auth != format!("Bearer {MODEL_KEY}") {
            return reply(
                StatusCode::UNAUTHORIZED,
                json!({"error": {"message": "bad model key"}}).to_string(),
            );
        }
        return reply(
            StatusCode::OK,
            json!({
                "choices": [{
                    "finish_reason": "stop",
                    "message": {
                        "role": "assistant",
                        "content": "mounted-key-ok"
                    }
                }],
                "usage": {"prompt_tokens": 8, "completion_tokens": 3}
            })
            .to_string(),
        );
    }

    let authed = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {POOL_TOKEN}"));
    if !authed {
        return reply(StatusCode::UNAUTHORIZED, "{}".to_owned());
    }

    let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
    let response = match uri.path() {
        "/v1/mesh/pool/recv" => {
            if guard.delivered {
                // Stop the worker cleanly on the second poll via 403 on recv.
                reply(StatusCode::FORBIDDEN, json!({"error": "done"}).to_string())
            } else {
                guard.delivered = true;
                let delivery = json!([{
                    "delivery_token": "tok-cont-01",
                    "stream_seq": 7,
                    "lease_ms": 5000,
                    "delivery_attempt": 1,
                    "message": {
                        "kind": "msg",
                        "from_session": "service-escapement-pool",
                        "subject": "mounted key smoke",
                        "body": "Reply with mounted-key-ok.",
                        "refs": {
                            "task_id": "task-cont-01",
                            "attempt": 0
                        }
                    }
                }]);
                reply(StatusCode::OK, delivery.to_string())
            }
        }
        "/v1/mesh/send" => {
            guard.pool_sends.push(body);
            reply(StatusCode::OK, json!({"ok": true}).to_string())
        }
        "/v1/mesh/pool/ack"
        | "/v1/mesh/pool/extend"
        | "/v1/mesh/pool/nack"
        | "/v1/mesh/pool/term" => reply(StatusCode::NO_CONTENT, String::new()),
        _ => reply(StatusCode::NOT_FOUND, "{}".to_owned()),
    };
    drop(guard);
    response
}

#[tokio::test]
async fn lg_pool_reads_openai_key_from_api_key_file_without_env_var() -> TestResult {
    let state = Arc::new(Mutex::new(RigState::default()));
    let app = Router::new()
        .route("/{*path}", post(handle_post))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir()?;
    let token_file = write_secret_0600(dir.path(), "pool-token", POOL_TOKEN)?;
    let api_key_file = write_secret_0600(dir.path(), "llm-key", MODEL_KEY)?;
    let db = dir.path().join("lg.db");
    let cfg = dir.path().join("lg.toml");
    std::fs::write(&cfg, "owner = \"nonroot\"\n")?;

    let base = format!("http://{addr}");
    let llm_base = format!("http://{addr}/v1");
    let output = tokio::task::spawn_blocking(move || {
        Command::new(LG)
            .env_remove("OPENAI_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENROUTER_API_KEY")
            .env("LG_API_KEY_FILE", &api_key_file)
            .args([
                "pool",
                "--cortex-url",
                &base,
                "--token-file",
                token_file.to_str().unwrap_or_default(),
                "--peer-id",
                PEER_ID,
                "--shell-id",
                "container-smoke-1",
                "--wait-ms",
                "50",
                "--config",
                cfg.to_str().unwrap_or_default(),
                "--db",
                db.to_str().unwrap_or_default(),
                "--model",
                "openai",
                "--model-name",
                "deepinfra/deepseek-v4.1-flash",
                "--base-url",
                &llm_base,
            ])
            .output()
    })
    .await??;

    server.abort();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains(MODEL_KEY), "stderr leaked model key");
    assert!(!stderr.contains(POOL_TOKEN), "stderr leaked pool token");

    let guard = state.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(
        guard.llm_auth_headers,
        vec![format!("Bearer {MODEL_KEY}")],
        "stderr was: {stderr}"
    );
    assert_eq!(guard.pool_sends.len(), 1, "stderr was: {stderr}");
    assert_eq!(guard.pool_sends[0]["message"]["refs"]["outcome"], "ok");
    drop(guard);
    Ok(())
}

#[test]
fn lg_pool_refuses_group_readable_api_key_file_and_redacts_stderr() -> TestResult {
    let dir = tempfile::tempdir()?;
    let token_file = write_secret_0600(dir.path(), "pool-token", POOL_TOKEN)?;
    let api_key_file = write_secret_0600(dir.path(), "llm-key", MODEL_KEY)?;
    std::fs::set_permissions(&api_key_file, std::fs::Permissions::from_mode(0o640))?;
    let db = dir.path().join("lg.db");
    let cfg = dir.path().join("lg.toml");
    std::fs::write(&cfg, "owner = \"nonroot\"\n")?;

    let output = Command::new(LG)
        .env_remove("OPENAI_API_KEY")
        .args([
            "pool",
            "--cortex-url",
            "http://127.0.0.1:9",
            "--token-file",
            token_file.to_str().unwrap_or_default(),
            "--peer-id",
            PEER_ID,
            "--shell-id",
            "container-smoke-2",
            "--config",
            cfg.to_str().unwrap_or_default(),
            "--db",
            db.to_str().unwrap_or_default(),
            "--model",
            "openai",
            "--model-name",
            "deepinfra/glm-5.2",
            "--api-key-file",
            api_key_file.to_str().unwrap_or_default(),
        ])
        .output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("0640"), "stderr was: {stderr}");
    assert!(!stderr.contains(MODEL_KEY), "stderr leaked model key");
    Ok(())
}
