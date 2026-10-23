//! The self-serve approval prompts of one agent turn. Every prompt has its own
//! id and an answer reaches only the call that raised it. A call that was
//! denied, left unanswered, or approved and then failed is refused for the
//! rest of the turn instead of being put to the person again.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use workforce_core::policy::PermissionChoice;

use crate::runner::InjectedMessage;

/// One tool call as the model emitted it, secret markers intact.
#[derive(Debug, Clone, PartialEq)]
pub struct GatedCall {
    pub tool: String,
    pub input: Value,
}

/// Why a gated call did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Denied,
    Unanswered,
    Failed,
    Unattended,
}

impl Refusal {
    /// What the model is told in place of the call's result.
    pub fn tool_result(self) -> &'static str {
        match self {
            Self::Denied => {
                "The user denied this call, so it did not run. Do not call it again in this \
                 turn; tell the user it was not done."
            }
            Self::Unanswered => {
                "Nobody answered the approval prompt for this call in time, so it did not run. \
                 Do not call it again in this turn; tell the user it was not done because \
                 nobody approved it in time."
            }
            Self::Failed => {
                "This exact call already failed after it was approved in this turn, so it was \
                 not run or put to the user again. Do not call it again; tell the user what \
                 failed."
            }
            Self::Unattended => {
                "This call needs a person's approval and nobody can answer an approval prompt \
                 here, so it did not run. Do not call it again; report that it needs approval."
            }
        }
    }
}

/// A prompt raised for one call, waiting for the person's answer.
pub struct OpenPrompt {
    pub request_id: String,
    call: GatedCall,
    reply: oneshot::Receiver<PermissionChoice>,
}

/// How a prompt settled: the person's choice, or `None` when nobody answered.
pub struct Decision {
    pub call: GatedCall,
    pub choice: Option<PermissionChoice>,
}

impl Decision {
    pub fn verdict(&self) -> Result<(), Refusal> {
        match self.choice {
            Some(choice) if choice.is_allow() => Ok(()),
            Some(_) => Err(Refusal::Denied),
            None => Err(Refusal::Unanswered),
        }
    }
}

/// Whether anyone on the turn's surface can answer a self-serve prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelfServe {
    #[default]
    Answerable,
    /// The surface renders no prompt, so a self-serve call is refused at once
    /// instead of waiting out the approval timeout.
    Unanswerable,
}

impl SelfServe {
    pub fn answerable_if(answers_approvals: bool) -> Self {
        if answers_approvals {
            Self::Answerable
        } else {
            Self::Unanswerable
        }
    }
}

/// The ledger of one turn's self-serve prompts.
#[derive(Default)]
pub struct TurnApprovals {
    ledger: Mutex<Ledger>,
}

#[derive(Default)]
struct Ledger {
    waiting: HashMap<String, oneshot::Sender<PermissionChoice>>,
    closed: Vec<(GatedCall, Refusal)>,
    ended: bool,
}

/// How many forwarded messages may wait for the turn to take them.
const FORWARD_BUFFER: usize = 32;

impl TurnApprovals {
    /// A ledger answered from `rx`: approval answers settle its prompts and
    /// every other message comes out of the returned receiver, in order.
    /// Messages already buffered are split before this returns, so a
    /// follow-up queued while the turn was setting up is there at once.
    pub fn listen(
        mut rx: mpsc::Receiver<InjectedMessage>,
    ) -> (Arc<Self>, mpsc::Receiver<InjectedMessage>) {
        let ledger = Arc::new(Self::default());
        let (forward, forwarded) = mpsc::channel(FORWARD_BUFFER);
        while let Ok(message) = rx.try_recv() {
            if let Some(other) = ledger.take_answer(message) {
                let _ = forward.try_send(other);
            }
        }
        let listening = Arc::clone(&ledger);
        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if let Some(other) = listening.take_answer(message) {
                    if forward.send(other).await.is_err() {
                        return;
                    }
                }
            }
        });
        (ledger, forwarded)
    }

    /// The turn's ledger and the rest of its inbox. An unanswerable surface
    /// gets no ledger, so its self-serve calls are refused, while its
    /// follow-ups still arrive.
    pub fn split(
        inbox: Option<mpsc::Receiver<InjectedMessage>>,
        self_serve: SelfServe,
    ) -> (Option<Arc<Self>>, Option<mpsc::Receiver<InjectedMessage>>) {
        match (inbox, self_serve) {
            (Some(rx), SelfServe::Answerable) => {
                let (ledger, others) = Self::listen(rx);
                (Some(ledger), Some(others))
            }
            (inbox, _) => (None, inbox),
        }
    }

    /// Settle the prompt an approval answer names; anything else is handed
    /// back.
    fn take_answer(&self, message: InjectedMessage) -> Option<InjectedMessage> {
        match message {
            InjectedMessage::ApprovalResponse {
                request_id,
                approved,
                choice,
                ..
            } => {
                let choice = choice.unwrap_or_else(|| PermissionChoice::from_legacy_bool(approved));
                if !self.answer(&request_id, choice) {
                    tracing::warn!(request = %request_id, "approval answer matched no waiting call");
                }
                None
            }
            other => Some(other),
        }
    }

    /// The turn is over: every waiting prompt settles unanswered now, and a
    /// prompt raised later is refused as unattended, so a call still held by
    /// a task that outlived its turn can never run on a late answer.
    pub fn end(&self) {
        let mut ledger = self.ledger.lock();
        ledger.ended = true;
        ledger.waiting.clear();
    }

    /// Raise a prompt for `call`, or refuse it at once when this turn has
    /// already closed the same call or has ended.
    pub fn open(&self, call: GatedCall) -> Result<OpenPrompt, Refusal> {
        let mut ledger = self.ledger.lock();
        if ledger.ended {
            return Err(Refusal::Unattended);
        }
        if let Some((_, refusal)) = ledger.closed.iter().find(|(closed, _)| *closed == call) {
            return Err(*refusal);
        }
        let (answer, reply) = oneshot::channel();
        let request_id = uuid::Uuid::new_v4().to_string();
        ledger.waiting.insert(request_id.clone(), answer);
        Ok(OpenPrompt {
            request_id,
            call,
            reply,
        })
    }

    /// Deliver a person's choice to the prompt `request_id` names. False when
    /// no prompt of this turn is waiting under that id.
    pub fn answer(&self, request_id: &str, choice: PermissionChoice) -> bool {
        self.ledger
            .lock()
            .waiting
            .remove(request_id)
            .is_some_and(|answer| answer.send(choice).is_ok())
    }

    /// Wait up to `patience` for the prompt's answer. A refused call is
    /// closed for the rest of the turn; a wait dropped before it settles
    /// takes its prompt with it, so a late answer finds nothing.
    pub async fn resolve(&self, prompt: OpenPrompt, patience: Duration) -> Decision {
        let OpenPrompt {
            request_id,
            call,
            mut reply,
        } = prompt;
        let _waiting = Waiting {
            ledger: self,
            request_id: &request_id,
        };
        let choice = match tokio::time::timeout(patience, &mut reply).await {
            Ok(answered) => answered.ok(),
            Err(_) => self.withdraw(&request_id, &mut reply),
        };
        let decision = Decision { call, choice };
        if let Err(refusal) = decision.verdict() {
            self.close(decision.call.clone(), refusal);
        }
        decision
    }

    /// Ends the ledger when dropped, however the turn that holds it returns.
    pub fn end_on_drop(self: Arc<Self>) -> EndsOnDrop {
        EndsOnDrop(self)
    }

    /// Record that `call` must not be put to the person again in this turn.
    pub fn close(&self, call: GatedCall, refusal: Refusal) {
        self.ledger.lock().closed.push((call, refusal));
    }

    /// Stop waiting on a prompt whose patience ran out. An answer that won
    /// the race was sent under the lock, so it is already in `reply`.
    fn withdraw(
        &self,
        request_id: &str,
        reply: &mut oneshot::Receiver<PermissionChoice>,
    ) -> Option<PermissionChoice> {
        match self.ledger.lock().waiting.remove(request_id) {
            Some(_) => None,
            None => reply.try_recv().ok(),
        }
    }
}

/// A prompt still waiting for its answer, withdrawn if the wait is dropped.
struct Waiting<'a> {
    ledger: &'a TurnApprovals,
    request_id: &'a str,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.ledger.ledger.lock().waiting.remove(self.request_id);
    }
}

/// Ends a turn's ledger when dropped: see [`TurnApprovals::end_on_drop`].
pub struct EndsOnDrop(Arc<TurnApprovals>);

impl Drop for EndsOnDrop {
    fn drop(&mut self) {
        self.0.end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATIENCE: Duration = Duration::from_secs(300);

    fn call(name: &str) -> GatedCall {
        GatedCall {
            tool: testing::TOOL.to_string(),
            input: serde_json::json!({ "name": name }),
        }
    }

    async fn decided(
        approvals: &TurnApprovals,
        call: GatedCall,
        choice: PermissionChoice,
    ) -> Decision {
        let prompt = approvals.open(call).expect("a prompt");
        assert!(approvals.answer(&prompt.request_id, choice));
        approvals.resolve(prompt, PATIENCE).await
    }

    #[tokio::test]
    async fn an_unanswerable_inbox_has_no_ledger_but_still_carries_follow_ups() {
        let (tx, rx) = mpsc::channel(4);
        tx.send(follow_up("also check main")).await.unwrap();
        let (ledger, others) = TurnApprovals::split(Some(rx), SelfServe::Unanswerable);
        assert!(ledger.is_none(), "nobody could answer its prompts");
        let message = others.expect("the inbox").recv().await;
        assert!(
            matches!(message, Some(InjectedMessage::UserMessage { text, .. }) if text == "also check main")
        );
    }

    #[tokio::test]
    async fn an_answerable_inbox_gets_a_ledger() {
        let (_tx, rx) = mpsc::channel(4);
        let (ledger, others) = TurnApprovals::split(Some(rx), SelfServe::Answerable);
        assert!(ledger.is_some() && others.is_some());
        assert!(matches!(
            TurnApprovals::split(None, SelfServe::Answerable),
            (None, None)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_reaches_only_the_prompt_it_names() {
        let approvals = TurnApprovals::default();
        let figma = approvals.open(call("figma")).expect("a prompt");
        let probe = approvals.open(call("probe")).expect("a prompt");
        assert_ne!(figma.request_id, probe.request_id);

        assert!(!approvals.answer("not-a-request", PermissionChoice::AllowOnce));
        assert!(approvals.answer(&probe.request_id, PermissionChoice::AllowOnce));

        let probe = approvals.resolve(probe, PATIENCE).await;
        assert_eq!(probe.choice, Some(PermissionChoice::AllowOnce));
        assert_eq!(probe.verdict(), Ok(()));
        let figma = approvals.resolve(figma, PATIENCE).await;
        assert_eq!(figma.choice, None, "the other prompt heard nothing");
        assert_eq!(figma.verdict(), Err(Refusal::Unanswered));
    }

    #[tokio::test]
    async fn a_denied_call_is_not_put_to_the_user_again_but_a_different_input_is() {
        let approvals = TurnApprovals::default();
        let denied = decided(&approvals, call("figma"), PermissionChoice::DenyOnce).await;
        assert_eq!(denied.verdict(), Err(Refusal::Denied));

        assert_eq!(
            approvals.open(call("figma")).err(),
            Some(Refusal::Denied),
            "the same call is refused without a prompt"
        );
        assert!(
            approvals.open(call("probe")).is_ok(),
            "a different input is its own decision"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_prompt_closes_and_a_late_answer_finds_nothing() {
        let approvals = TurnApprovals::default();
        let prompt = approvals.open(call("figma")).expect("a prompt");
        let request_id = prompt.request_id.clone();
        let started = tokio::time::Instant::now();

        let decision = approvals.resolve(prompt, PATIENCE).await;
        assert_eq!(decision.verdict(), Err(Refusal::Unanswered));
        assert_eq!(started.elapsed(), PATIENCE);
        assert!(!approvals.answer(&request_id, PermissionChoice::AllowOnce));
        assert_eq!(
            approvals.open(call("figma")).err(),
            Some(Refusal::Unanswered)
        );
    }

    #[tokio::test]
    async fn identical_calls_waiting_together_are_separate_decisions() {
        let approvals = TurnApprovals::default();
        let first = approvals.open(call("figma")).expect("a prompt");
        let second = approvals.open(call("figma")).expect("a prompt");
        assert_ne!(first.request_id, second.request_id);

        assert!(approvals.answer(&second.request_id, PermissionChoice::DenyOnce));
        assert!(approvals.answer(&first.request_id, PermissionChoice::AllowOnce));
        assert_eq!(approvals.resolve(first, PATIENCE).await.verdict(), Ok(()));
        assert_eq!(
            approvals.resolve(second, PATIENCE).await.verdict(),
            Err(Refusal::Denied)
        );
    }

    fn follow_up(text: &str) -> InjectedMessage {
        InjectedMessage::UserMessage {
            text: text.to_string(),
            user: "alice".to_string(),
            external_id: None,
        }
    }

    fn answer_to(request_id: &str) -> InjectedMessage {
        InjectedMessage::ApprovalResponse {
            request_id: request_id.to_string(),
            approved: false,
            user: "alice".to_string(),
            choice: Some(PermissionChoice::AllowOnce),
        }
    }

    fn text_of(message: InjectedMessage) -> String {
        match message {
            InjectedMessage::UserMessage { text, .. } => text,
            other => panic!("expected a follow-up, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn turn_approvals_listen_splits_answers_from_followups() {
        let (tx, rx) = mpsc::channel(8);
        tx.send(follow_up("queued during setup")).await.unwrap();
        let (approvals, mut others) = TurnApprovals::listen(rx);
        assert_eq!(
            text_of(others.try_recv().expect("buffered before listen returned")),
            "queued during setup"
        );

        let prompt = approvals.open(call("figma")).expect("a prompt");
        tx.send(answer_to(&prompt.request_id)).await.unwrap();
        tx.send(follow_up("and another thing")).await.unwrap();
        assert_eq!(
            approvals.resolve(prompt, PATIENCE).await.choice,
            Some(PermissionChoice::AllowOnce),
            "the answer settles the prompt, typed choice first"
        );
        assert_eq!(text_of(others.recv().await.unwrap()), "and another thing");
        drop(tx);
        assert!(
            others.recv().await.is_none(),
            "answers never reach the follow-up side"
        );
    }

    #[tokio::test]
    async fn ended_ledger_refuses_new_prompts() {
        let approvals = TurnApprovals::default();
        let held = approvals.open(call("figma")).expect("a prompt");
        let request_id = held.request_id.clone();
        approvals.end();

        let decision = approvals.resolve(held, PATIENCE).await;
        assert_eq!(
            decision.verdict(),
            Err(Refusal::Unanswered),
            "a prompt waiting when the turn ended settles at once, unanswered"
        );
        assert!(!approvals.answer(&request_id, PermissionChoice::AllowOnce));
        assert_eq!(
            approvals.open(call("probe")).err(),
            Some(Refusal::Unattended),
            "nobody is left to ask once the turn is over"
        );
    }

    #[tokio::test]
    async fn a_failed_approved_call_is_closed_and_a_successful_one_is_not() {
        let approvals = TurnApprovals::default();
        let failed = decided(&approvals, call("figma"), PermissionChoice::AllowOnce).await;
        assert_eq!(failed.verdict(), Ok(()));
        approvals.close(failed.call, Refusal::Failed);
        assert_eq!(approvals.open(call("figma")).err(), Some(Refusal::Failed));

        let succeeded = decided(&approvals, call("probe"), PermissionChoice::AllowOnce).await;
        assert_eq!(succeeded.verdict(), Ok(()));
        assert!(
            approvals.open(call("probe")).is_ok(),
            "allow_once covers one run, so a repeat is asked again"
        );
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    //! A gated turn in miniature: a tool whose schema requires a `name`, a
    //! model that retries refused calls the way a real one does, and a runner
    //! whose permission cascade asks before every call.

    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use async_trait::async_trait;
    use eyre::Result;
    use parking_lot::Mutex;
    use serde::Deserialize;
    use serde_json::{json, Value};
    use workforce_core::types::AgentId;
    use workforce_policy::engine::PolicyEngine;

    use crate::llm::provider::LlmProvider;
    use crate::llm::types::{
        ContentBlock, LlmMessage, LlmResponse, LlmToolDef, Role, StopReason, TokenUsage,
    };
    use crate::runner::{
        AgentRunner, CascadeState, PermissionGate, RunnerConfig, CURRENT_SESSION_KEY,
        CURRENT_USER_ID,
    };
    use crate::tools::registry::ToolRegistry;
    use crate::tools::{Tool, ToolResult};

    pub const TOOL: &str = "catalogue.remove_entry";
    pub const MEMBER: &str = "alice";

    /// Removes a catalogue entry by `name`, recording the input of every run.
    #[derive(Default)]
    pub struct RecordingTool {
        failing: bool,
        runs: Mutex<Vec<Value>>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Entry {
        name: String,
    }

    impl RecordingTool {
        pub fn failing() -> Self {
            Self {
                failing: true,
                ..Self::default()
            }
        }

        pub fn runs(&self) -> Vec<Value> {
            self.runs.lock().clone()
        }
    }

    #[async_trait]
    impl Tool for RecordingTool {
        fn name(&self) -> &str {
            TOOL
        }

        fn description(&self) -> &str {
            "Remove an entry from the catalogue"
        }

        fn input_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            })
        }

        async fn execute(&self, input: Value) -> Result<ToolResult> {
            self.runs.lock().push(input.clone());
            Ok(match serde_json::from_value::<Entry>(input) {
                Err(e) => ToolResult::failure(e.to_string(), 0),
                Ok(_) if self.failing => ToolResult::failure("the catalogue is unavailable", 0),
                Ok(entry) => ToolResult::success(json!({ "removed": entry.name }), 0),
            })
        }
    }

    /// Opens with parallel calls to [`TOOL`], then answers each failed call the
    /// way a model does: a result naming the missing `name` gets the corrected
    /// input, any other failure the same call again. It replies in text once
    /// every call succeeded or its budget of tool rounds is spent.
    pub struct ScriptedModel {
        opening: Vec<Value>,
        corrected: Value,
        budget: usize,
        rounds: AtomicUsize,
    }

    impl ScriptedModel {
        pub fn new(
            opening: impl IntoIterator<Item = Value>,
            corrected: Value,
            budget: usize,
        ) -> Self {
            Self {
                opening: opening.into_iter().collect(),
                corrected,
                budget,
                rounds: AtomicUsize::new(0),
            }
        }

        fn next_calls(&self, messages: &[LlmMessage]) -> Vec<Value> {
            let Some(last) = messages.iter().rposition(|m| m.role == Role::Assistant) else {
                return self.opening.clone();
            };
            let failures: HashMap<&str, &str> = messages[last + 1..]
                .iter()
                .flat_map(|m| &m.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error: Some(true),
                        ..
                    } => Some((tool_use_id.as_str(), content.as_str())),
                    _ => None,
                })
                .collect();
            messages[last]
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, input, .. } => {
                        failures.get(id.as_str()).map(|failure| {
                            if failure.contains("`name`") {
                                self.corrected.clone()
                            } else {
                                input.clone()
                            }
                        })
                    }
                    _ => None,
                })
                .collect()
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedModel {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            let round = self.rounds.fetch_add(1, Ordering::SeqCst);
            let calls = if round < self.budget {
                self.next_calls(messages)
            } else {
                Vec::new()
            };
            let (content, stop_reason) = if calls.is_empty() {
                (
                    vec![ContentBlock::Text {
                        text: "done".to_string(),
                    }],
                    StopReason::EndTurn,
                )
            } else {
                (
                    calls
                        .into_iter()
                        .enumerate()
                        .map(|(i, input)| {
                            ContentBlock::tool_use(format!("call-{round}-{i}"), TOOL.into(), input)
                        })
                        .collect(),
                    StopReason::ToolUse,
                )
            };
            Ok(LlmResponse {
                content,
                stop_reason,
                usage: TokenUsage::default(),
                model: "scripted".to_string(),
            })
        }
    }

    /// A permission cascade that asks before every call.
    pub struct AskEveryCall;

    impl PermissionGate for AskEveryCall {
        fn evaluate(&self, _user_id: &str, _agent_id: &str, _tool_name: &str) -> CascadeState {
            CascadeState::Ask
        }
    }

    /// A runner offering only `tool`, under a policy that allows everything
    /// and a cascade that asks before every call.
    pub fn gated_runner(model: ScriptedModel, tool: Arc<RecordingTool>) -> AgentRunner {
        let tools = ToolRegistry::new();
        tools.register(tool);
        let config = RunnerConfig {
            agent_id: AgentId::new("agent"),
            model: Some("scripted".to_string()),
            permission_gate: Some(Arc::new(AskEveryCall)),
            ..RunnerConfig::default()
        };
        let policy = PolicyEngine::from_yaml(
            "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
        )
        .expect("allow-all policy parses");
        AgentRunner::new(config, Arc::new(model), Arc::new(tools), Arc::new(policy))
    }

    /// Run `fut` as [`MEMBER`] in their session, as a surface scopes a turn.
    pub async fn as_member<F: Future>(fut: F) -> F::Output {
        CURRENT_USER_ID
            .scope(
                MEMBER.to_string(),
                CURRENT_SESSION_KEY.scope(format!("test:{MEMBER}"), fut),
            )
            .await
    }
}
