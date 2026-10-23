//! `workforce eval`: run a golden set of questions against an agent on a
//! running hub, as a person, and score the answers.
//!
//! - `run` asks one agent every question and prints scores per use case.
//! - `compare` does the same for several arms and prints the paired
//!   differences from the first.
//! - `report` prints the scores of a run directory again, without the hub.
//! - `check` validates a set and prints each question's version; with
//!   `--know-how`, how many know-how entries each question would be shown.
//! - `restore` puts back a test copy's config a dead run left overridden.
//!
//! A run directory holds the turns, the grades and `full.json` (questions,
//! answers and the judge's reasons), which stay on the host, and
//! `scores.json`, which holds numbers only and may leave it. Nothing a
//! question or an answer said is ever printed.

mod arms;
mod harness;
mod hub;
mod report;
mod results;
mod set;

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand};
use eyre::{Result, WrapErr};

use arms::Arm;
use results::{write_private, Judge, Results, Store};
use set::EvalSet;
use workforce_hub::know_how::preview::Preview;

const SCORES_FILE: &str = "scores.json";
const FULL_FILE: &str = "full.json";

/// How long an arm's overrides are given to reach every replica of the hub
/// before its first question. Every answer still says which config it ran
/// under, so one that came too early is caught, not counted.
const OVERRIDES_SETTLE: Duration = Duration::from_secs(5);

#[derive(Subcommand, Debug)]
pub enum EvalCommand {
    /// Ask one agent every question of a set and print scores per use case.
    Run(RunArgs),
    /// Run a set against several arms and print each one's difference from
    /// the first.
    Compare(CompareArgs),
    /// Print the scores of a run directory again, without calling the hub.
    Report(ReportArgs),
    /// Validate a set and print each question's version, and with
    /// `--know-how` how many know-how entries an agent would show you with
    /// each question.
    Check(CheckArgs),
    /// Put back the config of a test copy that a run which died left
    /// overridden, without running anything.
    Restore(RestoreArgs),
}

#[derive(Args, Debug)]
pub struct Common {
    /// The set's directory: `set.toml` and one TOML file per question.
    #[arg(long)]
    set: PathBuf,
    /// The run directory. A run that stopped carries on from what it holds.
    #[arg(long)]
    out: PathBuf,
    /// Gateway URL (or set WORKFORCE_GATEWAY_URL env var).
    #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
    url: String,
    /// The token of the person the run is made as: turns are theirs, with
    /// their credentials, memory and budget (or set WORKFORCE_GATEWAY_TOKEN
    /// env var).
    #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN", hide_env_values = true)]
    token: String,
    /// How many turns may be in flight at once.
    #[arg(long, default_value_t = 1)]
    concurrency: usize,
    /// Ask again the questions whose turn failed, and grade again the
    /// answers that went ungraded.
    #[arg(long)]
    retry: bool,
    /// Ask only the set's first N questions: a cheap check that an agent
    /// reaches its data before the whole set is spent on it.
    #[arg(long)]
    limit: Option<usize>,
    /// The longest one turn may take, in seconds.
    #[arg(long, default_value_t = 1800)]
    turn_timeout_secs: u64,
    /// The agent whose model routes the judge runs on, for every arm alike.
    /// Defaults to the first arm's agent.
    #[arg(long)]
    grader_agent: Option<String>,
    /// The judge's model, as a tier or a routed spec: where the install has
    /// one, a model of another family than the one under test. Defaults to
    /// the grader agent's default model.
    #[arg(long)]
    grader_model: Option<String>,
    /// Run a test copy even though it lacks credentials its original has.
    #[arg(long)]
    allow_missing_credentials: bool,
    /// Leave each turn's session on the hub. They are deleted once their
    /// answer is recorded, so no later turn can find an earlier answer.
    #[arg(long)]
    keep_sessions: bool,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// The agent to ask.
    #[arg(long)]
    agent: String,
    #[command(flatten)]
    common: Common,
}

#[derive(Args, Debug)]
pub struct CompareArgs {
    /// The arms file: `[[arm]]` tables, the first being the baseline.
    #[arg(long)]
    arms: PathBuf,
    #[command(flatten)]
    common: Common,
}

#[derive(Args, Debug)]
pub struct ReportArgs {
    #[arg(long)]
    set: PathBuf,
    #[arg(long)]
    out: PathBuf,
}

#[derive(Args, Debug)]
pub struct CheckArgs {
    #[arg(long)]
    set: PathBuf,
    /// The agent, a test copy usually, whose know-how to count for each
    /// question, as your eval turns would be shown it: no turn is run.
    #[arg(long, value_name = "AGENT")]
    know_how: Option<String>,
    /// Gateway URL, for `--know-how` (or set WORKFORCE_GATEWAY_URL env var).
    #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
    url: Option<String>,
    /// Your own sign-in token, for `--know-how` (or set
    /// WORKFORCE_GATEWAY_TOKEN env var).
    #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN", hide_env_values = true)]
    token: Option<String>,
}

/// Fewer questions the arms score differently than this can never make a
/// sign test distinguishable at p < 0.05.
const SIGN_TEST_MIN: usize = 6;

/// `set`'s questions with their versions, and with what `seen` counted of
/// `agent`'s know-how for them when it was asked to.
fn checked(set: &EvalSet, seen: Option<(&str, &Preview)>) -> String {
    let mut out = format!(
        "{} ({} questions) {}\n",
        set.name,
        set.questions.len(),
        set.description
    );
    let matched = |i: usize| seen.map(|(_, p)| p.matched.get(i).copied().unwrap_or(0));
    for (i, question) in set.questions.iter().enumerate() {
        let shown = matched(i).map(|n| format!(" {n:>3}")).unwrap_or_default();
        out.push_str(&format!(
            "{:<24} {:<14} {}{shown}\n",
            question.id,
            question.use_case.as_str(),
            question.hash()
        ));
    }
    let Some((agent, seen)) = seen else {
        return out;
    };
    let with = seen.matched.iter().filter(|n| **n > 0).count();
    out.push_str(&format!(
        "\nknow-how on {agent}, for you: {} entries visible; {with} of {} questions would be \
         shown some (min_score {})\n",
        seen.visible,
        set.questions.len(),
        seen.min_score
    ));
    let notes = [
        (!seen.matching).then_some(
            "the hub has no embedder, so know-how is never shown with a request".to_string(),
        ),
        (!seen.enabled).then(|| {
            format!("know-how is off on {agent} as configured; counted as an arm turning it on would see it")
        }),
        (seen.enabled && !seen.shown).then(|| {
            format!("{agent} matches know-how without showing it (inject is not \"on\")")
        }),
        (with < SIGN_TEST_MIN).then(|| {
            format!(
                "a know-how comparison needs at least {SIGN_TEST_MIN} questions the arms score \
                 differently, and only {with} would be shown any: it cannot show a difference"
            )
        }),
    ];
    for note in notes.into_iter().flatten() {
        out.push_str(&format!("note: {note}\n"));
    }
    out
}

#[derive(Args, Debug)]
pub struct RestoreArgs {
    /// The run directory of the run that died.
    #[arg(long)]
    out: PathBuf,
    /// Gateway URL (or set WORKFORCE_GATEWAY_URL env var).
    #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
    url: String,
    /// A system admin's sign-in token (or set WORKFORCE_GATEWAY_TOKEN env
    /// var).
    #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN", hide_env_values = true)]
    token: String,
}

/// Write both forms of the run in `dir` there, and print the scores.
fn publish(dir: &Path, set: &EvalSet) -> Result<()> {
    let full = report::report(&Results::read(dir)?, set);
    let scores = dir.join(SCORES_FILE);
    std::fs::write(&scores, serde_json::to_string_pretty(&full.scores)?)
        .wrap_err_with(|| format!("write {}", scores.display()))?;
    write_private(&dir.join(FULL_FILE), &serde_json::to_string_pretty(&full)?)?;
    print!("{}", full.render());
    eprintln!(
        "\nNumbers only (safe to take off the host): {}\nNames, questions, answers and reasons \
         (keep on the host): {}",
        scores.display(),
        dir.join(FULL_FILE).display()
    );
    Ok(())
}

async fn execute(arms: Vec<Arm>, common: Common) -> Result<()> {
    let set = EvalSet::load(&common.set)?;
    let options = harness::Options {
        concurrency: common.concurrency,
        retry: common.retry,
        limit: common.limit,
        turn_timeout: Duration::from_secs(common.turn_timeout_secs),
        allow_missing_credentials: common.allow_missing_credentials,
        keep_sessions: common.keep_sessions,
        settle: OVERRIDES_SETTLE,
    };
    let judge = Judge {
        agent: common.grader_agent.unwrap_or_else(|| arms[0].agent.clone()),
        model: common.grader_model,
    };
    let hub = hub::Hub::new(&common.url, &common.token)?;
    let store = Store::open(&common.out, &set, &arms, judge)?;
    let ran = harness::run(&hub, &set, &arms, &store, &options).await;
    if ran.is_ok() || !store.results()?.turns.is_empty() {
        publish(&common.out, &set)?;
    }
    ran
}

pub async fn run(command: EvalCommand) -> Result<()> {
    match command {
        EvalCommand::Run(args) => execute(vec![Arm::of_agent(&args.agent)], args.common).await,
        EvalCommand::Compare(args) => execute(arms::load(&args.arms)?, args.common).await,
        EvalCommand::Report(args) => publish(&args.out, &EvalSet::load(&args.set)?),
        EvalCommand::Restore(args) => {
            harness::restore(&hub::Hub::new(&args.url, &args.token)?, &args.out).await
        }
        EvalCommand::Check(args) => {
            let set = EvalSet::load(&args.set)?;
            let seen = match &args.know_how {
                None => None,
                Some(agent) => {
                    let (Some(url), Some(token)) = (&args.url, &args.token) else {
                        eyre::bail!("--know-how needs the hub's --url and your --token");
                    };
                    let questions: Vec<String> =
                        set.questions.iter().map(|q| q.question.clone()).collect();
                    let hub = hub::Hub::new(url, token)?;
                    hub.me().await?;
                    Some((agent.as_str(), hub.know_how(agent, None, &questions).await?))
                }
            };
            print!(
                "{}",
                checked(&set, seen.as_ref().map(|(agent, p)| (*agent, p)))
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(matched: Vec<usize>) -> Preview {
        Preview {
            enabled: false,
            shown: false,
            matching: true,
            source_agent: Some("analyst".into()),
            min_score: 0.75,
            visible: 12,
            matched,
        }
    }

    #[test]
    fn the_pre_check_counts_the_questions_know_how_would_be_shown_with() {
        let set = EvalSet::load(&set::tests::example_dir()).unwrap();
        let plain = checked(&set, None);
        assert!(!plain.contains("know-how"), "{plain}");
        let counted = checked(&set, Some(("analyst-eval", &seen(vec![2, 0, 5]))));
        assert!(
            counted.contains("12 entries visible; 2 of 3 questions would be shown some"),
            "{counted}"
        );
        assert!(
            counted.contains("off on analyst-eval as configured"),
            "{counted}"
        );
        assert!(counted.contains("cannot show a difference"), "{counted}");
        let rows: Vec<&str> = counted.lines().skip(1).take(3).collect();
        assert!(
            rows[0].ends_with("   2") && rows[1].ends_with("   0"),
            "{rows:?}"
        );
        for question in &set.questions {
            assert!(
                !counted.contains(&question.question),
                "a question was printed"
            );
        }
    }
}
