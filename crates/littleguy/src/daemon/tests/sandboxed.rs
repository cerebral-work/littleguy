//! The `run_code` tool and its approvals, end to end on the daemon.

use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_harvest::sandbox::{
    IsolationLevel, RunStatus, SandboxBackend, SandboxError, SandboxJob, SandboxOutcome,
};
use littleguy_channels::testing::FakeDiscord;
use littleguy_core::{ApprovalsConfig, SandboxBackend as BackendKind, SandboxConfig};
use tokio_util::sync::CancellationToken;

use super::*;

/// A backend that runs nothing. It counts its runs and prints `hi`.
#[derive(Debug, Default)]
struct CountingBackend {
    runs: AtomicUsize,
}

impl SandboxBackend for CountingBackend {
    fn isolation(&self) -> IsolationLevel {
        IsolationLevel::Process
    }

    fn execute(
        &self,
        _job: SandboxJob,
        _cancel: CancellationToken,
    ) -> autumn_harvest::sandbox::runtime::BoxFuture<'_, Result<SandboxOutcome, SandboxError>> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(Ok(SandboxOutcome {
            status: RunStatus::Exited { code: 0 },
            stdout: String::from("hi\n"),
            stderr: String::new(),
            stdout_bytes: 3,
            stderr_bytes: 0,
            stdout_truncated: false,
            stderr_truncated: false,
            outputs: Vec::new(),
            skipped_outputs: Vec::new(),
            duration_ms: 1,
            isolation: IsolationLevel::Process,
            image: None,
        })))
    }
}

fn sandbox_config(approver: Option<&str>, timeout: u64) -> Config {
    let mut config = config();
    config.tools.files = false;
    config.sandbox = Some(SandboxConfig {
        backend: BackendKind::Process,
        network: false,
    });
    config.approvals = ApprovalsConfig {
        approver: approver.map(|id| discord_sender(id).unwrap()),
        timeout_secs: timeout,
    };
    config
}

fn open_sandboxed(db: &Path, config: &Config, backend: &Arc<CountingBackend>) -> Daemon {
    let runtime = crate::sandbox::runtime_on(
        Arc::clone(backend) as Arc<dyn SandboxBackend>,
        config.sandbox.as_ref().unwrap(),
    )
    .unwrap();
    let extras = Extras {
        sandbox: Some(runtime),
        ..Extras::default()
    };
    Daemon::open_with(
        db,
        config,
        Arc::new(MemoryModel::default()),
        UID,
        !config.discord.allowed.is_empty(),
        extras,
    )
    .unwrap()
}

fn approval_entries(response: &Response) -> Vec<ApprovalEntryView> {
    match response {
        Response::Approvals { entries } => entries
            .iter()
            .map(|e| ApprovalEntryView {
                id: e.id,
                text: e.text.clone(),
            })
            .collect(),
        _ => panic!("the response is not a list of approvals"),
    }
}

#[derive(Debug)]
struct ApprovalEntryView {
    id: i64,
    text: String,
}

#[tokio::test(flavor = "multi_thread")]
async fn a_code_call_waits_then_runs_once_approved() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(CountingBackend::default());
    let mut daemon = open_sandboxed(
        &dir.path().join("lg.db"),
        &sandbox_config(None, 3600),
        &backend,
    );

    let waiting = daemon
        .handle_request(UID, request("s-1", "run echo hi"))
        .await;
    assert_eq!(waiting, Response::Waiting { approval: 1 });
    assert_eq!(backend.runs.load(Ordering::SeqCst), 0);

    // No new op runs while the request waits.
    let busy = daemon.handle_request(UID, request("s-2", "hello")).await;
    assert_eq!(busy, Response::Busy { approval: 1 });

    let listed = approval_entries(&daemon.handle_request(UID, Request::Approvals).await);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, 1);
    assert!(listed[0].text.contains("echo hi"), "{}", listed[0].text);

    // Only the owner decides.
    let refused = daemon
        .handle_request(UID + 1, Request::Approve { id: 1 })
        .await;
    assert!(matches!(refused, Response::Error { .. }));

    let answer = daemon.handle_request(UID, Request::Approve { id: 1 }).await;
    assert_eq!(
        answer,
        Response::Reply {
            text: String::from("ran: hi")
        }
    );
    assert_eq!(backend.runs.load(Ordering::SeqCst), 1);

    let empty = approval_entries(&daemon.handle_request(UID, Request::Approvals).await);
    assert_eq!(empty.len(), 0);
    let again = daemon.handle_request(UID, Request::Approve { id: 1 }).await;
    assert_eq!(
        again,
        Response::Error {
            message: String::from("request #1 does not wait")
        }
    );
    let missing = daemon
        .handle_request(
            UID,
            Request::Deny {
                id: 9,
                reason: None,
            },
        )
        .await;
    assert!(matches!(missing, Response::Error { message } if message == "no request #9"));

    // The assistant takes messages again.
    let next = daemon.handle_request(UID, request("s-3", "hello")).await;
    assert_eq!(reply_text(&next), "ok: hello");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_call_never_runs() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(CountingBackend::default());
    let mut daemon = open_sandboxed(
        &dir.path().join("lg.db"),
        &sandbox_config(None, 3600),
        &backend,
    );
    let waiting = daemon
        .handle_request(UID, request("d-1", "run rm -rf /"))
        .await;
    assert_eq!(waiting, Response::Waiting { approval: 1 });
    let denied = daemon
        .handle_request(
            UID,
            Request::Deny {
                id: 1,
                reason: Some(String::from("not that")),
            },
        )
        .await;
    assert!(
        matches!(denied, Response::Reply { .. }),
        "a denial gets a reply"
    );
    assert_ne!(reply_text(&denied), "ran: hi");
    assert_eq!(backend.runs.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wait_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lg.db");
    let config = sandbox_config(None, 3600);
    let backend = Arc::new(CountingBackend::default());
    {
        let mut daemon = open_sandboxed(&db, &config, &backend);
        let waiting = daemon.handle_request(UID, request("r-1", "run date")).await;
        assert_eq!(waiting, Response::Waiting { approval: 1 });
    }
    let mut daemon = open_sandboxed(&db, &config, &backend);
    let busy = daemon.handle_request(UID, request("r-2", "hello")).await;
    assert_eq!(busy, Response::Busy { approval: 1 });
    let answer = daemon.handle_request(UID, Request::Approve { id: 1 }).await;
    assert_eq!(reply_text(&answer), "ran: hi");
    assert_eq!(backend.runs.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deadline_denies_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(CountingBackend::default());
    let clock = HandClock::at("2026-11-09T10:00:00Z");
    let hand = clock.clone();
    let mut daemon = open_sandboxed(
        &dir.path().join("lg.db"),
        &sandbox_config(None, 60),
        &backend,
    )
    .with_clock(move || hand.now());
    let waiting = daemon.handle_request(UID, request("t-1", "run date")).await;
    assert_eq!(waiting, Response::Waiting { approval: 1 });
    daemon.tick().await.unwrap();
    assert_eq!(daemon.waiting, Some(1));

    clock.set("2026-11-09T10:02:00Z");
    tick_until(&mut daemon, || false).await;
    assert_eq!(daemon.waiting, None);
    assert_eq!(backend.runs.load(Ordering::SeqCst), 0);
    let late = daemon.handle_request(UID, Request::Approve { id: 1 }).await;
    assert!(
        matches!(late, Response::Error { .. }),
        "a late approval is refused"
    );
    let next = daemon.handle_request(UID, request("t-2", "hello")).await;
    assert_eq!(reply_text(&next), "ok: hello");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_sandbox_means_no_approvals() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::open(
        &dir.path().join("lg.db"),
        &config(),
        Arc::new(MemoryModel::default()),
        UID,
        false,
    )
    .unwrap();
    // The model asks for a tool that the assistant does not have.
    let answer = daemon.handle_request(UID, request("n-1", "run date")).await;
    assert!(
        matches!(answer, Response::Reply { .. }),
        "the assistant answers"
    );
    // The Rule of Two is on, but no tool brings private data. So no call
    // can wait.
    for request in [Request::Approvals, Request::Approve { id: 1 }] {
        let refused = daemon.handle_request(UID, request).await;
        assert!(
            matches!(&refused, Response::Error { message } if message.contains("[sandbox]")),
            "a daemon with no sandbox refuses approvals"
        );
    }
}

#[test]
fn a_firecracker_backend_with_missing_files_does_not_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config();
    config.sandbox = Some(SandboxConfig {
        backend: BackendKind::Firecracker(littleguy_core::FirecrackerConfig {
            firecracker: dir.path().join("firecracker"),
            jailer: dir.path().join("jailer"),
            kernel: dir.path().join("vmlinux"),
            rootfs: dir.path().join("rootfs.ext4"),
            uid: 990,
            gid: 990,
            netns: None,
        }),
        network: false,
    });
    let error = Daemon::open(
        &dir.path().join("lg.db"),
        &config,
        Arc::new(OfflineModel::new()),
        UID,
        false,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("firecracker"), "{error:#}");
}

#[test]
fn a_network_on_a_backend_without_one_is_refused() {
    let mut settings = sandbox_config(None, 60).sandbox.unwrap();
    settings.network = true;
    let error =
        crate::sandbox::runtime_on(Arc::new(CountingBackend::default()), &settings).unwrap_err();
    assert!(format!("{error:#}").contains("network"), "{error:#}");
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_buttons_decide_and_held_messages_run_after() {
    let fake = FakeDiscord::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("lg.sock");
    let uid = rustix::process::getuid().as_raw();
    let mut config = sandbox_config(Some("42"), 3600);
    config.discord.allowed = vec![discord_sender("42").unwrap(), discord_sender("43").unwrap()];
    let backend = Arc::new(CountingBackend::default());
    let runtime = crate::sandbox::runtime_on(
        Arc::clone(&backend) as Arc<dyn SandboxBackend>,
        config.sandbox.as_ref().unwrap(),
    )
    .unwrap();
    let extras = Extras {
        sandbox: Some(runtime),
        ..Extras::default()
    };
    let daemon = Daemon::open_with(
        &dir.path().join("lg.db"),
        &config,
        Arc::new(MemoryModel::default()),
        uid,
        true,
        extras,
    )
    .unwrap();
    let listener = bind(&socket).await.unwrap();
    let link = DiscordLink {
        rest: DiscordRest::new(fake.api_base.clone(), "token"),
        token: String::from("token"),
        gateway_url: fake.gateway_url.clone(),
    };
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let stop = async {
        let _ = stop_rx.await;
    };
    let contents = |fake: &FakeDiscord| -> Vec<String> {
        fake.sent().into_iter().map(|s| s.content).collect()
    };
    let script = async {
        assert!(fake.wait_for(10, |f| !f.identifies().is_empty()).await);
        // User 43 asks for code. The approver 42 gets the notice.
        fake.push_dm("7001", "dm43", "43", "run echo hi");
        assert!(fake.wait_for(10, |f| f.sent().len() == 2).await);
        let sent = fake.sent();
        let notice = sent.iter().find(|s| s.channel_id == "dm42").unwrap();
        assert!(notice.content.contains("echo hi"), "{}", notice.content);
        assert_eq!(notice.buttons, ["lg:approve:1", "lg:deny:1"]);
        let told = sent.iter().find(|s| s.channel_id == "dm43").unwrap();
        assert_eq!(told.content, crate::approvals::waiting_text(1));

        // A message that arrives now waits for the decision.
        fake.push_dm("7002", "dm43", "43", "hello");
        // The requester is not the approver, so a press by 43 changes nothing.
        fake.push_press("i-1", "43", "lg:approve:1");
        assert!(fake.wait_for(10, |f| f.callbacks().len() == 1).await);
        assert_eq!(fake.callbacks()[0].kind, 4);
        assert_eq!(backend.runs.load(Ordering::SeqCst), 0);
        assert_eq!(fake.sent().len(), 2);

        fake.push_press("i-2", "42", "lg:approve:1");
        assert!(fake.wait_for(10, |f| f.sent().len() == 4).await);
        let callbacks = fake.callbacks();
        assert_eq!(callbacks[1].kind, 7);
        assert!(callbacks[1].content.contains("Approved request #1"));
        let after: Vec<String> = contents(&fake)[2..].to_vec();
        assert_eq!(after, ["ran: hi", "ok: hello"]);

        // A second press on the same request does not run it again.
        fake.push_press("i-3", "42", "lg:approve:1");
        assert!(fake.wait_for(10, |f| f.callbacks().len() == 3).await);
        assert!(fake.callbacks()[2].content.contains("does not wait"));
        fake.push_press("i-4", "42", "other:button");
        assert!(fake.wait_for(10, |f| f.callbacks().len() == 4).await);
        assert_eq!(fake.callbacks()[3].kind, 4);
        stop_tx.send(()).unwrap();
    };
    let (served, ()) = tokio::join!(
        run(
            daemon,
            listener,
            Links {
                discord: Some(link),
                ..Links::default()
            },
            None,
            stop
        ),
        script
    );
    served.unwrap();
    assert_eq!(backend.runs.load(Ordering::SeqCst), 1);
    assert!(fake.sent()[2..].iter().all(|s| s.channel_id == "dm43"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cli_request_decided_in_discord_answers_the_approver() {
    let fake = FakeDiscord::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(CountingBackend::default());
    let config = sandbox_config(Some("42"), 3600);
    let mut daemon = open_sandboxed(&dir.path().join("lg.db"), &config, &backend);
    daemon.discord_rest(DiscordRest::new(fake.api_base.clone(), "token"));
    let waiting = daemon
        .handle_request(UID, request("c-1", "run echo hi"))
        .await;
    assert_eq!(waiting, Response::Waiting { approval: 1 });
    assert_eq!(fake.sent()[0].buttons, ["lg:approve:1", "lg:deny:1"]);

    let approver = littleguy_core::discord_sender("42").unwrap();
    let (answer, record) = daemon.press(&approver, "lg:deny:1");
    assert_eq!(
        answer,
        gate::PressAnswer::Update(String::from("Denied request #1."))
    );
    daemon
        .resume_after_press(&record.unwrap(), Service::Discord, "dm42")
        .await;
    let last = fake.sent().pop().unwrap();
    assert_eq!(last.channel_id, "dm42");
    assert_ne!(last.content, "ran: hi");
    assert_eq!(backend.runs.load(Ordering::SeqCst), 0);
}

#[test]
fn the_process_backend_opens_with_the_development_policy() {
    let settings = sandbox_config(None, 60).sandbox.unwrap();
    let runtime = crate::sandbox::runtime(&settings).unwrap();
    assert_eq!(runtime.isolation(), IsolationLevel::Process);
    assert_eq!(
        runtime.policy().network,
        autumn_harvest::sandbox::NetworkPolicy::Deny
    );
}

/// The approval flow over Slack: the notice, a stranger's press, the
/// approver's press, and the held message that runs after it.
#[tokio::test(flavor = "multi_thread")]
async fn a_slack_approver_decides_with_a_button() {
    use littleguy_channels::testing::slack::{APP_TOKEN, BOT_TOKEN, FakeSlack};
    use littleguy_core::slack_sender;

    let fake = FakeSlack::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("lg.sock");
    let uid = rustix::process::getuid().as_raw();
    let mut config = sandbox_config(None, 3600);
    config.approvals.approver = Some(slack_sender("U42").unwrap());
    config.slack.allowed = vec![slack_sender("U42").unwrap(), slack_sender("U43").unwrap()];
    let backend = Arc::new(CountingBackend::default());
    let runtime = crate::sandbox::runtime_on(
        Arc::clone(&backend) as Arc<dyn SandboxBackend>,
        config.sandbox.as_ref().unwrap(),
    )
    .unwrap();
    let extras = Extras {
        sandbox: Some(runtime),
        slack: true,
        ..Extras::default()
    };
    let daemon = Daemon::open_with(
        &dir.path().join("lg.db"),
        &config,
        Arc::new(MemoryModel::default()),
        uid,
        false,
        extras,
    )
    .unwrap();
    let listener = bind(&socket).await.unwrap();
    let links = Links {
        slack: Some(SlackApi::new(fake.api_base.clone(), BOT_TOKEN, APP_TOKEN)),
        ..Links::default()
    };
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let stop = async {
        let _ = stop_rx.await;
    };
    let script = async {
        assert!(fake.wait_for(10, |f| f.connects() == 1).await);
        // User U43 asks for code. The approver U42 gets the notice.
        fake.push_dm("7001.000000", "DU43", "U43", "run echo hi");
        assert!(fake.wait_for(10, |f| f.posted().len() == 2).await);
        let posted = fake.posted();
        let notice = posted.iter().find(|p| p.channel == "DU42").unwrap();
        assert!(notice.text.contains("echo hi"), "{}", notice.text);
        assert_eq!(notice.buttons, ["lg:approve:1", "lg:deny:1"]);
        let told = posted.iter().find(|p| p.channel == "DU43").unwrap();
        assert_eq!(told.text, crate::approvals::waiting_text(1));

        // A message that arrives now waits for the decision.
        fake.push_dm("7002.000000", "DU43", "U43", "hello");
        // The requester is not the approver, so a press by U43 changes nothing.
        fake.push_press("U43", "lg:approve:1", "1001.000000");
        assert!(fake.wait_for(10, |f| f.answers().len() == 1).await);
        assert_eq!(fake.answers()[0].kind, "ephemeral");
        assert_eq!(backend.runs.load(Ordering::SeqCst), 0);
        assert_eq!(fake.posted().len(), 2);

        fake.push_press("U42", "lg:approve:1", "1001.000000");
        assert!(fake.wait_for(10, |f| f.posted().len() == 4).await);
        let answers = fake.answers();
        assert_eq!(answers[1].kind, "update");
        assert_eq!(answers[1].target, "1001.000000");
        assert!(answers[1].text.contains("Approved request #1"));
        let after: Vec<String> = fake.posted()[2..].iter().map(|p| p.text.clone()).collect();
        assert_eq!(after, ["ran: hi", "ok: hello"]);
        stop_tx.send(()).unwrap();
    };
    let (served, ()) = tokio::join!(run(daemon, listener, links, None, stop), script);
    served.unwrap();
    assert_eq!(backend.runs.load(Ordering::SeqCst), 1);
    assert!(fake.posted()[2..].iter().all(|p| p.channel == "DU43"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cortex_wake_holds_during_approval_wait_and_runs_after_decision() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(CountingBackend::default());
    let mut daemon = open_sandboxed(
        &dir.path().join("lg.db"),
        &sandbox_config(None, 3600),
        &backend,
    );
    let waiting = daemon
        .handle_request(UID, request("w-1", "run echo hi"))
        .await;
    assert_eq!(waiting, Response::Waiting { approval: 1 });

    let wake_id = WakeMessageId::new("AB570176-0000-4000-8000-000000000004").unwrap();
    let held_resp = daemon.wake(&wake_id).await.unwrap();
    assert_eq!(held_resp, Response::Busy { approval: 1 });
    // Duplicate wake while waiting is queued only once.
    let dup_held = daemon.wake(&wake_id).await.unwrap();
    assert_eq!(dup_held, Response::Busy { approval: 1 });

    let decided = daemon.handle_request(UID, Request::Approve { id: 1 }).await;
    assert_eq!(reply_text(&decided), "ran: hi");
    daemon.release_held().await;

    let state = daemon.state().await.unwrap().unwrap();
    assert_eq!(
        state.last_answer.as_deref(),
        Some(
            "ok: New cortex mesh message ab570176-0000-4000-8000-000000000004. Read it with cortex__mesh_read."
        )
    );
    // Sending the same wake UUID again deduplicates via autumn-harvest.
    let dup_after = daemon.wake(&wake_id).await.unwrap();
    assert!(matches!(dup_after, Response::Duplicate { .. }));
}
