//! MCP tools under the Rule of Two, end to end on the daemon.

use std::sync::atomic::Ordering;

use autumn_harvest_agent::ToolEffect;
use autumn_harvest_agent::flow::labels::{PRIVATE_DATA, UNTRUSTED_INPUT};
use littleguy_core::{McpServer, McpTransport};
use littleguy_tools::mcp::Vars;
use littleguy_tools::testing::{self, HttpServer};

use super::*;

fn server(id: &str, url: &str, labels: &[&str], effect: ToolEffect) -> McpServer {
    McpServer {
        id: id.to_owned(),
        transport: McpTransport::Http {
            url: url.to_owned(),
            bearer_env: None,
        },
        labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        effect,
        tools: None,
        timeout_secs: 10,
    }
}

async fn open_with_mcp(db: &Path, config: &Config, servers: &[McpServer]) -> Daemon {
    let vars: Vars = Arc::new(|_: &str| None);
    let found = littleguy_tools::mcp::connect(servers, &vars).await;
    assert!(found.failed.is_empty(), "{:?}", found.failed);
    let extras = Extras {
        mcp: found.tools,
        ..Extras::default()
    };
    Daemon::open_with(
        db,
        config,
        Arc::new(MemoryModel::default()),
        UID,
        false,
        extras,
    )
    .unwrap()
}

fn mcp_config() -> Config {
    let mut config = config();
    config.tools = littleguy_core::ToolsConfig {
        web_fetch: false,
        web_search: false,
        files: false,
        ..littleguy_core::ToolsConfig::default()
    };
    config
}

async fn http() -> HttpServer {
    testing::serve_http(None).await.unwrap()
}

fn waiting_id(response: &Response) -> i64 {
    match response {
        Response::Waiting { approval } => *approval,
        _ => panic!("the call does not wait"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_that_completes_the_rule_of_two_waits_then_runs_once_approved() {
    let dir = tempfile::tempdir().unwrap();
    let web = http().await;
    let mail = server(
        "mail",
        &web.url,
        &[UNTRUSTED_INPUT, PRIVATE_DATA],
        ToolEffect::External,
    );
    let mut daemon = open_with_mcp(&dir.path().join("lg.db"), &mcp_config(), &[mail]).await;

    let waiting = daemon
        .handle_request(UID, request("f-1", "use mail__count"))
        .await;
    let id = waiting_id(&waiting);
    assert_eq!(
        web.count.load(Ordering::SeqCst),
        0,
        "the call ran before approval"
    );

    let busy = daemon.handle_request(UID, request("f-2", "hello")).await;
    assert_eq!(busy, Response::Busy { approval: id });

    let listed = daemon.handle_request(UID, Request::Approvals).await;
    let Response::Approvals { entries } = listed else {
        panic!("the response is not a list of approvals");
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].tool, "mail__count");
    let stores = daemon.approvals.clone().unwrap();
    let record = stores.approval(id).unwrap().unwrap();
    assert_eq!(record.reason, crate::guard::FLOW_REASON);

    let answer = daemon.handle_request(UID, Request::Approve { id }).await;
    assert_eq!(reply_text(&answer), "got: 1");
    assert_eq!(web.count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_flow_call_never_reaches_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let web = http().await;
    let mail = server(
        "mail",
        &web.url,
        &[UNTRUSTED_INPUT, PRIVATE_DATA],
        ToolEffect::Write,
    );
    let mut daemon = open_with_mcp(&dir.path().join("lg.db"), &mcp_config(), &[mail]).await;
    let waiting = daemon
        .handle_request(UID, request("d-1", "use mail__count"))
        .await;
    let id = waiting_id(&waiting);
    let denied = daemon
        .handle_request(
            UID,
            Request::Deny {
                id,
                reason: Some(String::from("no")),
            },
        )
        .await;
    assert!(
        matches!(denied, Response::Reply { .. }),
        "a denial gets a reply"
    );
    assert_eq!(web.count.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn labels_from_an_earlier_read_hold_a_later_send() {
    let dir = tempfile::tempdir().unwrap();
    let web = http().await;
    let inbox = server(
        "inbox",
        &web.url,
        &[UNTRUSTED_INPUT, PRIVATE_DATA],
        ToolEffect::ReadOnly,
    );
    let out = server("out", &web.url, &[], ToolEffect::External);
    let mut daemon = open_with_mcp(&dir.path().join("lg.db"), &mcp_config(), &[inbox, out]).await;

    // A read alone never waits.
    let read = daemon
        .handle_request(UID, request("l-1", "use inbox__echo {\"text\":\"mail\"}"))
        .await;
    assert_eq!(reply_text(&read), "got: mail");

    // A send after it completes the three labels, so it waits.
    let send = daemon
        .handle_request(UID, request("l-2", "use out__count"))
        .await;
    let id = waiting_id(&send);
    assert_eq!(web.count.load(Ordering::SeqCst), 0);
    let answer = daemon.handle_request(UID, Request::Approve { id }).await;
    assert_eq!(reply_text(&answer), "got: 1");
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_rule_off_every_mcp_call_runs() {
    let dir = tempfile::tempdir().unwrap();
    let web = http().await;
    let mail = server(
        "mail",
        &web.url,
        &[UNTRUSTED_INPUT, PRIVATE_DATA],
        ToolEffect::External,
    );
    let mut config = mcp_config();
    config.flow.rule_of_two = false;
    let mut daemon = open_with_mcp(&dir.path().join("lg.db"), &config, &[mail]).await;
    let ran = daemon
        .handle_request(UID, request("o-1", "use mail__count"))
        .await;
    assert_eq!(reply_text(&ran), "got: 1");
    let refused = daemon.handle_request(UID, Request::Approvals).await;
    assert!(
        matches!(refused, Response::Error { .. }),
        "no call can wait"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_mcp_tool_answers_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let web = http().await;
    let tools = server("tools", &web.url, &[], ToolEffect::ReadOnly);
    let mut daemon = open_with_mcp(&dir.path().join("lg.db"), &mcp_config(), &[tools]).await;
    let failed = daemon
        .handle_request(UID, request("e-1", "use tools__fail"))
        .await;
    assert!(
        matches!(failed, Response::Reply { .. }),
        "the op still ends with an answer"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reflex_red_waits_for_approval_and_never_blocks_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let web = http().await;
    let out = server("out", &web.url, &[], ToolEffect::External);
    let vars: Vars = Arc::new(|_: &str| None);
    let found = littleguy_tools::mcp::connect(&[out], &vars).await;
    assert!(found.failed.is_empty());

    let mock_reflex = littleguy_stores::MockHttpTransport::new();
    mock_reflex
        .enqueue_json(
            200,
            &serde_json::json!({
                "tier": "RED",
                "policy": "vespertin/comms:send-external@v3",
                "basis": "human sign-off required"
            }),
        )
        .unwrap();
    mock_reflex
        .enqueue_json(
            200,
            &serde_json::json!({
                "tier": "NEVER",
                "policy": "protected",
                "basis": "sealed action"
            }),
        )
        .unwrap();

    let reflex = littleguy::reflex_gate::ReflexClassifyClient::with_transport(
        "http://127.0.0.1:7717",
        Arc::new(mock_reflex),
    )
    .unwrap();
    let mut config = mcp_config();
    config.flow.rule_of_two = false;
    let extras = Extras {
        mcp: found.tools,
        reflex: Some(reflex),
        ..Extras::default()
    };
    let mut daemon = Daemon::open_with(
        &dir.path().join("lg.db"),
        &config,
        Arc::new(MemoryModel::default()),
        UID,
        false,
        extras,
    )
    .unwrap();

    let waiting = daemon
        .handle_request(UID, request("rfx-1", "use out__count"))
        .await;
    let id = waiting_id(&waiting);
    assert_eq!(web.count.load(Ordering::SeqCst), 0);
    let approved = daemon.handle_request(UID, Request::Approve { id }).await;
    assert_eq!(reply_text(&approved), "got: 1");
    assert_eq!(web.count.load(Ordering::SeqCst), 1);

    let blocked = daemon
        .handle_request(UID, request("rfx-2", "use out__count"))
        .await;
    assert!(matches!(blocked, Response::Reply { .. }));
    assert_eq!(
        web.count.load(Ordering::SeqCst),
        1,
        "NEVER tier must block tool execution"
    );
}
