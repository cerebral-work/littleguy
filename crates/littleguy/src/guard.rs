//! The calls that wait for a person: `run_code` and each call that a flow
//! rule holds.
//!
//! Two things make a call wait. The `run_code` tool always asks. A flow
//! rule asks when a call would complete a dangerous label set, such as the
//! Rule of Two. The engine decides both, and names the wait. It does not
//! show the call. So [`ApprovalGate`] records each call that may wait,
//! with its arguments, in `memory.db`. The daemon claims the record when
//! the wait starts and shows it to the approver (`crate::approvals`).
//!
//! The gate runs before the flow check of the engine, and it cannot see the
//! labels that the run holds. So it records each call whose labels a flow
//! rule names. A record whose call never waits is never claimed, and the
//! store drops it later.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use autumn_harvest::sandbox::SandboxRuntime;
use autumn_harvest_agent::flow::{FlowRules, effect_label};
use autumn_harvest_agent::sandbox::SandboxTool;
use autumn_harvest_agent::{
    AgentHarness, AgentProfile, BoxFuture, Rule, RunInfo, Tool, ToolCall, ToolDecision, ToolPolicy,
};
use littleguy::reflex_gate::{ReflexClassifyClient, ReflexToolPolicy};
use littleguy_stores::SqliteStores;

use crate::sandbox::{ASK_REASON, TOOL};

/// The reason of a call that a flow rule may hold, for the approver.
pub const FLOW_REASON: &str = "a flow rule holds this call: the conversation would hold \
     untrusted input, private data and a write or an outside effect together";

/// The tools and rules that make a call wait, and the store that keeps
/// each call that waits.
#[derive(Debug)]
pub struct Guard {
    /// The store of the calls that wait.
    pub stores: Arc<SqliteStores>,
    /// How long a call waits for a decision.
    pub timeout: Duration,
    /// The flow rules.
    pub flow: FlowRules,
    /// The runtime of the `run_code` tool, when the sandbox is on.
    pub sandbox: Option<Arc<SandboxRuntime>>,
    /// Whether the Google tools are on, so each send and invite waits.
    pub google: bool,
    /// Optional `reflexd` `/v1/classify` client (`specs/0008`).
    pub reflex: Option<ReflexClassifyClient>,
    /// Self-declared agent identifier sent to `reflexd` (for example `assistant/alice`).
    pub agent_name: String,
}

impl Guard {
    /// Whether anything can make a call wait: a tool that always asks
    /// (`always`), or a flow rule that `tools` can complete.
    ///
    /// A rule can trip only when the tools carry each label of its set. The
    /// default tools carry no private data, so the Rule of Two alone needs
    /// no store until a tool such as an MCP server brings that label.
    #[must_use]
    pub fn asks(flow: &FlowRules, tools: &[Arc<dyn Tool>], always: bool) -> bool {
        let carried: BTreeSet<&str> = tools
            .iter()
            .flat_map(|tool| {
                tool.labels()
                    .iter()
                    .map(String::as_str)
                    .chain([effect_label(tool.effect())])
            })
            .collect();
        always
            || flow.rules().iter().any(|rule| {
                *rule.rule() == Rule::Ask
                    && rule.labels().iter().all(|l| carried.contains(l.as_str()))
            })
    }

    /// Gives the harness the `run_code` tool, the gate and the flow rules,
    /// and the profile the wait.
    #[must_use]
    pub fn apply(
        self,
        mut harness: AgentHarness,
        profile: AgentProfile,
    ) -> (AgentHarness, AgentProfile) {
        let run_code = self.sandbox.is_some();
        if let Some(runtime) = self.sandbox {
            harness = harness.tool(SandboxTool::new(runtime).shared());
        }
        let gate: Arc<dyn ToolPolicy> = Arc::new(ApprovalGate::new(
            Arc::clone(&self.stores),
            run_code,
            self.google,
            &self.flow,
        ));
        let policy: Arc<dyn ToolPolicy> = match self.reflex {
            Some(client) => Arc::new(
                ReflexToolPolicy::new(client, self.agent_name, true)
                    .with_inner(gate)
                    .with_approval_store(self.stores),
            ),
            None => gate,
        };
        if !self.flow.is_empty() {
            harness = harness.flow_rules(self.flow);
        }
        (
            harness.policy(policy),
            profile.approval_timeout(self.timeout),
        )
    }
}

/// Asks a person before each `run_code` call, each mail send and each
/// invite, and records each call that may wait.
///
/// Every other tool runs without a question from the gate. A flow rule may
/// still hold it. A call that cannot be recorded is denied, so no call
/// waits that the approver cannot read.
#[derive(Debug)]
pub struct ApprovalGate {
    stores: Arc<SqliteStores>,
    run_code: bool,
    google: bool,
    watched: BTreeSet<String>,
}

impl ApprovalGate {
    /// A gate that records in `stores`. With `run_code`, each `run_code`
    /// call waits. With `google`, each mail send and each invite waits.
    /// Each call with a label of a rule in `flow` is recorded.
    #[must_use]
    pub fn new(stores: Arc<SqliteStores>, run_code: bool, google: bool, flow: &FlowRules) -> Self {
        let watched = flow
            .rules()
            .iter()
            .filter(|rule| *rule.rule() == Rule::Ask)
            .flat_map(|rule| rule.labels().iter().cloned())
            .collect();
        Self {
            stores,
            run_code,
            google,
            watched,
        }
    }

    /// Why `call` always waits for a person, or `None`.
    fn ask_reason(&self, call: &ToolCall) -> Option<&'static str> {
        if self.run_code && call.name == TOOL {
            return Some(ASK_REASON);
        }
        if self.google {
            return littleguy_tools::google::approval_reason(&call.name, &call.arguments);
        }
        None
    }

    /// Whether a flow rule may hold a call to `tool`.
    fn watches(&self, tool: Option<&dyn Tool>) -> bool {
        tool.is_some_and(|tool| {
            self.watched.contains(effect_label(tool.effect()))
                || tool
                    .labels()
                    .iter()
                    .any(|label| self.watched.contains(label))
        })
    }

    /// Records `call` with `reason`. Returns `false` when the store fails.
    fn record(&self, call: &ToolCall, reason: &str) -> bool {
        match self
            .stores
            .record_approval(&call.id, &call.name, &call.arguments, reason)
        {
            Ok(id) => {
                tracing::debug!(approval = id, tool = %call.name, "a call that may wait is recorded");
                true
            }
            Err(error) => {
                tracing::error!(%error, "a call that may wait was not recorded");
                false
            }
        }
    }

    /// The decision on `call`. The policy runs inside the model-turn
    /// activity, and replay reads its answer. So a call is recorded once
    /// for each attempt of that activity.
    #[must_use]
    pub fn decide_call(&self, call: &ToolCall, tool: Option<&dyn Tool>) -> ToolDecision {
        let refused = || ToolDecision::Deny {
            reason: String::from("the call could not be recorded for approval, so it did not run"),
        };
        if let Some(reason) = self.ask_reason(call) {
            if !self.record(call, reason) {
                return refused();
            }
            return ToolDecision::RequireApproval {
                reason: String::from(reason),
            };
        }
        if self.watches(tool) && !self.record(call, FLOW_REASON) {
            return refused();
        }
        ToolDecision::Allow
    }
}

impl ToolPolicy for ApprovalGate {
    fn decide<'a>(
        &'a self,
        call: &'a ToolCall,
        tool: Option<&'a dyn Tool>,
        _info: &'a RunInfo,
    ) -> BoxFuture<'a, ToolDecision> {
        Box::pin(std::future::ready(self.decide_call(call, tool)))
    }
}

#[cfg(test)]
mod tests {
    use autumn_harvest_agent::flow::labels::{PRIVATE_DATA, UNTRUSTED_INPUT};
    use autumn_harvest_agent::{FnTool, ToolEffect};
    use serde_json::json;

    use super::*;

    fn tool(name: &str, effect: ToolEffect, labels: &[&str]) -> Arc<dyn Tool> {
        let mut tool = FnTool::new(name, "A test tool.", json!({}), |_| async {
            Ok(serde_json::Value::Null)
        })
        .effect(effect);
        for label in labels {
            tool = tool.label(*label);
        }
        Arc::new(tool)
    }

    #[test]
    fn a_guard_is_needed_only_when_a_rule_can_trip() {
        let two = FlowRules::rule_of_two();
        let web = tool("web", ToolEffect::ReadOnly, &[UNTRUSTED_INPUT]);
        let mail = tool("mail", ToolEffect::ReadOnly, &[PRIVATE_DATA]);
        let send = tool("send", ToolEffect::External, &[]);
        assert!(!Guard::asks(&two, &[], false));
        assert!(Guard::asks(&two, &[], true), "the sandbox always asks");
        assert!(!Guard::asks(
            &two,
            &[Arc::clone(&web), Arc::clone(&send)],
            false
        ));
        assert!(!Guard::asks(
            &two,
            &[Arc::clone(&web), Arc::clone(&mail)],
            false
        ));
        let all = [web, mail, send];
        assert!(Guard::asks(&two, &all, false));
        assert!(!Guard::asks(&FlowRules::new(), &all, false));
    }

    #[test]
    fn the_gate_watches_the_labels_of_ask_rules() {
        let dir = tempfile::tempdir().unwrap();
        let stores = crate::memory::open(&dir.path().join("lg.db")).unwrap();
        let gate = ApprovalGate::new(Arc::clone(&stores), false, false, &FlowRules::rule_of_two());
        let web = tool("web", ToolEffect::ReadOnly, &[UNTRUSTED_INPUT]);
        let note = tool("note", ToolEffect::Internal, &[]);
        assert!(gate.watches(Some(web.as_ref())));
        assert!(!gate.watches(Some(note.as_ref())));
        assert!(!gate.watches(None));
        let call = ToolCall {
            id: String::from("c-1"),
            name: String::from("web"),
            arguments: json!({"url": "https://example.com"}),
        };
        assert_eq!(
            gate.decide_call(&call, Some(web.as_ref())),
            ToolDecision::Allow
        );
        let call = ToolCall {
            name: String::from(TOOL),
            ..call
        };
        assert_eq!(
            gate.decide_call(&call, None),
            ToolDecision::Allow,
            "without the sandbox, run_code is not asked"
        );
        let send = ToolCall {
            id: String::from("c-2"),
            name: String::from("mail_send"),
            arguments: json!({"to": "a@x.org", "subject": "s", "body": "b"}),
        };
        assert_eq!(gate.decide_call(&send, None), ToolDecision::Allow);
        let mailing = ApprovalGate::new(stores, false, true, &FlowRules::new());
        assert_eq!(
            mailing.decide_call(&send, None),
            ToolDecision::RequireApproval {
                reason: String::from(littleguy_tools::google::SEND_REASON),
            }
        );
    }
}
