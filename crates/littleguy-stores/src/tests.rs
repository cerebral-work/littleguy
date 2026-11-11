use std::collections::BTreeSet;
use std::future::Future;
use std::task::{Context, Poll, Waker};

use autumn_harvest_agent::AgentStop;
use autumn_harvest_agent::episode::{Episode, EpisodeStore, PassMark};
use autumn_harvest_agent::memory::{
    EditKey, InMemoryMemoryStore, MemoryOp, MemoryScope, MemoryStore,
};
use proptest::prelude::*;

use super::{SCHEMA_VERSION, SqliteStores, StoreError};

/// Runs a store future. Every store call is ready at once.
fn now<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("a store call waited"),
    }
}

fn scope() -> MemoryScope {
    MemoryScope::new("agent:assistant/mark")
}

fn add(text: &str) -> MemoryOp {
    MemoryOp::Add {
        block: String::from("memory"),
        text: String::from(text),
    }
}

fn remove(old: &str) -> MemoryOp {
    MemoryOp::Remove {
        block: String::from("memory"),
        old: String::from(old),
    }
}

fn entries(store: &SqliteStores) -> Vec<String> {
    let blocks = now(MemoryStore::load(store, &scope())).unwrap();
    blocks
        .into_iter()
        .find(|block| block.label == "memory")
        .unwrap()
        .entries
}

fn episode(id: &str) -> Episode {
    Episode {
        id: String::from(id),
        text: format!("the answer of {id}"),
        stop: AgentStop::Completed,
        at_ms: 1,
        labels: BTreeSet::new(),
    }
}

#[test]
fn a_memory_edit_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let store = SqliteStores::open(&path).unwrap();
    now(store.apply(&scope(), add("Mark drinks tea"), &EditKey::new("k1"))).unwrap();
    drop(store);

    let store = SqliteStores::open(&path).unwrap();
    assert_eq!(entries(&store), vec![String::from("Mark drinks tea")]);
    // A new scope starts with the default blocks.
    let other = now(MemoryStore::load(&store, &MemoryScope::new("other"))).unwrap();
    assert_eq!(other.len(), 2);
    assert!(other.iter().all(|block| block.entries.is_empty()));
}

#[test]
fn an_edit_key_applies_once() {
    let store = SqliteStores::open_in_memory().unwrap();
    now(store.apply(&scope(), add("a"), &EditKey::new("k1"))).unwrap();
    now(store.apply(&scope(), remove("a"), &EditKey::new("k2"))).unwrap();
    // The retry of k1 changes nothing and returns the block as it is.
    let retry = now(store.apply(&scope(), add("a"), &EditKey::new("k1"))).unwrap();
    assert_eq!(retry.entries, Vec::<String>::new());
    assert_eq!(entries(&store), Vec::<String>::new());
}

#[test]
fn a_failed_edit_keeps_its_failure() {
    let store = SqliteStores::open_in_memory().unwrap();
    let first = now(store.apply(&scope(), remove("tea"), &EditKey::new("k1"))).unwrap_err();
    now(store.apply(&scope(), add("tea"), &EditKey::new("k2"))).unwrap();
    // The entry exists now, but the retry of k1 gets its first answer.
    let retry = now(store.apply(&scope(), remove("tea"), &EditKey::new("k1"))).unwrap_err();
    assert_eq!(retry, first);
    assert_eq!(entries(&store), vec![String::from("tea")]);
}

#[test]
fn an_edit_to_an_unknown_block_is_refused() {
    let store = SqliteStores::open_in_memory().unwrap();
    let op = MemoryOp::Add {
        block: String::from("nope"),
        text: String::from("x"),
    };
    let error = now(store.apply(&scope(), op.clone(), &EditKey::new("k1"))).unwrap_err();
    assert!(error.message().contains("nope"));
    assert_eq!(
        now(store.apply(&scope(), op, &EditKey::new("k1"))).unwrap_err(),
        error
    );
}

#[test]
fn an_entry_keeps_its_flow_labels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let store = SqliteStores::open(&path).unwrap();
    let labels = BTreeSet::from([String::from("untrusted")]);
    now(store.apply_labelled(&scope(), add("from the web"), &EditKey::new("k1"), &labels)).unwrap();
    drop(store);

    let store = SqliteStores::open(&path).unwrap();
    let blocks = now(MemoryStore::load(&store, &scope())).unwrap();
    assert_eq!(
        autumn_harvest_agent::memory::snapshot_labels(&blocks),
        labels
    );
}

#[test]
fn a_memory_clear_removes_the_entries_and_the_keys() {
    let store = SqliteStores::open_in_memory().unwrap();
    now(store.apply(&scope(), add("a"), &EditKey::new("k1"))).unwrap();
    now(MemoryStore::clear(&store, &scope())).unwrap();
    assert_eq!(entries(&store), Vec::<String>::new());
    // The key is gone too, so the same edit applies again.
    now(store.apply(&scope(), add("a"), &EditKey::new("k1"))).unwrap();
    assert_eq!(entries(&store), vec![String::from("a")]);
}

#[test]
fn the_oldest_edit_keys_are_forgotten() {
    let store = SqliteStores::open_in_memory().unwrap();
    let total = super::MAX_EDIT_KEYS + 2;
    for n in 0..total {
        let key = EditKey::new(format!("k{n}"));
        now(store.apply(&scope(), add("same"), &key)).unwrap();
    }
    let kept: i64 = store
        .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM memory_edits", [], |row| row.get(0))?))
        .unwrap();
    assert_eq!(kept, super::MAX_EDIT_KEYS);
}

#[test]
fn the_episode_log_survives_a_reopen_and_appends_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let store = SqliteStores::open(&path).unwrap();
    let first = now(store.append(&scope(), &EditKey::new("a1"), episode("e1"), 10)).unwrap();
    let retry = now(store.append(&scope(), &EditKey::new("a1"), episode("e1"), 10)).unwrap();
    assert_eq!(first.pending, 1);
    assert_eq!(retry.pending, 1);
    drop(store);

    let store = SqliteStores::open(&path).unwrap();
    let log = now(EpisodeStore::load(&store, &scope())).unwrap();
    let ids: Vec<&str> = log.episodes().map(|episode| episode.id.as_str()).collect();
    assert_eq!(ids, vec!["e1"]);
}

#[test]
fn a_pass_marks_its_episodes_once() {
    let store = SqliteStores::open_in_memory().unwrap();
    now(store.append(&scope(), &EditKey::new("a1"), episode("e1"), 10)).unwrap();
    now(store.append(&scope(), &EditKey::new("a2"), episode("e2"), 10)).unwrap();
    let mark = PassMark {
        key: EditKey::new("p1"),
        generation: 0,
        episodes: vec![String::from("e1")],
        dropped: 0,
        facts: vec![String::from("Mark likes tea")],
    };
    assert!(now(store.mark(&scope(), mark.clone())).unwrap());
    assert!(now(store.mark(&scope(), mark)).unwrap());
    let log = now(EpisodeStore::load(&store, &scope())).unwrap();
    assert_eq!(log.pending_len(), 1);
    assert_eq!(log.derived().count(), 1);
}

#[test]
fn an_episode_clear_answers_a_retry_with_its_first_counts() {
    let store = SqliteStores::open_in_memory().unwrap();
    now(store.append(&scope(), &EditKey::new("a1"), episode("e1"), 10)).unwrap();
    let first = now(EpisodeStore::clear(&store, &scope(), &EditKey::new("c1"))).unwrap();
    let retry = now(EpisodeStore::clear(&store, &scope(), &EditKey::new("c1"))).unwrap();
    assert_eq!(first.episodes, 1);
    assert_eq!(retry, first);
    let log = now(EpisodeStore::load(&store, &scope())).unwrap();
    assert!(log.is_empty());
    assert_eq!(log.generation(), 1);
    // A pass that read the old generation marks nothing.
    let stale = PassMark {
        key: EditKey::new("p1"),
        generation: 0,
        episodes: vec![String::from("e1")],
        dropped: 0,
        facts: Vec::new(),
    };
    assert!(!now(store.mark(&scope(), stale)).unwrap());
}

#[test]
fn a_sent_report_is_remembered_across_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let store = SqliteStores::open(&path).unwrap();
    assert!(!store.was_sent("run:0").unwrap());
    store.mark_sent("run:0").unwrap();
    store.mark_sent("run:0").unwrap();
    drop(store);
    let store = SqliteStores::open(&path).unwrap();
    assert!(store.was_sent("run:0").unwrap());
    assert!(!store.was_sent("run:1").unwrap());
}

#[test]
fn a_file_of_the_first_schema_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    drop(conn);
    let store = SqliteStores::open(&path).unwrap();
    store.mark_sent("k").unwrap();
    assert!(store.was_sent("k").unwrap());
}

#[test]
fn a_newer_schema_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    drop(conn);
    let error = SqliteStores::open(&path).unwrap_err();
    assert!(matches!(error, StoreError::Schema { found } if found == SCHEMA_VERSION + 1));
}

#[test]
fn a_value_that_does_not_decode_is_a_decode_error() {
    let store = SqliteStores::open_in_memory().unwrap();
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO memory_scopes (scope, blocks) VALUES (?1, 'not json')",
                [scope().as_str()],
            )?;
            Ok(())
        })
        .unwrap();
    let error = now(MemoryStore::load(&store, &scope())).unwrap_err();
    assert!(error.message().contains("does not decode"));
}

#[test]
fn the_debug_output_shows_no_entry() {
    let store = SqliteStores::open_in_memory().unwrap();
    now(store.apply(&scope(), add("a secret"), &EditKey::new("k1"))).unwrap();
    assert!(!format!("{store:?}").contains("secret"));
}

/// One step of the conformance property.
#[derive(Debug, Clone)]
enum Step {
    Edit { op: MemoryOp, key: u8 },
    Clear,
}

fn step() -> impl Strategy<Value = Step> {
    let text = prop::sample::select(vec!["tea", "coffee", "tea and cake", " ", "x"]);
    let block = prop::sample::select(vec!["memory", "user", "nope"]);
    let op = (0..3_u8, block, text.clone(), text).prop_map(|(kind, block, old, text)| {
        let block = String::from(block);
        match kind {
            0 => MemoryOp::Add {
                block,
                text: String::from(text),
            },
            1 => MemoryOp::Replace {
                block,
                old: String::from(old),
                text: String::from(text),
            },
            _ => MemoryOp::Remove {
                block,
                old: String::from(old),
            },
        }
    });
    prop_oneof![
        9 => (op, 0..6_u8).prop_map(|(op, key)| Step::Edit { op, key }),
        1 => Just(Step::Clear),
    ]
}

proptest! {
    /// The SQLite store answers every edit as the engine's own store does.
    #[test]
    fn the_sqlite_store_matches_the_engine_store(steps in prop::collection::vec(step(), 1..40)) {
        let sqlite = SqliteStores::open_in_memory().unwrap();
        let engine = InMemoryMemoryStore::new();
        for step in steps {
            match step {
                Step::Edit { op, key } => {
                    let key = EditKey::new(format!("k{key}"));
                    let ours = now(sqlite.apply(&scope(), op.clone(), &key));
                    let theirs = now(engine.apply(&scope(), op, &key));
                    // An unknown block names the known blocks only in the
                    // engine message, so compare the outcome, then the block.
                    prop_assert_eq!(ours.is_ok(), theirs.is_ok());
                    if let (Ok(ours), Ok(theirs)) = (ours, theirs) {
                        prop_assert_eq!(ours, theirs);
                    }
                }
                Step::Clear => {
                    now(MemoryStore::clear(&sqlite, &scope())).unwrap();
                    now(MemoryStore::clear(&engine, &scope())).unwrap();
                }
            }
            prop_assert_eq!(
                now(MemoryStore::load(&sqlite, &scope())).unwrap(),
                now(MemoryStore::load(&engine, &scope())).unwrap()
            );
        }
    }
}

mod approvals {
    use serde_json::json;

    use crate::approvals::MAX_APPROVALS;
    use crate::{SqliteStores, Verdict};

    fn store() -> SqliteStores {
        SqliteStores::open_in_memory().unwrap()
    }

    #[test]
    fn a_record_is_claimed_decided_and_closed_once() {
        let stores = store();
        let args = json!({"runtime": "sh", "code": "echo hi"});
        let id = stores
            .record_approval("call-1", "run_code", &args, "ask")
            .unwrap();
        // Unclaimed records are not open.
        assert_eq!(stores.open_approvals().unwrap().len(), 0);
        // A decision needs a claim.
        assert!(!stores.set_verdict(id, Verdict::Approved).unwrap());

        let claim = stores
            .claim_approval("tool_approval:1:0:call-1", "call-1", Some("cli"))
            .unwrap()
            .unwrap();
        assert!(claim.fresh);
        assert_eq!(claim.record.id, id);
        assert_eq!(claim.record.arguments, args);
        assert_eq!(claim.record.requester.as_deref(), Some("cli"));

        // A second sight of the same wait finds the same record.
        let again = stores
            .claim_approval("tool_approval:1:0:call-1", "call-1", Some("other"))
            .unwrap()
            .unwrap();
        assert!(!again.fresh);
        assert_eq!(again.record.requester.as_deref(), Some("cli"));

        assert_eq!(stores.open_approvals().unwrap(), vec![again.record]);
        assert!(stores.set_verdict(id, Verdict::Approved).unwrap());
        assert!(!stores.set_verdict(id, Verdict::Denied).unwrap());
        assert_eq!(
            stores.approval(id).unwrap().unwrap().verdict,
            Some(Verdict::Approved)
        );

        stores.close_approval(id).unwrap();
        assert_eq!(stores.open_approvals().unwrap().len(), 0);
        assert!(stores.approval(id).unwrap().unwrap().closed);
        assert_eq!(stores.approval(id + 1).unwrap(), None);
    }

    #[test]
    fn a_wait_claims_the_newest_record_of_its_call() {
        let stores = store();
        let old = stores
            .record_approval("call-1", "run_code", &json!({"code": "old"}), "ask")
            .unwrap();
        stores
            .claim_approval("tool_approval:1:0:call-1", "call-1", None)
            .unwrap()
            .unwrap();
        stores.close_approval(old).unwrap();
        // The model used the same call id in a later round.
        let new = stores
            .record_approval("call-1", "run_code", &json!({"code": "new"}), "ask")
            .unwrap();
        let claim = stores
            .claim_approval("tool_approval:2:0:call-1", "call-1", None)
            .unwrap()
            .unwrap();
        assert_eq!(claim.record.id, new);
        assert_eq!(claim.record.arguments["code"], "new");
        assert_eq!(
            stores
                .claim_approval("tool_approval:3:0:other", "other", None)
                .unwrap(),
            None
        );
    }

    #[test]
    fn old_closed_records_are_dropped() {
        let stores = store();
        let first = stores
            .record_approval("c", "run_code", &json!({}), "ask")
            .unwrap();
        stores
            .claim_approval("tool_approval:0:0:c", "c", None)
            .unwrap()
            .unwrap();
        // An open record stays, however old.
        for _ in 0..MAX_APPROVALS {
            stores
                .record_approval("x", "run_code", &json!({}), "ask")
                .unwrap();
        }
        assert!(stores.approval(first).unwrap().is_some());
        stores.close_approval(first).unwrap();
        stores
            .record_approval("x", "run_code", &json!({}), "ask")
            .unwrap();
        // The closed record and the oldest unclaimed one are gone.
        assert!(stores.approval(first).unwrap().is_none());
        assert!(stores.approval(first + 1).unwrap().is_none());
        assert!(stores.approval(first + 2).unwrap().is_some());
    }

    #[test]
    fn a_version_two_file_gains_the_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE sent_reports (key TEXT PRIMARY KEY);")
                .unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let stores = SqliteStores::open(&path).unwrap();
        let id = stores
            .record_approval("c", "run_code", &json!({"a": 1}), "ask")
            .unwrap();
        assert_eq!(stores.approval(id).unwrap().unwrap().tool, "run_code");
    }
}

mod reverie_adapter {
    use std::collections::BTreeSet;

    use autumn_harvest_agent::ErrorKind;
    use autumn_harvest_agent::episode::{EpisodeStore, PassMark};
    use autumn_harvest_agent::memory::{EditKey, MemoryOp, MemoryScope, MemoryStore};
    use serde_json::json;

    use super::{add, episode, remove};
    use crate::{
        AdapterError, HttpMethod, MockHttpTransport, ReverieClientConfig,
        ReverieObservationBuilder, ReverieObservationKind, ReverieObservationStore, ReverieScope,
        ReverieSearchQuery, ReverieWriteMode, StoreHttpResponse, validate_topic_key,
    };

    #[test]
    fn config_and_topic_key_validation_rejects_malformed_inputs() {
        assert!(ReverieClientConfig::new("ftp://127.0.0.1:7437", "littleguy").is_err());
        assert!(
            ReverieClientConfig::new("http://user:secret@127.0.0.1:7437", "littleguy").is_err()
        );
        assert!(ReverieClientConfig::new("http://127.0.0.1:7437?bad=1", "littleguy").is_err());
        assert!(ReverieClientConfig::localhost_default("  ").is_err());
        assert!(
            ReverieClientConfig::localhost_default("littleguy")
                .unwrap()
                .with_smart_context_limit(0)
                .is_err()
        );

        for invalid_topic in [
            "",
            "single",
            "/leading/slash",
            "trailing/slash/",
            "littleguy/../escape",
            "Upper/Case",
            "space in/key",
        ] {
            assert!(
                validate_topic_key(invalid_topic).is_err(),
                "expected invalid topic_key `{invalid_topic}` to fail"
            );
        }
    }

    #[tokio::test]
    async fn search_smart_context_and_observation_builder_encode_without_interpolation() {
        let cfg = ReverieClientConfig::localhost_default("littleguy")
            .unwrap()
            .with_scope(ReverieScope::Project)
            .with_smart_context_limit(20)
            .unwrap()
            .with_agent_id("spiffe://unsigned.gg/agent/platform/lg-01")
            .unwrap()
            .with_subject("memory-knowledge")
            .unwrap();
        let mock = MockHttpTransport::new();
        let store = ReverieObservationStore::open_in_memory(cfg, mock.clone()).unwrap();

        mock.enqueue_json(
            200,
            &json!({
                "results": [{
                    "id": 101,
                    "title": "Mandate note",
                    "content": "spiffe://unsigned.gg",
                    "project": "littleguy",
                    "topic_key": "architecture/mandate",
                    "tags": [{"facet": "FlowLabel", "value": "untrusted_input"}]
                }]
            }),
        )
        .unwrap();

        let search_query = ReverieSearchQuery::new("a&project=injected#frag?x=1")
            .with_scope(ReverieScope::Project)
            .with_topic_prefix("architecture/mandate")
            .with_limit(5);
        let hits = store.search(&search_query).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].flow_labels(),
            BTreeSet::from([String::from("untrusted_input")])
        );

        mock.enqueue_json(
            200,
            &json!({
                "project": "littleguy",
                "context": "## Recent Decisions\n- Mandate active",
                "observations": []
            }),
        )
        .unwrap();
        let smart = store.smart_context(None, None).await.unwrap();
        assert!(smart.context.contains("Mandate active"));

        let recorded = mock.recorded_requests();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].method, HttpMethod::Get);
        assert_eq!(recorded[0].url.path(), "/search");
        let pairs: Vec<(String, String)> = recorded[0].url.query_pairs().into_owned().collect();
        assert!(pairs.contains(&(
            String::from("q"),
            String::from("a&project=injected#frag?x=1")
        )));
        assert!(pairs.contains(&(String::from("project"), String::from("littleguy"))));
        assert_eq!(recorded[0].url.fragment(), None);

        assert_eq!(recorded[1].url.path(), "/context/smart");
        let ctx_pairs: Vec<(String, String)> = recorded[1].url.query_pairs().into_owned().collect();
        assert!(ctx_pairs.contains(&(String::from("project"), String::from("littleguy"))));
        assert!(ctx_pairs.contains(&(String::from("limit"), String::from("20"))));
        assert!(ctx_pairs.contains(&(String::from("subject"), String::from("memory-knowledge"))));

        let built = ReverieObservationBuilder::new(
            "littleguy",
            "architecture/wave2-stores",
            "Wave 2 store layout",
            "Pluggable HTTP transport isolates offline tests.",
        )
        .kind(ReverieObservationKind::Architecture)
        .write_mode(ReverieWriteMode::Upsert)
        .evidence_link("specs/0003")
        .build()
        .unwrap();
        mock.enqueue_json(
            201,
            &json!({"id": 202, "topic_key": "architecture/wave2-stores"}),
        )
        .unwrap();
        let receipt = store.save_observation(&built).await.unwrap();
        assert_eq!(
            receipt.topic_key.as_deref(),
            Some("architecture/wave2-stores")
        );
    }

    #[tokio::test]
    async fn memory_and_episode_stores_enforce_edit_key_idempotency_retry_and_taint() {
        let cfg = ReverieClientConfig::localhost_default("littleguy").unwrap();
        let mock = MockHttpTransport::new();
        let store = ReverieObservationStore::open_in_memory(cfg, mock.clone()).unwrap();
        let sc = MemoryScope::new("agent:assistant/mark");

        // 1. Transient HTTP 503 fails with Unavailable and does NOT consume the EditKey.
        mock.enqueue(Ok(StoreHttpResponse::new(
            503,
            b"reveried restarting".to_vec(),
        )));
        let labels = BTreeSet::from([String::from("untrusted_input")]);
        let transient_err = store
            .apply_labelled(&sc, add("secret-note-alpha"), &EditKey::new("m1"), &labels)
            .await
            .unwrap_err();
        assert_eq!(transient_err.kind(), ErrorKind::Unavailable);

        // 2. Retry with the SAME EditKey succeeds once Reverie recovers (`200 OK`).
        let block = store
            .apply_labelled(&sc, add("secret-note-alpha"), &EditKey::new("m1"), &labels)
            .await
            .unwrap();
        assert_eq!(block.entries, vec![String::from("secret-note-alpha")]);
        assert!(!format!("{store:?}").contains("secret-note-alpha"));

        let count_after_success = mock.recorded_requests().len();
        assert_eq!(count_after_success, 2);
        let synced_json = mock.recorded_requests()[1].parse_json_body().unwrap();
        assert_eq!(synced_json["project"], "littleguy");
        assert_eq!(
            synced_json["topic_key"],
            "littleguy/memory/agent-assistant-mark/memory"
        );
        assert!(
            synced_json["tags"]
                .as_array()
                .unwrap()
                .contains(&json!({"facet": "FlowLabel", "value": "untrusted_input"}))
        );

        // 3. Replaying EditKey `m1` returns cached result with ZERO new HTTP requests.
        let replay = store
            .apply_labelled(&sc, add("secret-note-alpha"), &EditKey::new("m1"), &labels)
            .await
            .unwrap();
        assert_eq!(replay, block);
        assert_eq!(mock.recorded_requests().len(), count_after_success);

        // 4. Invalid memory edit (`Tool` error) caches failure locally with ZERO HTTP requests.
        let bad_op = MemoryOp::Add {
            block: String::from("missing_block"),
            text: String::from("x"),
        };
        let err1 = store
            .apply(&sc, bad_op.clone(), &EditKey::new("bad-1"))
            .await
            .unwrap_err();
        assert_eq!(err1.kind(), ErrorKind::Tool);
        assert_eq!(mock.recorded_requests().len(), count_after_success);
        let _ = remove("unused");

        // 5. EpisodeStore append, mark, and clear sync once per EditKey.
        mock.clear_recorded();
        let mut ep = episode("ep-1");
        ep.labels.insert(String::from("private_data"));
        let a1 = store
            .append(&sc, &EditKey::new("ea-1"), ep.clone(), 10)
            .await
            .unwrap();
        let a1_retry = store
            .append(&sc, &EditKey::new("ea-1"), ep, 10)
            .await
            .unwrap();
        assert_eq!(a1, a1_retry);
        assert_eq!(mock.recorded_requests().len(), 1);

        let mark = PassMark {
            key: EditKey::new("pm-1"),
            generation: 0,
            episodes: vec![String::from("ep-1")],
            dropped: 0,
            facts: vec![String::from("Mark prefers concise diffs")],
        };
        assert!(store.mark(&sc, mark.clone()).await.unwrap());
        assert!(store.mark(&sc, mark).await.unwrap());
        assert_eq!(mock.recorded_requests().len(), 2);

        let c1 = EpisodeStore::clear(&store, &sc, &EditKey::new("ec-1"))
            .await
            .unwrap();
        let c1_retry = EpisodeStore::clear(&store, &sc, &EditKey::new("ec-1"))
            .await
            .unwrap();
        assert_eq!(c1, c1_retry);
        assert_eq!(mock.recorded_requests().len(), 3);

        // Simulate transport network error mapping.
        mock.enqueue(Err(AdapterError::Transport {
            message: String::from("connection refused"),
        }));
        let clear_err = MemoryStore::clear(&store, &sc).await.unwrap_err();
        assert_eq!(clear_err.kind(), ErrorKind::Unavailable);
    }
}

mod xerj_adapter {
    use std::collections::BTreeSet;

    use autumn_harvest_agent::episode::{EpisodeStore, PassMark};
    use autumn_harvest_agent::memory::{EditKey, MemoryScope, MemoryStore};
    use serde_json::json;

    use super::{add, episode};
    use crate::{
        CYGNUS_XERJ_DEFAULT_URL, HttpMethod, MockHttpTransport, StoreHttpResponse,
        WatermarkDriftReason, WatermarkSyncDecision, XerjClientConfig, XerjEmbedMode,
        XerjFusionStrategy, XerjHybridQueryBuilder, XerjHybridSearchStore, XerjMemoryRecallQuery,
        compute_corpus_content_hash,
    };

    #[tokio::test]
    async fn cygnus_config_and_hybrid_query_builder_enforce_safety_invariants() {
        // Loopback `:9200` is rejected on shared hosts unless explicitly opted in.
        assert!(XerjClientConfig::new("http://127.0.0.1:9200", "littleguy").is_err());
        assert!(XerjClientConfig::new("http://localhost:9200", "littleguy").is_err());
        assert!(
            XerjClientConfig::new_allowing_loopback("http://127.0.0.1:9200", "littleguy").is_ok()
        );

        let cfg = XerjClientConfig::cygnus_default("littleguy-mem").unwrap();
        assert_eq!(
            cfg.base_url().as_str(),
            "http://xerj.tail769bd2.ts.net:9200/"
        );
        assert_eq!(cfg.embed_mode(), XerjEmbedMode::Proxy);
        assert_eq!(
            CYGNUS_XERJ_DEFAULT_URL,
            "http://xerj.tail769bd2.ts.net:9200"
        );

        // Reject path traversal / query injection in index names and `max_tokens < 32`.
        assert!(
            XerjHybridQueryBuilder::new("../_all", "Mandate")
                .build()
                .is_err()
        );
        assert!(
            XerjHybridQueryBuilder::new("soma-*?pretty=true", "Mandate")
                .build()
                .is_err()
        );
        assert!(
            XerjHybridQueryBuilder::new("soma-*", "Mandate")
                .max_tokens(16)
                .build()
                .is_err()
        );

        let query = XerjHybridQueryBuilder::new("soma-*", "SeatSubstrate lease_epoch")
            .fusion(XerjFusionStrategy::Rrf)
            .weights(1.0, 1.5)
            .limits(15, 5)
            .max_tokens(512)
            .term_filter("kind", "rust_fn")
            .local_judge("How does SeatSubstrate fence lease_epoch?", Some(0.35))
            .build()
            .unwrap();

        let mock = MockHttpTransport::new();
        mock.enqueue_json(
            200,
            &json!({
                "hits": {
                    "hits": [{
                        "_index": "soma-runtime-code",
                        "_id": "doc-1",
                        "_score": 0.91,
                        "_p_relevant": 0.88,
                        "_source": {"ax_path": "crates/escapement/src/seat.rs", "symbol": "SeatSubstrate"},
                        "fields": {"_passage": ["pub trait SeatSubstrate"]}
                    }]
                }
            }),
        )
        .unwrap();

        let store = XerjHybridSearchStore::open_in_memory(cfg, mock.clone()).unwrap();
        let resp = store.hybrid_search(&query).await.unwrap();
        assert_eq!(resp.hits().len(), 1);
        assert_eq!(
            resp.hits()[0].source["ax_path"],
            "crates/escapement/src/seat.rs"
        );

        let req = &mock.recorded_requests()[0];
        assert_eq!(req.method, HttpMethod::Post);
        assert_eq!(
            req.url.as_str(),
            "http://xerj.tail769bd2.ts.net:9200/soma-*/_search"
        );
        assert!(
            req.headers
                .contains(&(String::from("X-Xerj-Embed-Mode"), String::from("proxy")))
        );

        let wire = req.parse_json_body().unwrap();
        assert_eq!(wire["query"]["hybrid"]["fusion"], "rrf");
        assert_eq!(wire["fields"], json!(["_passage"]));
        assert_eq!(wire["max_tokens"], 512);
        assert_eq!(wire["size"], 5);
        assert_eq!(
            wire["query"]["hybrid"]["queries"][1]["query"]["semantic"]["k"],
            15
        );
    }

    #[tokio::test]
    async fn content_hash_and_commit_sha_watermarks_detect_every_drift_mode() {
        let docs_a = [
            ("src/lib.rs", b"pub fn alpha() -> u32 { 1 }".as_slice()),
            ("specs/0003.md", b"# Spec 0003".as_slice()),
        ];
        let docs_reordered = [
            ("specs/0003.md", b"# Spec 0003".as_slice()),
            ("src/lib.rs", b"pub fn alpha() -> u32 { 1 }".as_slice()),
        ];
        let docs_modified = [
            ("src/lib.rs", b"pub fn alpha() -> u32 { 2 }".as_slice()),
            ("specs/0003.md", b"# Spec 0003".as_slice()),
        ];

        let hash_a = compute_corpus_content_hash(&docs_a);
        let hash_reordered = compute_corpus_content_hash(&docs_reordered);
        let hash_modified = compute_corpus_content_hash(&docs_modified);
        assert_eq!(hash_a.len(), 64);
        assert_eq!(
            hash_a, hash_reordered,
            "corpus hash must be order-independent"
        );
        assert_ne!(hash_a, hash_modified, "1-byte edit must change corpus hash");

        let sha_1 = "09ad254000000000000000000000000000000001";
        let sha_2 = "cae597e000000000000000000000000000000002";

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("xerj-wm.db");
        let cfg = XerjClientConfig::cygnus_default("littleguy-mem").unwrap();
        let mock = MockHttpTransport::new();
        let store = XerjHybridSearchStore::open(&db_path, cfg.clone(), mock.clone()).unwrap();

        // 1. Remote 404 -> NeedsSync(MissingRemoteMeta).
        mock.enqueue(Ok(StoreHttpResponse::new(404, b"{}".to_vec())));
        let d1 = store
            .evaluate_watermark("lg-code-rust", sha_1, &hash_a)
            .await
            .unwrap();
        assert!(matches!(
            d1,
            WatermarkSyncDecision::NeedsSync {
                reason: WatermarkDriftReason::MissingRemoteMeta,
                ..
            }
        ));

        // 2. Advance watermark -> PUT /lg-code-rust/_mapping + local SQLite row.
        mock.enqueue_json(200, &json!({"acknowledged": true}))
            .unwrap();
        let wm = store
            .advance_watermark("lg-code-rust", sha_1, &hash_a, 1_760_000_000_000)
            .await
            .unwrap();
        assert_eq!(wm.commit_sha, sha_1);
        assert_eq!(wm.content_hash, hash_a);

        let put_req = mock.recorded_requests().last().unwrap().clone();
        assert_eq!(put_req.method, HttpMethod::Put);
        assert_eq!(put_req.url.path(), "/lg-code-rust/_mapping");
        let put_body = put_req.parse_json_body().unwrap();
        assert_eq!(put_body["_meta"]["commit_sha"], sha_1);
        assert_eq!(put_body["_meta"]["content_hash"], hash_a);

        // 3. Reopen SQLite file and verify UpToDate when remote _meta agrees.
        drop(store);
        let store = XerjHybridSearchStore::open(&db_path, cfg, mock.clone()).unwrap();
        let matching_remote = json!({
            "lg-code-rust": {
                "mappings": {
                    "_meta": {
                        "commit_sha": sha_1,
                        "content_hash": hash_a,
                        "updated_at_ms": 1_760_000_000_000_u64
                    }
                }
            }
        });
        mock.enqueue_json(200, &matching_remote).unwrap();
        let d2 = store
            .evaluate_watermark("lg-code-rust", sha_1, &hash_a)
            .await
            .unwrap();
        assert_eq!(d2, WatermarkSyncDecision::UpToDate { watermark: wm });

        // 4. Commit SHA changed -> CommitShaChanged.
        mock.enqueue_json(200, &matching_remote).unwrap();
        let d3 = store
            .evaluate_watermark("lg-code-rust", sha_2, &hash_a)
            .await
            .unwrap();
        assert!(matches!(
            d3,
            WatermarkSyncDecision::NeedsSync {
                reason: WatermarkDriftReason::CommitShaChanged,
                ..
            }
        ));

        // 5. Same commit SHA but dirty working-tree content hash -> ContentHashChanged.
        mock.enqueue_json(200, &matching_remote).unwrap();
        let d4 = store
            .evaluate_watermark("lg-code-rust", sha_1, &hash_modified)
            .await
            .unwrap();
        assert!(matches!(
            d4,
            WatermarkSyncDecision::NeedsSync {
                reason: WatermarkDriftReason::ContentHashChanged,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn xerj_memory_and_episode_stores_sync_and_recall_offline() {
        let cfg = XerjClientConfig::cygnus_default("littleguy-mem").unwrap();
        let mock = MockHttpTransport::new();
        let store = XerjHybridSearchStore::open_in_memory(cfg, mock.clone()).unwrap();
        let sc = MemoryScope::new("agent:assistant/mark");

        let labels = BTreeSet::from([String::from("untrusted_input")]);
        store
            .apply_labelled(
                &sc,
                add("sensitive-xerj-entry"),
                &EditKey::new("xk-1"),
                &labels,
            )
            .await
            .unwrap();
        // Idempotent retry emits no extra network request.
        store
            .apply_labelled(
                &sc,
                add("sensitive-xerj-entry"),
                &EditKey::new("xk-1"),
                &labels,
            )
            .await
            .unwrap();
        assert_eq!(mock.recorded_requests().len(), 1);
        assert_eq!(
            mock.recorded_requests()[0].url.path(),
            "/_memory/littleguy-mem"
        );
        assert!(!format!("{store:?}").contains("sensitive-xerj-entry"));

        store
            .append(&sc, &EditKey::new("xe-1"), episode("ep-1"), 10)
            .await
            .unwrap();
        let mark = PassMark {
            key: EditKey::new("xp-1"),
            generation: 0,
            episodes: vec![String::from("ep-1")],
            dropped: 0,
            facts: vec![String::from("fact-1")],
        };
        assert!(store.mark(&sc, mark).await.unwrap());
        EpisodeStore::clear(&store, &sc, &EditKey::new("xc-1"))
            .await
            .unwrap();

        mock.enqueue_json(
            200,
            &json!({
                "hits": [{
                    "id": "mem:agent:assistant/mark:memory",
                    "text": "sensitive-xerj-entry",
                    "score": 0.95,
                    "metadata": {"flow_labels": ["untrusted_input"]}
                }]
            }),
        )
        .unwrap();
        let recalled = store
            .recall_memories(&XerjMemoryRecallQuery::hybrid_rrf("xerj", 5))
            .await
            .unwrap();
        assert_eq!(recalled.len(), 1);
        assert_eq!(recalled[0].text, "sensitive-xerj-entry");
    }
}
