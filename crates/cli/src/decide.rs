//! `workforce decide` — an install's own decisions as training data, built
//! by the same state builders the hub sends with.
//!
//! - `gather` reads the install's stores (workflow runs and the definitions
//!   they ran under, decision records, turn traces), read-only, and writes
//!   recorded examples as JSONL.
//! - `export-states` turns examples into `{state, questions, label}`
//!   training records: `state` and `questions` are what a live request
//!   carries for that input.
//! - `ask` puts examples to the configured backends as a hub would and
//!   writes each pick with its probability and latency.
//!
//! An example is one JSON object per line, tagged with its use; `id`,
//! `source`, `weight`, `day` and `meta` are carried through to its record:
//!
//! ```json
//! {"use":"branch_fallback","step":"Review","task":"…","response":"…","outputs":{},"report":null,
//!  "branches":[{"label":"approved","leads_to":"Merge the PR"}],"label":"approved",
//!  "id":"branch:<run>:3","source":"agent","weight":1.0,"day":"2026-10-01","meta":{…}}
//! {"use":"scope_gate","message":"…","topic":"…","label":true}
//! {"use":"routing","message":"…","agent":"ops","channel":"slack","label":null}
//! {"use":"decision_step","task":"…","questions":{…},"label":"bug"}
//! {"use":"tool","state":{…},"questions":{…},"label":{"q1":"yes"}}
//! {"use":"decide_first","state":"…","questions":{…},"floors":{…},"label":"ignore"}
//! ```
//!
//! Counts, and only counts, go to standard error: what was written and what
//! was left out, by reason.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use clap::{Args, Subcommand, ValueEnum};
use eyre::{bail, Context, Result};
use workforce_common::settings::WorkforceConfig;
use workforce_decide::{export, RequestBudget};
use workforce_hub::decide_gather::{Gather, Stores};

#[derive(Subcommand, Debug)]
pub enum DecideCommand {
    /// Gather recorded examples (JSONL) from this install's stores,
    /// read-only.
    Gather(GatherArgs),
    /// Turn recorded examples (JSONL) into `{state, questions, label}`
    /// training records (JSONL).
    ExportStates(ExportArgs),
    /// Put recorded examples to the configured decision backends and write
    /// each pick with its probability and latency.
    Ask(AskArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GatherUse {
    Routing,
    Branch,
    Scope,
    /// Agent steps asked before their turn, labelled by the turn.
    #[value(name = "decide_first")]
    DecideFirst,
}

impl From<GatherUse> for Gather {
    fn from(usage: GatherUse) -> Self {
        match usage {
            GatherUse::Routing => Gather::Routing,
            GatherUse::Branch => Gather::Branch,
            GatherUse::Scope => Gather::Scope,
            GatherUse::DecideFirst => Gather::DecideFirst,
        }
    }
}

#[derive(Args, Debug)]
pub struct GatherArgs {
    /// The install's `workforce.toml`.
    #[arg(long)]
    config: PathBuf,
    /// The use to gather examples for.
    #[arg(long = "use", value_enum)]
    usage: GatherUse,
    /// How far back to read: a span (`30d`, `12h`) or a moment
    /// (`2026-09-01T00:00:00Z`).
    #[arg(long, default_value = "30d")]
    since: String,
    /// The most examples to write.
    #[arg(long, default_value_t = 10_000)]
    limit: usize,
    /// Where system workflow definitions live, for a run that recorded no
    /// revision. Relative to the config's directory.
    #[arg(long, default_value = "workflows")]
    workflows_dir: PathBuf,
    /// Where the examples go. Standard output when omitted.
    #[arg(short, long)]
    out: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Recorded examples, one JSON object per line. Standard input when
    /// omitted.
    #[arg(long)]
    input: Option<PathBuf>,
    /// Where the records go. Standard output when omitted.
    #[arg(short, long)]
    out: Option<PathBuf>,
    /// Characters of text a whole request may carry, state and questions
    /// together: the `state_chars` of the use the model is being trained
    /// for.
    #[arg(long, default_value_t = RequestBudget::DEFAULT.get())]
    state_chars: usize,
}

#[derive(Args, Debug)]
pub struct AskArgs {
    /// The install's `workforce.toml`: its `[decision]` names the backends,
    /// the budgets and the slots.
    #[arg(long)]
    config: PathBuf,
    /// Ask with this agent's vault under the global one, as its steps do.
    #[arg(long)]
    agent: Option<String>,
    /// Recorded examples, one JSON object per line. Standard input when
    /// omitted.
    #[arg(long)]
    input: Option<PathBuf>,
    /// Where the picks go. Standard output when omitted.
    #[arg(short, long)]
    out: Option<PathBuf>,
}

fn reader(path: Option<&Path>) -> Result<Box<dyn BufRead>> {
    Ok(match path {
        Some(path) => Box::new(BufReader::new(
            File::open(path).wrap_err_with(|| format!("open {}", path.display()))?,
        )),
        None => Box::new(io::stdin().lock()),
    })
}

/// Where the output goes: `path`, readable by its owner alone since what is
/// written holds what people asked, or standard output.
fn writer(path: Option<&Path>) -> Result<Box<dyn Write>> {
    let Some(path) = path else {
        return Ok(Box::new(io::stdout().lock()));
    };
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .wrap_err_with(|| format!("create {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .wrap_err_with(|| format!("restrict {}", path.display()))?;
    }
    Ok(Box::new(BufWriter::new(file)))
}

/// `since` as a moment: a span back from now, or the moment itself.
fn since(text: &str) -> Result<DateTime<Utc>> {
    use workforce_common::activity_log::{parse_absolute_time, parse_relative_time};
    parse_relative_time(text)
        .or_else(|_| parse_absolute_time(text))
        .wrap_err_with(|| format!("--since {text:?} is neither a span (30d) nor a moment"))
}

/// What was left out and why, as one line of counts.
fn left_out(counts: &BTreeMap<&'static str, usize>) -> String {
    match counts.is_empty() {
        true => "nothing left out".to_string(),
        false => counts
            .iter()
            .map(|(why, count)| format!("{count} left out: {why}"))
            .collect::<Vec<_>>()
            .join("; "),
    }
}

pub async fn run(command: DecideCommand) -> Result<()> {
    match command {
        DecideCommand::Gather(args) => {
            let config = WorkforceConfig::load(&args.config)?;
            let root = args.config.parent().unwrap_or(Path::new("."));
            let stores = Stores::read_only(&config, root.join(&args.workflows_dir))?;
            let mut out = writer(args.out.as_deref())?;
            let tally = stores
                .gather(
                    args.usage.into(),
                    since(&args.since)?,
                    args.limit,
                    |example| Ok(writeln!(out, "{}", serde_json::to_string(example)?)?),
                )
                .await?;
            out.flush()?;
            eprintln!("{} examples; {}", tally.examples, left_out(&tally.left_out));
            Ok(())
        }
        DecideCommand::ExportStates(args) => {
            let written = export_states(
                reader(args.input.as_deref())?,
                writer(args.out.as_deref())?,
                RequestBudget::chars(args.state_chars),
            )?;
            eprintln!(
                "{} records; {}",
                written.records,
                left_out(&written.left_out)
            );
            Ok(())
        }
        DecideCommand::Ask(args) => {
            let config = WorkforceConfig::load(&args.config)?;
            let Some(decisions) = workforce_hub::decide::build(&config.decision)? else {
                bail!(
                    "[decision] is off in {}: there is no backend to ask",
                    args.config.display()
                );
            };
            // Nobody is asking: only the shared tiers hold a key, exactly
            // as for work a hub does with no person behind it. The vaults
            // are read as they stand: this runs beside a live hub, possibly
            // from a newer binary, and must change nothing under it.
            let (agent, global) =
                workforce_hub::vault::open_readers(&config, args.agent.as_deref()).await?;
            let caller = workforce_hub::credentials::Caller::new(None, agent, global);
            let asked = workforce_hub::decide_ask::ask_examples(
                &decisions,
                &caller,
                reader(args.input.as_deref())?,
                writer(args.out.as_deref())?,
            )
            .await?;
            eprintln!(
                "{} answered; {} with nothing to decide about left out",
                asked.answered, asked.empty
            );
            match asked.busy_at {
                Some(line) => bail!(
                    "stopped at line {line}: the decision model is busy, or its fallback answered \
                     for it. Nothing from that line on is in the output; run the rest when it is free"
                ),
                None => Ok(()),
            }
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Written {
    records: usize,
    /// Examples that made no record, by reason.
    left_out: BTreeMap<&'static str, usize>,
}

/// Every non-blank line of `input` as a record on `out`.
fn export_states(
    input: impl BufRead,
    mut out: impl Write,
    budget: RequestBudget,
) -> Result<Written> {
    let mut written = Written::default();
    for (number, line) in input.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record = export::line(&line, budget)
            .wrap_err_with(|| format!("line {}: not a recorded example", number + 1))?;
        match record {
            Ok(record) => {
                writeln!(out, "{record}")?;
                written.records += 1;
            }
            Err(why) => *written.left_out.entry(why.as_str()).or_default() += 1,
        }
    }
    out.flush()?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn each_example_line_becomes_one_record_line_built_by_the_live_builders() {
        let message = "Import broke.\n```\na\nb\nc\n```\nWhich deploy did it?";
        let input = format!(
            "{}\n\n{}\n{}\n{}\n",
            json!({"use": "routing", "message": message, "label": "investigation",
                   "id": "trace:t1", "source": "person", "weight": 3.0, "day": "2026-10-01",
                   "meta": {"session": "s1"}}),
            json!({"use": "scope_gate", "message": "hi", "topic": "imports", "label": false}),
            json!({"use": "routing", "message": " ", "label": "ambiguous"}),
            json!({"use": "routing", "message": "Deploy it", "label": null}),
        );
        let mut out = Vec::new();
        let written = export_states(input.as_bytes(), &mut out, RequestBudget::DEFAULT).unwrap();
        assert_eq!(
            written,
            Written {
                records: 2,
                left_out: BTreeMap::from([("nothing to decide about", 1), ("no label", 1)]),
            }
        );
        assert_eq!(
            left_out(&written.left_out),
            "1 left out: no label; 1 left out: nothing to decide about"
        );
        assert_eq!(left_out(&BTreeMap::new()), "nothing left out");
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            Some(&lines[0]["state"]),
            workforce_decide::request::state(message, workforce_decide::StateBudget::DEFAULT)
                .as_ref()
        );
        assert_eq!(
            lines[0]["id"], "trace:t1",
            "a record joins back to its example"
        );
        assert_eq!(lines[0]["source"], "person");
        assert_eq!(lines[0]["weight"], 3.0);
        assert_eq!(lines[0]["day"], "2026-10-01");
        assert_eq!(lines[0]["meta"], json!({"session": "s1"}));
        assert!(lines[1].get("id").is_none());
        assert_eq!(lines[0]["label"], "investigation");
        assert_eq!(lines[0]["questions"]["use_case"]["type"], "choice");
        assert_eq!(lines[1]["questions"]["on_topic"]["type"], "noul");
        assert_eq!(lines[1]["label"], false);
    }

    #[test]
    fn a_bad_line_names_itself() {
        let err = export_states(
            "{\"use\":\"routing\"}\n".as_bytes(),
            Vec::new(),
            RequestBudget::DEFAULT,
        )
        .unwrap_err();
        assert!(err.to_string().contains("line 1"), "{err}");
    }

    #[test]
    fn gather_takes_decide_first_as_its_config_spells_it() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(subcommand)]
            command: DecideCommand,
        }
        let parsed = <Cli as clap::Parser>::try_parse_from([
            "decide",
            "gather",
            "--config",
            "workforce.toml",
            "--use",
            "decide_first",
        ])
        .unwrap();
        let DecideCommand::Gather(args) = parsed.command else {
            panic!("gather expected");
        };
        assert_eq!(Gather::from(args.usage), Gather::DecideFirst);
    }

    #[test]
    fn since_is_a_span_back_from_now_or_a_moment() {
        let span = since("2d").unwrap();
        let back = Utc::now() - span;
        assert!((back.num_hours() - 48).abs() <= 1, "{back}");
        assert!(since("not a time").is_err());
    }
}
