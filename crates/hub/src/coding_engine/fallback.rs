//! A chain of engines tried in order: the primary, then each fallback entry
//! when the round cannot run or fails for a reason the install lets it move
//! on from.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use workforce_agent::runner::ProgressEvent;
use workforce_common::settings::FallbackTrigger;
use workforce_orchestrator::workflow::instance::EngineAttempt;

use super::{CliRoundResult, CodingEngine, CodingRound, ModelChoices, Refusal, RoundContext};
use crate::claude_code::{headline, is_cancelled, CliHealth, Completion};
use crate::coding_sandbox::CodingSandbox;
use crate::subscription::limits::is_rate_limited;

/// A chain's final error, carrying the attempts it handed over from before
/// it. It stands in for the error it wraps: the same message in every
/// format and the same causes, so a caller reading the text (cancellation,
/// rate limits, the message a user sees) cannot tell it apart.
#[derive(Debug)]
struct HandedOver {
    attempts: Vec<EngineAttempt>,
    source: eyre::Report,
}

impl std::fmt::Display for HandedOver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&*self.source, f)
    }
}

impl std::error::Error for HandedOver {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        std::error::Error::source(&*self.source)
    }
}

/// The attempts a chain handed over from before it failed with `error`;
/// empty for an error that came from no chain.
pub fn handed_over(error: &eyre::Report) -> Vec<EngineAttempt> {
    error
        .downcast_ref::<HandedOver>()
        .map(|h| h.attempts.clone())
        .unwrap_or_default()
}

/// What a round that ran spent, under the engine it names or, when it names
/// none, `label`.
pub fn attempt_of(label: &str, r: &CliRoundResult) -> EngineAttempt {
    EngineAttempt {
        engine: if r.engine.is_empty() {
            label.to_string()
        } else {
            r.engine.clone()
        },
        model: r.model.clone(),
        input_tokens: r.input_tokens,
        output_tokens: r.output_tokens,
        cost_usd: r.cost_usd,
        error: None,
    }
}

/// `result` with the attempts handed over before it attached.
pub fn with_handed_over(
    result: Result<CliRoundResult>,
    attempts: Vec<EngineAttempt>,
) -> Result<CliRoundResult> {
    if attempts.is_empty() {
        return result;
    }
    match result {
        Ok(r) => Ok(CliRoundResult {
            handed_over: attempts,
            ..r
        }),
        Err(source) => Err(eyre::Report::new(HandedOver { attempts, source })),
    }
}

pub struct FallbackEngine {
    chain: Vec<(Arc<dyn CodingEngine>, Option<String>)>,
    allow_metered: bool,
    fallback_on: Vec<FallbackTrigger>,
}

/// A chain entry as this round runs it: the engine, the round placed on it,
/// and the model that round resolves to.
struct Entry<'a> {
    engine: &'a dyn CodingEngine,
    round: CodingRound,
    model: Option<String>,
}

/// What a chain knows about an entry before running it.
struct Candidate {
    metered: bool,
    preflight: Result<(), Refusal>,
}

/// The entries a round may try, in order, or why none can run. A primary the
/// caller cannot use now is final unless the chain `advances` on why: no
/// login of theirs at all, or one at its limit. When none can run, the
/// refusal carries the first entry's trigger.
fn plan(
    candidates: &[Candidate],
    allow_metered: bool,
    advances: impl Fn(FallbackTrigger) -> bool,
) -> Result<Vec<usize>, Refusal> {
    let mut eligible = Vec::new();
    let mut refusals = Vec::new();
    let mut metered_skipped = false;
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.metered && !allow_metered {
            metered_skipped = true;
            continue;
        }
        match &candidate.preflight {
            Ok(()) => eligible.push(index),
            Err(refusal) if index == 0 && !advances(refusal.trigger) => return Err(refusal.clone()),
            Err(refusal) => refusals.push(refusal.clone()),
        }
    }
    if !eligible.is_empty() {
        return Ok(eligible);
    }
    let mut message = match refusals.as_slice() {
        [] => "No coding engine is available.".to_string(),
        [one] => one.reason.clone(),
        many => format!(
            "No coding engine can run this round: {}",
            many.iter()
                .map(|refusal| refusal.reason.as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        ),
    };
    if metered_skipped {
        message.push_str(" Metered fallbacks are off ([coding_session.engine] allow_metered).");
    }
    Err(Refusal {
        reason: message,
        trigger: refusals
            .first()
            .map_or(FallbackTrigger::NoToken, |refusal| refusal.trigger),
    })
}

/// Why a finished attempt should hand over to the next entry, if it should.
/// The engine that ran it says whether its failure text is a spent limit; a
/// cancelled round hands over to nothing, since the caller asked it to stop.
fn trigger_of(
    engine: &dyn CodingEngine,
    result: &Result<CliRoundResult>,
) -> Option<FallbackTrigger> {
    match result {
        Ok(r) if r.completion == Completion::Failed => {
            if engine.limit_reached(&r.text) {
                Some(FallbackTrigger::RateLimited)
            } else {
                Some(FallbackTrigger::Failure)
            }
        }
        Ok(_) => None,
        Err(e) if is_cancelled(e) => None,
        Err(e) if is_rate_limited(&e.to_string()) => Some(FallbackTrigger::RateLimited),
        Err(_) => Some(FallbackTrigger::Failure),
    }
}

/// Try `attempts` in order, moving on only when `advances` allows the trigger
/// the last attempt raised; the final attempt's result stands whatever it is,
/// carrying every attempt it handed over from, what each spent and why it was
/// left.
async fn run_chain<F, Fut>(
    entries: &[Entry<'_>],
    attempts: &[usize],
    advances: impl Fn(FallbackTrigger) -> bool,
    mut attempt: F,
) -> Result<CliRoundResult>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<CliRoundResult>>,
{
    let (last, earlier) = attempts
        .split_last()
        .expect("a chain has at least one entry");
    let mut handed_over = Vec::new();
    for &index in earlier {
        let entry = &entries[index];
        let result = attempt(index).await;
        match trigger_of(entry.engine, &result) {
            Some(trigger) if advances(trigger) => {
                let handed = handed_over_from(entry, &result);
                warn!(
                    entry = index,
                    engine = %handed.engine,
                    model = handed.model.as_deref().unwrap_or(""),
                    ?trigger,
                    error = handed.error.as_deref().unwrap_or(""),
                    "coding engine handing over to the next in the chain"
                );
                handed_over.push(handed);
            }
            _ => return with_handed_over(result, handed_over),
        }
    }
    with_handed_over(attempt(*last).await, handed_over)
}

/// The one entry of a chain of `entries` a round pinned to a recorded job
/// runs on, whatever the chain would choose now; `None` for a round the chain
/// chooses for.
fn pinned_entry(round: &CodingRound, entries: usize) -> Option<Result<usize>> {
    let jobs = round.jobs.as_ref().filter(|jobs| jobs.pinned())?;
    Some(match jobs.engine() < entries {
        true => Ok(jobs.engine()),
        false => Err(eyre::eyre!(
            "the engine this round ran on is no longer configured"
        )),
    })
}

/// The round as the chain's entry at `index` runs it: on that entry's model
/// when it names one, and under job ids of its own, so one entry never
/// attaches to another's job.
fn placed(round: &CodingRound, index: usize, model: Option<&String>) -> CodingRound {
    CodingRound {
        model: model.cloned().or_else(|| round.model.clone()),
        jobs: round.jobs.as_ref().map(|jobs| jobs.for_engine(index)),
        ..round.clone()
    }
}

/// What an attempt the chain moved on from spent, and why it was left. An
/// attempt that errored spent what nobody reported, so it carries no usage.
fn handed_over_from(entry: &Entry<'_>, result: &Result<CliRoundResult>) -> EngineAttempt {
    match result {
        Ok(r) => EngineAttempt {
            error: Some(r.failure_line()),
            ..attempt_of(entry.engine.label(), r)
        },
        Err(e) => EngineAttempt {
            engine: entry.engine.label().to_string(),
            model: entry.model.clone(),
            error: headline(&format!("{e:#}")),
            ..Default::default()
        },
    }
}

impl FallbackEngine {
    pub fn new(
        chain: Vec<(Arc<dyn CodingEngine>, Option<String>)>,
        allow_metered: bool,
        fallback_on: Vec<FallbackTrigger>,
    ) -> Self {
        Self {
            chain,
            allow_metered,
            fallback_on,
        }
    }

    fn advances(&self, trigger: FallbackTrigger) -> bool {
        self.fallback_on.contains(&trigger)
    }

    fn eligible(&self, ctx: &RoundContext<'_>) -> Result<Vec<usize>, Refusal> {
        let candidates: Vec<Candidate> = self
            .chain
            .iter()
            .map(|(engine, _)| Candidate {
                metered: engine.metered(),
                preflight: engine.preflight(ctx),
            })
            .collect();
        plan(&candidates, self.allow_metered, |trigger| {
            self.advances(trigger)
        })
    }
}

#[async_trait]
impl CodingEngine for FallbackEngine {
    fn label(&self) -> &'static str {
        self.chain[0].0.label()
    }

    fn metered(&self) -> bool {
        self.chain[0].0.metered()
    }

    fn preflight(&self, ctx: &RoundContext<'_>) -> Result<(), Refusal> {
        self.eligible(ctx).map(drop)
    }

    fn limit_reached(&self, text: &str) -> bool {
        self.chain
            .iter()
            .any(|(engine, _)| engine.limit_reached(text))
    }

    fn model_choices(&self) -> ModelChoices {
        self.chain[0].0.model_choices()
    }

    async fn cli_health(&self) -> Option<CliHealth> {
        self.chain[0].0.cli_health().await
    }

    async fn run(
        &self,
        sandbox: Arc<dyn CodingSandbox>,
        round: &CodingRound,
        ctx: &RoundContext<'_>,
        cancel: Option<CancellationToken>,
        progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<CliRoundResult> {
        let attempts = match pinned_entry(round, self.chain.len()) {
            Some(entry) => vec![entry?],
            None => self
                .eligible(ctx)
                .map_err(|refusal| eyre::eyre!(refusal.reason))?,
        };
        let entries: Vec<Entry<'_>> = self
            .chain
            .iter()
            .enumerate()
            .map(|(index, (engine, model))| {
                let placed = placed(round, index, model.as_ref());
                Entry {
                    engine: engine.as_ref(),
                    model: engine.model_for(placed.model.as_deref(), ctx),
                    round: placed,
                }
            })
            .collect();
        let entries = &entries;
        run_chain(
            entries,
            &attempts,
            |t| self.advances(t),
            |index| {
                let sandbox = Arc::clone(&sandbox);
                let cancel = cancel.clone();
                let progress = progress.clone();
                async move {
                    let entry = &entries[index];
                    entry
                        .engine
                        .run(sandbox, &entry.round, ctx, cancel, progress)
                        .await
                }
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::coding_engine::claude_code::claude_limit_reached;

    #[test]
    fn a_pinned_round_runs_on_the_entry_its_job_ran_on_alone() {
        let pinned = |mark: &str| CodingRound {
            jobs: crate::claude_code::RoundJobs::resuming("r1", chrono::Utc::now(), mark),
            ..CodingRound::one_shot(String::new(), Duration::ZERO)
        };
        assert_eq!(pinned_entry(&pinned("r1.1.0"), 2).unwrap().unwrap(), 1);
        assert!(pinned_entry(&pinned("r1.2.0"), 2).unwrap().is_err());
        assert!(pinned_entry(&CodingRound::one_shot(String::new(), Duration::ZERO), 2).is_none());
    }

    #[test]
    fn each_entry_runs_the_round_under_job_ids_of_its_own() {
        let round = CodingRound {
            model: Some("opus".into()),
            jobs: Some(crate::claude_code::RoundJobs::new(
                "r1".into(),
                chrono::Utc::now(),
            )),
            ..CodingRound::one_shot(String::new(), Duration::ZERO)
        };
        let first = placed(&round, 0, None);
        let second = placed(&round, 1, Some(&"gpt-5".to_string()));
        assert_eq!(first.jobs.unwrap().job().as_str(), "r1.0.0");
        assert_eq!(second.jobs.unwrap().job().as_str(), "r1.1.0");
        assert_eq!(first.model.as_deref(), Some("opus"));
        assert_eq!(second.model.as_deref(), Some("gpt-5"));
        assert!(placed(
            &CodingRound::one_shot(String::new(), Duration::ZERO),
            1,
            None
        )
        .jobs
        .is_none());
    }

    /// An engine that knows only Claude's limit messages.
    struct Claude;

    #[async_trait]
    impl CodingEngine for Claude {
        fn label(&self) -> &'static str {
            "claude-code"
        }
        fn preflight(&self, _: &RoundContext<'_>) -> Result<(), Refusal> {
            Ok(())
        }
        fn limit_reached(&self, text: &str) -> bool {
            claude_limit_reached(text)
        }
        fn model_choices(&self) -> ModelChoices {
            crate::coding_engine::claude_code::claude_model_choices()
        }
        async fn run(
            &self,
            _: Arc<dyn CodingSandbox>,
            _: &CodingRound,
            _: &RoundContext<'_>,
            _: Option<CancellationToken>,
            _: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
        ) -> Result<CliRoundResult> {
            unreachable!()
        }
    }

    fn trigger_of(result: &Result<CliRoundResult>) -> Option<FallbackTrigger> {
        super::trigger_of(&Claude, result)
    }

    async fn run_chain<F, Fut>(
        attempts: &[usize],
        advances: impl Fn(FallbackTrigger) -> bool,
        attempt: F,
    ) -> Result<CliRoundResult>
    where
        F: FnMut(usize) -> Fut,
        Fut: Future<Output = Result<CliRoundResult>>,
    {
        let round = CodingRound {
            model: Some("claude-opus-4-8".into()),
            ..CodingRound::one_shot(String::new(), Duration::ZERO)
        };
        let entries: Vec<Entry<'_>> = (0..3)
            .map(|_| Entry {
                engine: &Claude,
                model: round.model.clone(),
                round: round.clone(),
            })
            .collect();
        super::run_chain(&entries, attempts, advances, attempt).await
    }

    fn ok() -> Candidate {
        Candidate {
            metered: false,
            preflight: Ok(()),
        }
    }

    fn no_token() -> Candidate {
        Candidate {
            metered: false,
            preflight: Err(Refusal::no_token("no token")),
        }
    }

    fn cooling() -> Candidate {
        Candidate {
            metered: false,
            preflight: Err(Refusal {
                reason: "at its usage limit".into(),
                trigger: FallbackTrigger::RateLimited,
            }),
        }
    }

    /// Whether a chain moving on for `triggers` moves on for `trigger`.
    fn on(triggers: &[FallbackTrigger]) -> impl Fn(FallbackTrigger) -> bool + '_ {
        move |trigger| triggers.contains(&trigger)
    }

    fn metered() -> Candidate {
        Candidate {
            metered: true,
            preflight: Ok(()),
        }
    }

    fn failed(text: &str) -> Result<CliRoundResult> {
        Ok(CliRoundResult {
            text: text.into(),
            completion: Completion::Failed,
            exit_code: 1,
            ..Default::default()
        })
    }

    fn done(engine: &str) -> Result<CliRoundResult> {
        Ok(CliRoundResult {
            engine: engine.into(),
            ..Default::default()
        })
    }

    #[test]
    fn a_plan_keeps_order_skips_metered_when_disallowed_and_names_the_key() {
        let no_token = on(&[FallbackTrigger::NoToken]);
        assert_eq!(
            plan(&[ok(), metered(), ok()], false, &no_token).unwrap(),
            vec![0, 2]
        );
        assert_eq!(
            plan(&[ok(), metered(), ok()], true, &no_token).unwrap(),
            vec![0, 1, 2]
        );
        let err = plan(&[self::no_token(), metered()], false, &no_token).unwrap_err();
        assert!(
            err.reason.contains("no token") && err.reason.contains("allow_metered"),
            "{err}"
        );
        assert_eq!(
            plan(&[self::no_token(), ok()], false, &no_token).unwrap(),
            vec![1]
        );
    }

    #[test]
    fn a_primary_without_a_token_is_final_unless_the_chain_advances_on_it() {
        let err = plan(&[no_token(), ok()], false, on(&[])).unwrap_err();
        assert_eq!(err.reason, "no token");
    }

    #[test]
    fn a_primary_whose_login_cools_falls_back_as_a_limit_does() {
        assert_eq!(
            plan(
                &[cooling(), ok()],
                false,
                on(&[FallbackTrigger::RateLimited])
            )
            .unwrap(),
            vec![1]
        );
        let err = plan(&[cooling(), ok()], false, on(&[FallbackTrigger::NoToken])).unwrap_err();
        assert_eq!(err.trigger, FallbackTrigger::RateLimited);
    }

    #[test]
    fn triggers_follow_the_completion_and_the_limit_messages() {
        assert_eq!(
            trigger_of(&failed(
                "You've reached your Fable limit. Switch to another model to continue."
            )),
            Some(FallbackTrigger::RateLimited)
        );
        assert_eq!(
            trigger_of(&failed("You've hit your weekly limit · resets Sep 18, 9am")),
            Some(FallbackTrigger::RateLimited)
        );
        assert_eq!(
            trigger_of(&failed("API Error: Request rejected (429)")),
            Some(FallbackTrigger::RateLimited)
        );
        assert_eq!(
            trigger_of(&failed("segfault")),
            Some(FallbackTrigger::Failure)
        );
        assert_eq!(
            trigger_of(&Err(eyre::eyre!("spawn failed"))),
            Some(FallbackTrigger::Failure)
        );
        assert_eq!(
            trigger_of(&Err(eyre::eyre!("round cancelled by the caller"))),
            None,
            "a cancelled round hands over to nothing"
        );
        assert_eq!(
            trigger_of(&Err(eyre::eyre!("API Error (429) rate limit"))),
            Some(FallbackTrigger::RateLimited)
        );
        assert_eq!(trigger_of(&done("x")), None);
        for completion in [
            Completion::TurnCap,
            Completion::Timeout,
            Completion::SessionNotFound,
        ] {
            let result = Ok(CliRoundResult {
                completion,
                exit_code: 1,
                ..Default::default()
            });
            assert_eq!(trigger_of(&result), None, "{completion:?} never hands over");
        }
    }

    #[tokio::test]
    async fn the_chain_hands_over_on_allowed_triggers_only() {
        let advances = |t: FallbackTrigger| t == FallbackTrigger::RateLimited;
        let scripted = |index: usize| async move {
            match index {
                0 => failed("You've reached your Fable limit."),
                1 => done("opus"),
                _ => unreachable!(),
            }
        };
        let result = run_chain(&[0, 1, 2], advances, scripted).await.unwrap();
        assert_eq!(result.engine, "opus");

        let stays = run_chain(&[0, 1], advances, |index| async move {
            match index {
                0 => failed("segfault"),
                _ => unreachable!("a plain failure does not advance here"),
            }
        })
        .await
        .unwrap();
        assert_eq!(stays.completion, Completion::Failed);

        let last = run_chain(
            &[0, 1],
            |_| true,
            |_| async { failed("limit reached your") },
        )
        .await
        .unwrap();
        assert_eq!(
            last.completion,
            Completion::Failed,
            "the last entry's result stands"
        );
    }

    /// A failed attempt that ran on `engine` (empty: the engine did not name
    /// itself) and spent `cost_usd`.
    fn spent(engine: &str, text: &str, cost_usd: f64) -> Result<CliRoundResult> {
        Ok(CliRoundResult {
            engine: engine.into(),
            model: Some("claude-opus-4-8".into()),
            input_tokens: 1_000,
            output_tokens: 50,
            cost_usd,
            ..failed(text).unwrap()
        })
    }

    const FABLE_LIMIT: &str = "You've reached your Fable limit.";

    /// What the chain itemises for an attempt `spent` built and left with `error`.
    fn attempt(engine: &str, cost_usd: f64, error: &str) -> EngineAttempt {
        EngineAttempt {
            engine: engine.into(),
            model: Some("claude-opus-4-8".into()),
            input_tokens: 1_000,
            output_tokens: 50,
            cost_usd,
            error: Some(error.into()),
        }
    }

    #[tokio::test]
    async fn the_chain_itemises_the_attempts_it_handed_over_from() {
        let result = run_chain(
            &[0, 1, 2],
            |_| true,
            |index| async move {
                match index {
                    0 => spent("", FABLE_LIMIT, 0.75),
                    1 => spent("codex", "segfault", 0.25),
                    _ => done("claude-code:bedrock"),
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(result.engine, "claude-code:bedrock");
        assert_eq!(
            result.handed_over,
            vec![
                attempt("claude-code", 0.75, FABLE_LIMIT),
                attempt("codex", 0.25, "segfault")
            ]
        );
    }

    #[tokio::test]
    async fn an_attempt_that_errored_is_itemised_with_its_error() {
        let result = run_chain(
            &[0, 1],
            |_| true,
            |index| async move {
                match index {
                    0 => Err(eyre::eyre!("output consumer dropped")
                        .wrap_err("secure command runner unavailable")),
                    _ => done("codex"),
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(result.engine, "codex");
        assert_eq!(
            result.handed_over,
            vec![EngineAttempt {
                engine: "claude-code".into(),
                model: Some("claude-opus-4-8".into()),
                error: Some("secure command runner unavailable: output consumer dropped".into()),
                ..Default::default()
            }]
        );
    }

    #[tokio::test]
    async fn an_errored_attempt_keeps_only_the_first_line_and_no_secret() {
        let secret = concat!("ghp_EX", "AMPLE00000000000000000000000000000");
        let handed = run_chain(
            &[0, 1, 2],
            |_| true,
            |index| async move {
                match index {
                    0 => Err(eyre::eyre!("boom\nsecond line {secret}")),
                    1 => Err(eyre::eyre!(
                        "{} {secret} {}",
                        "x".repeat(290),
                        "y".repeat(700)
                    )),
                    _ => done("codex"),
                }
            },
        )
        .await
        .unwrap()
        .handed_over;
        assert_eq!(handed[0].error.as_deref(), Some("boom"));
        let clipped = handed[1].error.as_deref().unwrap();
        assert_eq!(clipped.chars().count(), 301);
        assert!(clipped.ends_with('…'));
        assert!(clipped.contains("[REDACTED"), "{clipped}");
        assert!(
            !clipped.contains("ghp_"),
            "a secret the cut falls across is redacted before it: {clipped}"
        );
    }

    #[tokio::test]
    async fn a_final_error_keeps_the_handed_over_attempts_and_its_message() {
        let last_error = || eyre::eyre!("socket closed").wrap_err("API Error (429) rate limit");
        let e = run_chain(
            &[0, 1],
            |_| true,
            |index| async move {
                match index {
                    0 => spent("claude-code", "You've reached your Fable limit.", 0.5),
                    _ => Err(last_error()),
                }
            },
        )
        .await
        .err()
        .expect("the chain fails");
        assert_eq!(
            handed_over(&e),
            vec![attempt("claude-code", 0.5, FABLE_LIMIT)]
        );
        let original = last_error();
        assert_eq!(e.to_string(), original.to_string());
        assert_eq!(format!("{e:#}"), format!("{original:#}"));
        assert_eq!(
            e.chain().map(ToString::to_string).collect::<Vec<_>>(),
            original
                .chain()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
        assert!(is_rate_limited(&e.to_string()));
        assert_eq!(
            trigger_of(&Err(e)),
            Some(FallbackTrigger::RateLimited),
            "a wrapped error reads exactly as the one it wraps"
        );

        let cancelled = run_chain(
            &[0, 1],
            |_| true,
            |index| async move {
                match index {
                    0 => spent("claude-code", "You've reached your Fable limit.", 0.5),
                    _ => Err(eyre::eyre!("round cancelled by the caller")),
                }
            },
        )
        .await
        .err()
        .expect("the chain fails");
        assert_eq!(cancelled.to_string(), "round cancelled by the caller");
        assert_eq!(handed_over(&cancelled).len(), 1);
        assert_eq!(trigger_of(&Err(cancelled)), None);
    }

    #[tokio::test]
    async fn a_chain_that_stops_early_keeps_what_it_handed_over() {
        let advances = |t: FallbackTrigger| t == FallbackTrigger::RateLimited;
        let stopped = run_chain(&[0, 1, 2], advances, |index| async move {
            match index {
                0 => spent("claude-code", "You've reached your Fable limit.", 0.5),
                1 => spent("codex", "segfault", 0.25),
                _ => unreachable!("a plain failure does not advance here"),
            }
        })
        .await
        .unwrap();
        assert_eq!(stopped.engine, "codex");
        assert_eq!(stopped.completion, Completion::Failed);
        assert_eq!(
            stopped.handed_over,
            vec![attempt("claude-code", 0.5, FABLE_LIMIT)]
        );

        let cancelled = run_chain(&[0, 1, 2], advances, |index| async move {
            match index {
                0 => spent("claude-code", "You've reached your Fable limit.", 0.5),
                1 => Err(eyre::eyre!("round cancelled by the caller")),
                _ => unreachable!("a cancelled round hands over to nothing"),
            }
        })
        .await
        .err()
        .expect("the chain fails");
        assert_eq!(cancelled.to_string(), "round cancelled by the caller");
        assert_eq!(handed_over(&cancelled).len(), 1);
    }

    #[tokio::test]
    async fn a_single_attempt_is_untouched() {
        let served = run_chain(&[0], |_| true, |_| async { done("claude-code") })
            .await
            .unwrap();
        assert!(served.handed_over.is_empty());

        let e = run_chain(
            &[0],
            |_| true,
            |_| async { Err(eyre::eyre!("spawn failed")) },
        )
        .await
        .err()
        .expect("the chain fails");
        assert!(e.downcast_ref::<HandedOver>().is_none());
        assert!(handed_over(&e).is_empty());
        assert_eq!(e.to_string(), "spawn failed");
    }
}
