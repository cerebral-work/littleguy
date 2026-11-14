//! The approval flow of the daemon.
//!
//! After each drive, [`Daemon::settle`] compares the wait of the run with
//! the open records. A record whose wait ended closes, and its answer goes
//! to the requester. A new wait claims its record, and the approver gets a
//! notice. A decision sends the engine signal, then drives the run on.

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use autumn_harvest_agent::approval::approval_call_id;
use autumn_harvest_agent::{AgentState, sqlite};
use autumn_harvest_sqlite::RunState;
use littleguy_channels::{ChannelError, Service};
use littleguy_core::SenderId;
use littleguy_stores::{ApprovalRecord, SqliteStores, Verdict};
use serde_json::Value;

use super::Daemon;
use crate::approvals::{self, Choice, Requester};
use crate::chats::Chats;
use crate::protocol::{ApprovalEntry, Response};

/// The reason of a placeholder record, for a wait with no record.
const UNRECORDED: &str = "the call of this wait was not recorded";

/// The answer to a button press, sent before any slow work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PressAnswer {
    /// Replace the message, and take its buttons away.
    Update(String),
    /// Tell only the presser. The buttons stay.
    Private(String),
}

/// The sent-report key of the notice of `id`.
fn notice_key(id: i64) -> String {
    format!("approval-notice:{id}")
}

/// Closes `record`, whose wait ended, and sends its answer to a chat
/// requester. `lg approve` reads the answer of a CLI requester itself.
/// `state` reads the assistant, and `chats` sends the answer.
async fn finish(
    stores: &SqliteStores,
    record: &ApprovalRecord,
    state: impl Future<Output = autumn_harvest_sqlite::SqliteResult<Option<AgentState>>>,
    chats: &Chats,
) -> Result<()> {
    stores.close_approval(record.id)?;
    let outcome = match record.verdict {
        Some(Verdict::Approved) => "approved",
        Some(Verdict::Denied) => "denied",
        None => "timed out",
    };
    tracing::info!(approval = record.id, outcome, "a request ended");
    let Some(Requester::Chat(service, channel)) = Requester::decode(record.requester.as_deref())
    else {
        return Ok(());
    };
    let answer = state
        .await?
        .and_then(|state| state.last_answer)
        .unwrap_or_else(|| format!("Request #{} ended without an answer.", record.id));
    if let Err(error) = chats.send(service, &channel, &answer).await {
        tracing::warn!(%error, approval = record.id, "the answer of a request did not go out");
    }
    Ok(())
}

/// Sends the notice of `record` to `approver`. Returns whether the notice
/// is done: sent, or not needed because nobody gets one.
async fn notify(approver: Option<SenderId>, chats: &Chats, record: &ApprovalRecord) -> bool {
    let Some(approver) = approver else {
        tracing::info!(
            approval = record.id,
            "a request waits: run `lg approvals`, then `lg approve` or `lg deny`"
        );
        return true;
    };
    // The chat client arrives after the open. A later settle tries again.
    let sent = chats
        .dm(
            &approver,
            &approvals::notice(record),
            &approvals::buttons(record.id),
        )
        .await;
    match sent {
        Ok(()) => true,
        // The client is not there yet. The next settle tries again.
        Err(ChannelError::Off(_)) => false,
        Err(error) => {
            tracing::warn!(%error, approval = record.id, "the approval notice did not go out");
            false
        }
    }
}

impl Daemon {
    /// The store of the calls that wait, or the error text for a daemon
    /// where no call can wait.
    fn approval_store(&self) -> Result<Arc<SqliteStores>, String> {
        self.approvals.clone().ok_or_else(|| {
            String::from(
                "no call waits for approval in this daemon: it has no [sandbox] section, \
                 and its tools cannot complete a flow rule",
            )
        })
    }

    /// Matches the open records to the wait of `run`.
    ///
    /// Each open record that the run no longer waits on closes, and its
    /// answer goes to its requester. A new wait claims its record for
    /// `requester`, and the approver gets a notice once. Returns the
    /// request that the run waits on.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub(super) async fn settle(
        &mut self,
        run: Option<&RunState>,
        requester: Option<&Requester>,
    ) -> Result<Option<i64>> {
        let Some(stores) = self.approvals.clone() else {
            return Ok(None);
        };
        let wait = match run {
            Some(RunState::WaitingSignal(signal)) => {
                approval_call_id(signal).map(|call| (signal.clone(), call.to_owned()))
            }
            _ => None,
        };
        for record in stores.open_approvals()? {
            let ended = wait.as_ref().map(|(signal, _)| signal) != record.signal.as_ref();
            if ended {
                let state = sqlite::state(&self.rt, self.routing.assistant());
                let chats = Arc::clone(&self.chats);
                // The state query is a large future, so it lives on the heap.
                Box::pin(finish(&stores, &record, state, &chats)).await?;
            }
        }
        let Some((signal, call)) = wait else {
            self.waiting = None;
            return Ok(None);
        };
        let requester = requester.map(Requester::encode);
        let claim =
            if let Some(claim) = stores.claim_approval(&signal, &call, requester.as_deref())? {
                claim
            } else {
                // The policy always records first. A lost record still needs
                // an answer, and it can only be denied.
                stores.record_approval(&call, "unknown", &Value::Null, UNRECORDED)?;
                stores
                    .claim_approval(&signal, &call, requester.as_deref())?
                    .context("the approval record did not save")?
            };
        let id = claim.record.id;
        let approver = self.approver.clone();
        let chats = Arc::clone(&self.chats);
        if !stores.was_sent(&notice_key(id))? && notify(approver, &chats, &claim.record).await {
            stores.mark_sent(&notice_key(id))?;
        }
        self.waiting = Some(id);
        Ok(Some(id))
    }

    /// The open records, for `lg approvals`.
    pub(super) fn approval_list(&self) -> Result<Response> {
        let stores = match self.approval_store() {
            Ok(stores) => stores,
            Err(message) => return Ok(Response::Error { message }),
        };
        let entries = stores
            .open_approvals()?
            .into_iter()
            .filter(|record| record.verdict.is_none())
            .map(|record| ApprovalEntry {
                id: record.id,
                tool: record.tool.clone(),
                text: approvals::describe(&record),
            })
            .collect();
        Ok(Response::Approvals { entries })
    }

    /// Sends `choice` on request `id` to the run. Returns the record, or
    /// the text for the person when the request does not wait.
    ///
    /// # Errors
    ///
    /// Returns an error when the store or the runtime fails.
    pub(super) fn decide(
        &mut self,
        id: i64,
        choice: &Choice,
    ) -> Result<Result<ApprovalRecord, String>> {
        let stores = match self.approval_store() {
            Ok(stores) => stores,
            Err(text) => return Ok(Err(text)),
        };
        let Some(record) = stores.approval(id)? else {
            return Ok(Err(format!("no request #{id}")));
        };
        let Some(signal) = record.signal.clone().filter(|_| !record.closed) else {
            return Ok(Err(format!("request #{id} does not wait")));
        };
        if record.verdict.is_some() {
            return Ok(Err(format!("request #{id} is decided already")));
        }
        let payload = match approvals::payload(&record, choice) {
            Ok(payload) => payload,
            Err(text) => return Ok(Err(text)),
        };
        let exec = sqlite::current_run(&self.rt, self.routing.assistant())?
            .context("the assistant has no running run")?;
        sqlite::decide(&mut self.rt, exec, &signal, &payload)?;
        let verdict = match choice {
            Choice::Approve => Verdict::Approved,
            Choice::Deny(_) => Verdict::Denied,
        };
        stores.set_verdict(id, verdict)?;
        tracing::info!(approval = id, ?verdict, "a request was decided");
        Ok(Ok(record))
    }

    /// Drives the run on after a decision on `record`. A CLI requester gets
    /// the answer. A new wait gets its number.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime or the store fails.
    pub(super) async fn resume(&mut self, record: &ApprovalRecord) -> Result<Response> {
        let id = self.routing.assistant().clone();
        let run = sqlite::drive(&mut self.rt, &id).await?;
        let requester = Requester::decode(record.requester.as_deref());
        if let Some(next) = self.settle(run.as_ref(), requester.as_ref()).await? {
            return Ok(Response::Waiting { approval: next });
        }
        if requester != Some(Requester::Cli) {
            return Ok(Response::Decided {
                approval: record.id,
            });
        }
        let answer = sqlite::state(&self.rt, &id)
            .await?
            .and_then(|s| s.last_answer);
        Ok(answer.map_or_else(
            || Response::NoAnswer {
                reason: String::from("the op ended without an answer"),
            },
            |text| Response::Reply { text },
        ))
    }

    /// `lg approve` and `lg deny`: decide, then drive the run on.
    pub(super) async fn decide_and_resume(&mut self, id: i64, choice: Choice) -> Result<Response> {
        match self.decide(id, &choice)? {
            Ok(record) => self.resume(&record).await,
            Err(message) => Ok(Response::Error { message }),
        }
    }

    /// Handles one button press by `user`. Only the approver may decide.
    /// Returns the answer for the chat, and the record to drive on after it.
    pub(super) fn press(
        &mut self,
        user: &SenderId,
        custom_id: &str,
    ) -> (PressAnswer, Option<ApprovalRecord>) {
        let Some((choice, id)) = approvals::parse_custom_id(custom_id) else {
            return (
                PressAnswer::Private(String::from("This button is not one of mine.")),
                None,
            );
        };
        if self.approver.as_ref() != Some(user) {
            tracing::warn!(%user, approval = id, "a stranger pressed an approval button");
            return (
                PressAnswer::Private(String::from("Only the approver can decide this request.")),
                None,
            );
        }
        match self.decide(id, &choice) {
            Ok(Ok(record)) => {
                let done = match choice {
                    Choice::Approve => format!("Approved request #{id}. It runs now."),
                    Choice::Deny(_) => format!("Denied request #{id}."),
                };
                (PressAnswer::Update(done), Some(record))
            }
            Ok(Err(text)) => (PressAnswer::Update(text), None),
            Err(error) => {
                tracing::error!(error = %format!("{error:#}"), "a button decision failed");
                (
                    PressAnswer::Private(format!("The decision did not go through: {error:#}")),
                    None,
                )
            }
        }
    }

    /// Drives the run on after a press. The approver gets the answer of a
    /// CLI requester, because no `lg approve` waits for it.
    pub(super) async fn resume_after_press(
        &mut self,
        record: &ApprovalRecord,
        service: Service,
        channel: &str,
    ) {
        match self.resume(record).await {
            Ok(Response::Reply { text } | Response::NoAnswer { reason: text }) => {
                if let Err(error) = self.chats.send(service, channel, &text).await {
                    tracing::warn!(%error, "the answer of a request did not go out");
                }
            }
            Ok(_) => {}
            Err(error) => {
                tracing::error!(error = %format!("{error:#}"), "driving after a decision failed");
            }
        }
    }

    /// Runs the held chat messages and Cortex wakes, one at a time, until a
    /// request waits again.
    pub(super) async fn release_held(&mut self) {
        while self.waiting.is_none() {
            if let Some(message_id) = self.held_wakes.pop_front() {
                if let Err(error) = self.wake(&message_id).await {
                    tracing::error!(error = %format!("{error:#}"), %message_id, "a held cortex wake failed");
                }
                continue;
            }
            let Some((service, message)) = self.held.pop_front() else {
                return;
            };
            let outgoing = match self.chat(service, &message).await {
                Ok(Some(outgoing)) => outgoing,
                Ok(None) => continue,
                Err(error) => {
                    tracing::error!(error = %format!("{error:#}"), "a held message failed");
                    continue;
                }
            };
            let chats = Arc::clone(&self.chats);
            if let Err(error) = chats
                .send(service, &outgoing.channel_id, &outgoing.text)
                .await
            {
                // The cursor stays, so the next reconnect reads it again.
                tracing::warn!(%error, "a held reply did not go out");
                continue;
            }
            if let Some(sender) = outgoing.track
                && let Err(error) =
                    self.delivered(&sender, &outgoing.channel_id, &outgoing.message_id)
            {
                tracing::error!(error = %format!("{error:#}"), "the delivery cursor did not save");
            }
        }
    }
}
