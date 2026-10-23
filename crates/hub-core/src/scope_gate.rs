//! Cross-surface agent-level scope filter.
//!
//! Hot-reloadable registry of `[agents.X.scope]` policies plus the
//! orchestrator that runs a classifier and reports an outcome. The LLM
//! classifier lives in `workforce_skills::adapters::slack::classifier` (kept
//! there for historical reasons — the logic is provider-agnostic and reused
//! by every surface that calls into this gate); the decision-model one is the
//! install's Kev/Jev, rolled out per `[decision.uses.scope_gate]`.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use tracing::{debug, info, warn};

use workforce_agent::llm::provider::LlmProvider;
use workforce_common::settings::{
    DecisionUse, ScopeClassifier, ScopeFilterConfig, ScopeFilterMode, ScopeGateUse,
};
use workforce_decide::scope::ScopeVerdict;
use workforce_skills::adapters::slack::classifier::{
    self as scope_classifier, ScopeClassification,
};

use crate::decision_records::{Pending, Recorder};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeOutcome {
    Allow,
    Reject { canned: String },
}

pub struct ScopeGate {
    policies: RwLock<Arc<HashMap<String, ScopeFilterConfig>>>,
}

impl ScopeGate {
    pub fn new(initial: HashMap<String, ScopeFilterConfig>) -> Arc<Self> {
        Arc::new(Self {
            policies: RwLock::new(Arc::new(initial)),
        })
    }

    pub fn upsert(&self, agent_id: String, policy: ScopeFilterConfig) {
        let mut guard = self.policies.write();
        let mut next: HashMap<String, ScopeFilterConfig> = (**guard).clone();
        next.insert(agent_id, policy);
        *guard = Arc::new(next);
    }

    pub fn remove(&self, agent_id: &str) {
        let mut guard = self.policies.write();
        if !guard.contains_key(agent_id) {
            return;
        }
        let mut next: HashMap<String, ScopeFilterConfig> = (**guard).clone();
        next.remove(agent_id);
        *guard = Arc::new(next);
    }

    pub fn lookup(&self, agent_id: &str) -> Option<ScopeFilterConfig> {
        self.policies.read().get(agent_id).cloned()
    }

    pub fn snapshot(&self) -> Arc<HashMap<String, ScopeFilterConfig>> {
        self.policies.read().clone()
    }

    pub fn is_empty(&self) -> bool {
        self.policies.read().is_empty()
    }
}

/// The decision model's side of the gate, behind a trait so the gate's
/// tests can stand in for it.
#[async_trait]
pub trait ScopeDecider: Send + Sync {
    /// How far the install has rolled the decision model out for this gate,
    /// and the floor a scope inherits when it sets none.
    fn rollout(&self) -> ScopeGateUse;
    /// The model's verdict, and the records of what it was asked for the
    /// gate to keep with what its own classifier said.
    async fn verdict(
        &self,
        text: &str,
        topic: &str,
        floor: f64,
    ) -> eyre::Result<(ScopeVerdict, Pending)>;
}

/// A sender with no key for any backend, or a backend too busy to answer
/// within the gate's budget, is an expected state, not a fault.
fn no_decision_for_sender(e: &eyre::Report) -> bool {
    e.downcast_ref::<workforce_decide::DecideError>()
        .is_some_and(|e| e.is_unavailable() || e.is_overloaded())
}

/// Who classifies a message, from the install's rollout and the scope's
/// own `classifier`: the scope's word is final, else `shadow` runs both.
/// Without a decision backend everything is the LLM.
enum Plan<'a> {
    Llm,
    Decision(&'a dyn ScopeDecider),
    Shadow(&'a dyn ScopeDecider),
}

fn plan<'a>(scope: &ScopeFilterConfig, decider: Option<&'a dyn ScopeDecider>) -> Plan<'a> {
    let Some(decider) = decider else {
        if scope.classifier == Some(ScopeClassifier::Decision) {
            warn!("scope pinned to the decision model but [decision] is off; the LLM classifies");
        }
        return Plan::Llm;
    };
    match (scope.classifier, decider.rollout().mode) {
        (Some(ScopeClassifier::Llm), _) | (None, DecisionUse::Off) => Plan::Llm,
        (Some(ScopeClassifier::Decision), _) | (None, DecisionUse::On) => Plan::Decision(decider),
        (None, DecisionUse::Shadow) => Plan::Shadow(decider),
    }
}

impl Plan<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Plan::Llm => "llm",
            Plan::Decision(_) => "decision",
            Plan::Shadow(_) => "shadow",
        }
    }
}

/// What the LLM classifier said, as a decision record keeps it: nothing
/// when it gave no verdict and the message was only let through, which is
/// no label for anyone to learn from.
fn classified(verdict: &ScopeClassification) -> Option<&'static str> {
    match (verdict.failed_open, verdict.on_topic) {
        (true, _) => None,
        (false, true) => Some("on_topic"),
        (false, false) => Some("off_topic"),
    }
}

pub async fn evaluate_with(
    scope: &ScopeFilterConfig,
    llm: &dyn LlmProvider,
    decider: Option<&dyn ScopeDecider>,
    records: Option<Recorder>,
    text: &str,
    surface: &str,
    agent_id: &str,
) -> ScopeOutcome {
    let Some(topic) = scope_classifier::resolve_topic_definition(
        scope.custom_prompt.as_deref(),
        scope.persona.as_deref(),
    ) else {
        return ScopeOutcome::Allow;
    };
    let by_llm = || scope_classifier::classify_scope(llm, text, &topic, scope.model.as_deref());
    let plan = plan(scope, decider);
    let (verdict, classifier) = match &plan {
        Plan::Llm => (by_llm().await, "llm"),
        Plan::Decision(decider) => match decide(*decider, scope, text, &topic).await {
            Ok((decided, asked)) => {
                asked.keep_in(records.as_ref(), None);
                info!(
                    target: "scope_filter",
                    surface = %surface,
                    agent = %agent_id,
                    on_topic = decided.on_topic,
                    p_off_topic = decided.p_off_topic,
                    "decision model's scope verdict"
                );
                (decided.into(), "decision")
            }
            Err(e) => {
                if no_decision_for_sender(&e) {
                    debug!(error = %e, "No decision for this sender; classifying with the LLM");
                } else {
                    warn!(error = %e, "Scope decision failed; classifying with the LLM");
                }
                (by_llm().await, "llm")
            }
        },
        Plan::Shadow(decider) => {
            let (llm_verdict, decision) =
                tokio::join!(by_llm(), decide(*decider, scope, text, &topic));
            match decision {
                Ok((shadow, asked)) => {
                    asked.keep_in(records.as_ref(), classified(&llm_verdict));
                    info!(
                        target: "scope_filter",
                        surface = %surface,
                        agent = %agent_id,
                        llm_on_topic = llm_verdict.on_topic,
                        decision_on_topic = shadow.on_topic,
                        agrees = llm_verdict.on_topic == shadow.on_topic,
                        p_off_topic = shadow.p_off_topic,
                        "shadow: decision model's scope verdict"
                    )
                }
                Err(e) if no_decision_for_sender(&e) => {
                    debug!(error = %e, "No decision for this sender in shadow")
                }
                Err(e) => warn!(error = %e, "Scope decision failed in shadow"),
            }
            (llm_verdict, "llm")
        }
    };

    if verdict.on_topic {
        return ScopeOutcome::Allow;
    }

    let preview: String = text.chars().take(100).collect();
    info!(
        target: "scope_filter",
        surface = %surface,
        agent = %agent_id,
        mode = ?scope.mode,
        plan = plan.kind(),
        classifier,
        reason = %verdict.reason,
        preview = %preview,
        "Off-topic message"
    );

    match scope.mode {
        ScopeFilterMode::Reject => {
            let canned = scope_classifier::resolve_canned_response(
                scope.canned_response.as_deref(),
                scope.persona.as_deref(),
            );
            ScopeOutcome::Reject { canned }
        }
        ScopeFilterMode::Warn => ScopeOutcome::Allow,
    }
}

/// The decision model's answer as the classifier reports it, plus the
/// probability behind it.
struct Decided {
    on_topic: bool,
    reason: String,
    p_off_topic: f64,
}

impl From<Decided> for ScopeClassification {
    fn from(d: Decided) -> Self {
        Self {
            on_topic: d.on_topic,
            reason: d.reason,
            failed_open: false,
        }
    }
}

async fn decide(
    decider: &dyn ScopeDecider,
    scope: &ScopeFilterConfig,
    text: &str,
    topic: &str,
) -> eyre::Result<(Decided, Pending)> {
    let floor = scope
        .min_probability
        .unwrap_or(decider.rollout().min_probability)
        .get();
    let (verdict, asked) = decider.verdict(text, topic, floor).await?;
    let decided = Decided {
        on_topic: !verdict.off_topic,
        reason: format!("decision model p(off-topic)={:.2}", verdict.p_off_topic),
        p_off_topic: verdict.p_off_topic,
    };
    Ok((decided, asked))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::decision_records::testing::{decision, recorded};
    use crate::decision_records::InMemoryDecisionRecords;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use workforce_agent::llm::streaming::StreamEvent;
    use workforce_agent::llm::types::{
        ContentBlock, LlmMessage, LlmResponse, LlmToolDef, StopReason, TokenUsage,
    };
    use workforce_common::settings::Probability;

    struct FixedLlm {
        response: String,
        calls: AtomicUsize,
    }

    impl FixedLlm {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmProvider for FixedLlm {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> eyre::Result<LlmResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: self.response.clone(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
                model: "mock".to_string(),
            })
        }

        async fn create_message_stream(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
            _tx: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> eyre::Result<()> {
            unreachable!("scope classifier never streams")
        }
    }

    fn llm_saying(on_topic: bool) -> FixedLlm {
        FixedLlm {
            response: format!(r#"{{"on_topic": {on_topic}, "reason": "llm"}}"#),
            calls: AtomicUsize::new(0),
        }
    }

    fn persona_policy(name: &str, mode: ScopeFilterMode) -> ScopeFilterConfig {
        ScopeFilterConfig {
            persona: Some(name.to_string()),
            mode,
            ..Default::default()
        }
    }

    fn floor(p: f64) -> Probability {
        Probability::try_from(p).unwrap()
    }

    /// A decision model with a fixed opinion, counting how often it is asked.
    struct FixedDecider {
        rollout: ScopeGateUse,
        p_off_topic: f64,
        fails: bool,
        asked: AtomicUsize,
        floors: std::sync::Mutex<Vec<f64>>,
    }

    impl FixedDecider {
        fn new(mode: DecisionUse, p_off_topic: f64) -> Self {
            Self {
                rollout: ScopeGateUse {
                    mode,
                    min_probability: floor(0.7),
                    limits: Default::default(),
                },
                p_off_topic,
                fails: false,
                asked: AtomicUsize::new(0),
                floors: Default::default(),
            }
        }

        fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ScopeDecider for FixedDecider {
        fn rollout(&self) -> ScopeGateUse {
            self.rollout
        }

        async fn verdict(
            &self,
            text: &str,
            topic: &str,
            floor: f64,
        ) -> eyre::Result<(ScopeVerdict, Pending)> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.floors.lock().unwrap().push(floor);
            if self.fails {
                return Err(eyre::eyre!("kev down"));
            }
            let verdict = ScopeVerdict {
                off_topic: self.p_off_topic >= floor,
                p_off_topic: self.p_off_topic,
            };
            let asked = Pending::of(
                &recorded(),
                &workforce_decide::Input::ScopeGate {
                    message: text.into(),
                    topic: topic.into(),
                },
                workforce_decide::RequestBudget::DEFAULT,
                &decision(serde_json::json!({
                    "on_topic": {"type": "noul", "noul": 1.0 - self.p_off_topic}
                })),
                self.rollout.mode == DecisionUse::On,
            );
            Ok((verdict, asked))
        }
    }

    async fn gate(
        scope: &ScopeFilterConfig,
        llm: &FixedLlm,
        decider: Option<&FixedDecider>,
    ) -> ScopeOutcome {
        gate_recording(scope, llm, decider, None).await
    }

    async fn gate_recording(
        scope: &ScopeFilterConfig,
        llm: &FixedLlm,
        decider: Option<&FixedDecider>,
        records: Option<Recorder>,
    ) -> ScopeOutcome {
        evaluate_with(
            scope,
            llm,
            decider.map(|d| d as &dyn ScopeDecider),
            records,
            "what's the weather",
            "test",
            "allan",
        )
        .await
    }

    #[test]
    fn lookup_returns_none_for_unknown() {
        let gate = ScopeGate::new(HashMap::new());
        assert!(gate.lookup("nope").is_none());
    }

    #[test]
    fn upsert_then_lookup_visible() {
        let gate = ScopeGate::new(HashMap::new());
        gate.upsert(
            "allan".into(),
            persona_policy("triage", ScopeFilterMode::Reject),
        );
        let p = gate.lookup("allan").unwrap();
        assert_eq!(p.persona.as_deref(), Some("triage"));
        assert_eq!(p.mode, ScopeFilterMode::Reject);
    }

    #[test]
    fn upsert_overwrites() {
        let gate = ScopeGate::new(HashMap::new());
        gate.upsert(
            "allan".into(),
            persona_policy("triage", ScopeFilterMode::Reject),
        );
        gate.upsert("allan".into(), persona_policy("faq", ScopeFilterMode::Warn));
        let p = gate.lookup("allan").unwrap();
        assert_eq!(p.persona.as_deref(), Some("faq"));
        assert_eq!(p.mode, ScopeFilterMode::Warn);
    }

    #[test]
    fn remove_drops_policy() {
        let gate = ScopeGate::new(HashMap::new());
        gate.upsert(
            "allan".into(),
            persona_policy("triage", ScopeFilterMode::Reject),
        );
        gate.remove("allan");
        assert!(gate.lookup("allan").is_none());
    }

    #[test]
    fn remove_unknown_is_noop() {
        let gate = ScopeGate::new(HashMap::new());
        gate.remove("nope");
    }

    #[test]
    fn the_scope_word_is_final_else_the_rollout_decides() {
        let unset = persona_policy("triage", ScopeFilterMode::Reject);
        let kind = |scope: &ScopeFilterConfig, mode: Option<DecisionUse>| {
            let decider = mode.map(|m| FixedDecider::new(m, 0.0));
            plan(scope, decider.as_ref().map(|d| d as &dyn ScopeDecider)).kind()
        };
        assert_eq!(kind(&unset, None), "llm");
        assert_eq!(kind(&unset, Some(DecisionUse::Off)), "llm");
        assert_eq!(kind(&unset, Some(DecisionUse::Shadow)), "shadow");
        assert_eq!(kind(&unset, Some(DecisionUse::On)), "decision");
        let llm = ScopeFilterConfig {
            classifier: Some(ScopeClassifier::Llm),
            ..unset.clone()
        };
        assert_eq!(kind(&llm, Some(DecisionUse::On)), "llm");
        let decision = ScopeFilterConfig {
            classifier: Some(ScopeClassifier::Decision),
            ..unset
        };
        assert_eq!(kind(&decision, Some(DecisionUse::Off)), "decision");
        assert_eq!(kind(&decision, None), "llm", "no decision backend at all");
    }

    #[tokio::test]
    async fn rolled_out_the_decision_model_rejects_and_the_llm_is_not_asked() {
        let scope = persona_policy("triage", ScopeFilterMode::Reject);
        let llm = llm_saying(true);
        let decider = FixedDecider::new(DecisionUse::On, 0.9);
        let outcome = gate(&scope, &llm, Some(&decider)).await;
        assert!(matches!(outcome, ScopeOutcome::Reject { .. }));
        assert_eq!((decider.asked(), llm.calls()), (1, 0));

        let unsure = FixedDecider::new(DecisionUse::On, 0.6);
        assert_eq!(gate(&scope, &llm, Some(&unsure)).await, ScopeOutcome::Allow);
        assert_eq!(llm.calls(), 0);
    }

    #[tokio::test]
    async fn a_scope_floor_replaces_the_installs() {
        let scope = ScopeFilterConfig {
            min_probability: Some(floor(0.55)),
            ..persona_policy("triage", ScopeFilterMode::Reject)
        };
        let decider = FixedDecider::new(DecisionUse::On, 0.6);
        assert!(matches!(
            gate(&scope, &llm_saying(true), Some(&decider)).await,
            ScopeOutcome::Reject { .. }
        ));
        assert_eq!(decider.floors.lock().unwrap().as_slice(), &[0.55]);

        let inherited = persona_policy("triage", ScopeFilterMode::Reject);
        let decider = FixedDecider::new(DecisionUse::On, 0.6);
        assert_eq!(
            gate(&inherited, &llm_saying(true), Some(&decider)).await,
            ScopeOutcome::Allow
        );
        assert_eq!(decider.floors.lock().unwrap().as_slice(), &[0.7]);
    }

    #[tokio::test]
    async fn a_failed_decision_falls_back_to_the_llm() {
        let scope = persona_policy("triage", ScopeFilterMode::Reject);
        let mut decider = FixedDecider::new(DecisionUse::On, 0.9);
        decider.fails = true;
        let llm = llm_saying(false);
        assert!(matches!(
            gate(&scope, &llm, Some(&decider)).await,
            ScopeOutcome::Reject { .. }
        ));
        assert_eq!(llm.calls(), 1);
        assert_eq!(
            gate(&scope, &llm_saying(true), Some(&decider)).await,
            ScopeOutcome::Allow
        );
    }

    #[tokio::test]
    async fn a_shadowed_message_is_kept_with_the_classifiers_verdict_and_a_decided_one_without() {
        let scope = persona_policy("triage", ScopeFilterMode::Reject);
        let store = Arc::new(InMemoryDecisionRecords::default());
        let recorder = Recorder::start(store.clone());
        let records = || Some(recorder.clone());

        let shadow = FixedDecider::new(DecisionUse::Shadow, 0.9);
        gate_recording(&scope, &llm_saying(true), Some(&shadow), records()).await;
        let kept = store.settled(1).await;
        assert_eq!(kept.len(), 1);
        let record = &kept[0].record;
        assert_eq!(record.verdict.as_deref(), Some("on_topic"));
        assert!(!record.acted);
        assert_eq!(
            record.input,
            workforce_decide::Input::ScopeGate {
                message: "what's the weather".into(),
                topic: scope_classifier::resolve_topic_definition(None, Some("triage")).unwrap(),
            },
            "the request as the model was asked it, not a preview"
        );
        assert_eq!(
            record.answers["on_topic"],
            workforce_decide::Answer::Noul { noul: 1.0 - 0.9 }
        );

        gate_recording(&scope, &llm_saying(false), Some(&shadow), records()).await;
        assert_eq!(
            store.settled(2).await[1].record.verdict.as_deref(),
            Some("off_topic")
        );

        let on = FixedDecider::new(DecisionUse::On, 0.9);
        gate_recording(&scope, &llm_saying(true), Some(&on), records()).await;
        let decided = &store.settled(3).await[2].record;
        assert_eq!((decided.verdict.as_deref(), decided.acted), (None, true));

        let garbled = FixedLlm {
            response: "I cannot say.".into(),
            calls: AtomicUsize::new(0),
        };
        gate_recording(&scope, &garbled, Some(&shadow), records()).await;
        let unjudged = &store.settled(4).await[3].record;
        assert_eq!(
            unjudged.verdict, None,
            "a classifier that failed open judged nothing: no label"
        );

        let off = FixedDecider::new(DecisionUse::Off, 0.9);
        gate_recording(&scope, &llm_saying(true), Some(&off), records()).await;
        let failing = FixedDecider {
            fails: true,
            ..FixedDecider::new(DecisionUse::Shadow, 0.9)
        };
        gate_recording(&scope, &llm_saying(true), Some(&failing), records()).await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(store.all().len(), 4, "nothing asked, nothing kept");
    }

    #[tokio::test]
    async fn in_shadow_the_llm_decides_and_the_decision_model_is_still_asked() {
        let scope = persona_policy("triage", ScopeFilterMode::Reject);
        let llm = llm_saying(true);
        let decider = FixedDecider::new(DecisionUse::Shadow, 0.95);
        assert_eq!(
            gate(&scope, &llm, Some(&decider)).await,
            ScopeOutcome::Allow
        );
        assert_eq!((decider.asked(), llm.calls()), (1, 1));

        let decider = FixedDecider::new(DecisionUse::Shadow, 0.0);
        assert!(matches!(
            gate(&scope, &llm_saying(false), Some(&decider)).await,
            ScopeOutcome::Reject { .. }
        ));

        let mut failing = FixedDecider::new(DecisionUse::Shadow, 0.95);
        failing.fails = true;
        let llm = llm_saying(true);
        assert_eq!(
            gate(&scope, &llm, Some(&failing)).await,
            ScopeOutcome::Allow
        );
        assert_eq!(llm.calls(), 1, "a failed shadow never asks the LLM twice");
    }

    #[tokio::test]
    async fn off_or_pinned_to_the_llm_the_decision_model_is_never_asked() {
        let scope = persona_policy("triage", ScopeFilterMode::Reject);
        let decider = FixedDecider::new(DecisionUse::Off, 0.95);
        assert_eq!(
            gate(&scope, &llm_saying(true), Some(&decider)).await,
            ScopeOutcome::Allow
        );
        assert_eq!(decider.asked(), 0);

        let pinned = ScopeFilterConfig {
            classifier: Some(ScopeClassifier::Llm),
            ..scope
        };
        let decider = FixedDecider::new(DecisionUse::On, 0.95);
        assert_eq!(
            gate(&pinned, &llm_saying(true), Some(&decider)).await,
            ScopeOutcome::Allow
        );
        assert_eq!(decider.asked(), 0);
    }

    #[tokio::test]
    async fn evaluate_on_topic_allows() {
        let scope = persona_policy("triage", ScopeFilterMode::Reject);
        let outcome = evaluate_with(
            &scope,
            &llm_saying(true),
            None,
            None,
            "bug in login",
            "test",
            "allan",
        )
        .await;
        assert_eq!(outcome, ScopeOutcome::Allow);
    }

    #[tokio::test]
    async fn evaluate_off_topic_reject_returns_canned() {
        let scope = persona_policy("triage", ScopeFilterMode::Reject);
        match gate(&scope, &llm_saying(false), None).await {
            ScopeOutcome::Reject { canned } => assert!(canned.contains("triage")),
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn evaluate_off_topic_warn_still_allows() {
        let scope = persona_policy("triage", ScopeFilterMode::Warn);
        assert_eq!(
            gate(&scope, &llm_saying(false), None).await,
            ScopeOutcome::Allow
        );
    }

    #[tokio::test]
    async fn evaluate_misconfigured_empty_topic_allows() {
        let scope = ScopeFilterConfig {
            mode: ScopeFilterMode::Reject,
            ..Default::default()
        };
        let llm = llm_saying(false);
        assert_eq!(gate(&scope, &llm, None).await, ScopeOutcome::Allow);
        assert_eq!(llm.calls(), 0);
    }

    #[tokio::test]
    async fn evaluate_uses_explicit_canned_response() {
        let scope = ScopeFilterConfig {
            persona: Some("triage".into()),
            mode: ScopeFilterMode::Reject,
            canned_response: Some("explicit reply".into()),
            ..Default::default()
        };
        assert_eq!(
            gate(&scope, &llm_saying(false), None).await,
            ScopeOutcome::Reject {
                canned: "explicit reply".into()
            }
        );
    }
}
