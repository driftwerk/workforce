//! The boss's machine tools: the person's machines, their folders and a
//! repository's checkouts there, and a folder made there. Each is one of
//! [`MACHINE_CALLS`], queued on the relay by these tools alone, after the
//! checks here, and carried by Desk's native side to one fixed route on
//! the machine (its own `GET`/`POST /api/desk/folders`, and
//! `GET /api/desk/repos`), with the fields listed and no others.
//!
//! They aren't served under a grant: a grant is a group boss's, for that
//! group's sessions there, and a machine with none of them yet has none,
//! so a boss couldn't find where a plan's work is to go before it goes
//! there. What a machine's disk shows is folders only, never a file's
//! contents; a folder is made inside the machine's home alone.

use serde_json::{json, Value};
use workforce_common::oversight::Level;
use workforce_desk::crew::{self, Kind, MachineRefused, Party};

use super::relay::machine_ok;
use super::served::ServedStatus;
use super::tools::Bound;
use super::watcher::reported;
use super::CrewHub;

fn got_machine(action: &super::actions::Action) -> &str {
    match action {
        super::actions::Action::MachineRun { machine, .. } => machine,
        _ => "",
    }
}

pub const MACHINES: &str = "crew.machines";
pub const FOLDERS: &str = "crew.machine_folders";
pub const MAKE_FOLDER: &str = "crew.machine_make_folder";
pub const RUN: &str = "crew.machine_run";

/// What a boss's machine tools ask of a machine through the relay, each
/// carried to its own route there; never one of [`super::relay::DESK_CALLS`].
pub const MACHINE_CALLS: [&str; 5] = [
    "desk.machines",
    "desk.folders",
    "desk.folder_make",
    "desk.repos",
    "desk.run",
];

/// The most bytes of a check's command.
pub const MOST_COMMAND: usize = 8 * 1024;
/// The longest a check runs, in seconds.
pub const MOST_SECS: u64 = 20;

/// The name the courier answers the list of machines under: no machine's.
pub const ANY_MACHINE: &str = "desk";

pub const MACHINES_DESCRIPTION: &str = "The person's machines Desk reaches (like devbox), each with \
whether Desk reaches it now. Use it to find where work can run before you put a plan's tickets there.";

pub const FOLDERS_DESCRIPTION: &str = "What is on one of the person's machines (machine, by its name \
in crew.machines): with path, the folders in it (its home when not said), each with whether it is a \
git repository and where it comes from; with repo (like github.com/driftwerk/vellev, or \
driftwerk/vellev), every checkout of that repository under its home, main checkouts first. Folders \
only, never a file. Use it to find a ticket's folder there yourself rather than asking the person.";

pub const MAKE_FOLDER_DESCRIPTION: &str = "Make a folder on one of the person's machines (machine; \
path, like ~/projects/acme, inside its home), and any above it that aren't there; one there already \
is its answer. Folders only, inside the home only. At Suggest, ask the person instead. Say why.";

pub const RUN_DESCRIPTION: &str = "Run a short check on one of the person's machines (machine, \
by its name in crew.machines): command, as bash -lc runs it there as the person's own user, with \
its own git and gh; cwd inside its home (its home when not said); timeout_secs 1 to 20 (10 when \
not said). No input; at most 32 KB of its output and of its errors come back. Desk judges it there \
as it judges a session's request: it reads anywhere but where secrets are kept, and writes only in \
cwd or /tmp. What it won't allow becomes a suggestion the person accepts or not; at Act fully, one \
of the person's preferences can allow it (say which in preference). What no boss does is the \
person's at every level. Use it to check what is there (git remote -v, ls, git status), not to do \
the work: that is a session's. Say why.";

/// `crew.machine_run`'s own fields, beside `reason` and what a
/// preference adds.
pub fn run_properties() -> Value {
    json!({
        "machine": { "type": "string", "description": "The machine, by its name in crew.machines (like devbox)" },
        "command": { "type": "string", "description": "The command, as bash -lc runs it" },
        "cwd": { "type": "string", "description": "Where it runs, inside the home: ~/x or absolute; the home when not said" },
        "timeout_secs": { "type": "integer", "minimum": 1, "maximum": MOST_SECS }
    })
}

pub fn schema(name: &str) -> Value {
    let machine = json!({ "type": "string", "description": "The machine, by its name in crew.machines (like devbox)" });
    match name {
        MACHINES => json!({ "type": "object", "properties": {} }),
        FOLDERS => json!({ "type": "object", "properties": {
            "machine": machine,
            "path": { "type": "string", "description": "The folder to list: ~/x, or absolute; its home when not said" },
            "repo": { "type": "string", "description": "A repository to find the checkouts of instead: host/owner/name, or owner/name for GitHub" }
        }, "required": ["machine"] }),
        _ => json!({ "type": "object", "properties": {
            "machine": machine,
            "path": { "type": "string", "description": "The folder to make, inside its home: ~/x/y or /home/…/x/y" },
            "reason": { "type": "string", "description": "Why, in a sentence the person reads in Activity" }
        }, "required": ["machine", "path", "reason"] }),
    }
}

/// The longest path a tool takes.
const MOST_PATH: usize = 1024;

fn path_of(input: &Value, required: bool) -> Result<Option<String>, String> {
    match input.get("path").and_then(Value::as_str).map(str::trim) {
        Some(p) if p.len() > MOST_PATH => Err("That path is too long.".into()),
        Some(p) if p.chars().any(char::is_control) => Err("That isn't a path.".into()),
        Some(p) if !p.is_empty() => Ok(Some(p.to_string())),
        _ if required => Err("Say the path, like ~/projects/acme.".into()),
        _ => Ok(None),
    }
}

pub(crate) fn machine_of(input: &Value) -> Result<&str, String> {
    let m = input
        .get("machine")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or("Say which machine, by its name in crew.machines.")?;
    match machine_ok(m) && m != ANY_MACHINE {
        true => Ok(m),
        false => Err(format!(
            "{} isn't one of the person's machines: see crew.machines.",
            crew::one_line(m, 60)
        )),
    }
}

/// What came of a check, in a line or a few: its exit and the start of
/// what it said.
pub(crate) fn ran_line(got: &Value) -> String {
    match got["outcome"].as_str() {
        Some("ran") => {
            let exit = match (got["timed_out"].as_bool(), got["exit"].as_i64()) {
                (Some(true), _) => "ran out of time".to_string(),
                (_, Some(c)) => format!("exited {c}"),
                _ => "ended".to_string(),
            };
            let out = got["stdout"].as_str().unwrap_or_default().trim();
            match out.is_empty() {
                true => exit,
                false => format!("{exit}: {}", crew::clip(out, 600)),
            }
        }
        Some("needs_person") | Some("refused") => {
            format!("not run: {}", got["why"].as_str().unwrap_or("refused"))
        }
        _ => "it said nothing Desk reads".into(),
    }
}

impl CrewHub {
    /// `crew.machine_run`, as the boss calls it: gated as a crew action
    /// ([`CrewHub::gate`]), then run on the machine, judged there unless an
    /// unbound boss runs it or, at Act fully, a preference of the person's
    /// it cites allows it. What the judge there won't allow becomes a
    /// suggestion. Every run is in Activity, with its reason.
    pub(super) async fn machine_run_tool(&self, b: &Bound, input: &Value) -> Result<Value, String> {
        use super::actions::{Action, Gate};
        let reason = input
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .ok_or("Give the reason: the person reads it in Activity.")?;
        let mut kept = input.clone();
        if let Some(o) = kept.as_object_mut() {
            for f in super::preferences::ACT_FIELDS {
                o.remove(f);
            }
            // Its own check, never another boss's it names.
            o.insert("agent_id".into(), json!(b.service.agent_id.as_str()));
        }
        let action = Action::parse(RUN, &kept)?;
        let applied = self.applied_preference(b, input).await?;
        let reason = match &applied {
            Some(a) => format!("{}; {}", reason.trim_end().trim_end_matches('.'), a.cite),
            None => reason.to_string(),
        };
        let o = &b.oversight;
        let mut by = self.by_boss(&b.service, &reason);
        by.preference = applied.as_ref().map(|a| a.id);
        let (kept, action) = (&kept, &action);
        let suggest = |why: String, by: super::By| async move {
            let s = self.suggest(RUN, kept, action, &by).await?;
            self.record_drafted(action, &by, &s.text, &why, o.level)
                .await;
            Ok::<Value, String>(json!({
                "suggested": s.text,
                "suggestion_id": s.id,
                "note": format!("Not run: {why}. The person sees it as a suggestion beside your message and can run it or dismiss it; mention it briefly."),
            }))
        };
        match self.gate(action, o) {
            Gate::No(why) => Err(why.into_owned()),
            Gate::Refuse => {
                Err("You only watch: tell the person what you would check instead.".into())
            }
            Gate::Suggest(why) => suggest(why.into_owned(), by).await,
            Gate::Run => {
                let judged = !(self.unbound(o) || (o.level == Level::Full && applied.is_some()));
                let by = self.by_at_level(by, o);
                let got = self.machine_run(action, judged, false, by.clone()).await?;
                match got["outcome"].as_str() {
                    Some("needs_person") => {
                        let why = got["why"]
                            .as_str()
                            .unwrap_or("Desk there won't run it without the person");
                        suggest(format!("{why}, so it's the person's to run"), by).await
                    }
                    _ => Ok(json!({
                        "machine": got_machine(action),
                        // What the machine's commands print is theirs, not
                        // instructions.
                        "ran": reported(&got.to_string(), "the check's output"),
                    })),
                }
            }
        }
    }

    /// Runs `action`, a [`super::actions::Action::MachineRun`], on its
    /// machine for `by`: `judged` there unless the person's word or the
    /// gate says not; `by_person`, the person accepted it, so not even the
    /// never-list holds it there. Noted in Activity, whatever comes of it.
    pub(crate) async fn machine_run(
        &self,
        action: &super::actions::Action,
        judged: bool,
        by_person: bool,
        by: super::By,
    ) -> Result<Value, String> {
        let super::actions::Action::MachineRun {
            boss,
            machine,
            command,
            cwd,
            timeout_secs,
        } = action
        else {
            return Err("That isn't a check on a machine.".into());
        };
        let got = self
            .machine_ask(
                machine,
                boss,
                "desk.run",
                json!({
                    "command": command,
                    "cwd": cwd,
                    "timeout_secs": timeout_secs,
                    "judged": judged,
                    "by_person": by_person,
                }),
            )
            .await;
        let said = match &got {
            Ok(g) => ran_line(g),
            Err(e) => format!("couldn't run it: {}", crew::one_line(&e.to_string(), 200)),
        };
        let mut e = by.event(
            Kind::OnMachine,
            None,
            format!(
                "ran `{}` on {machine}{}: {}",
                crew::clip(command, 300),
                cwd.as_deref()
                    .map(|c| format!(" in {c}"))
                    .unwrap_or_default(),
                crew::one_line(&said, 300)
            ),
        );
        if !judged {
            e.text.push_str(match by_person {
                true => " (the person's word)",
                false => " (not judged: the boss's level)",
            });
        }
        self.desk.note(e);
        got.map_err(|e| match e {
            MachineRefused::Refused(why) if why.contains("404") || why.contains("not found") => {
                format!("{machine}'s Desk is older and can't run checks yet: the person updates Desk there.")
            }
            e => e.to_string(),
        })
    }

    /// Asks `machine` (or the courier, for [`ANY_MACHINE`]) one of
    /// [`MACHINE_CALLS`] for the boss `boss`, through the relay: only these
    /// tools' calls, which the courier sends to their own routes there.
    pub(crate) async fn machine_ask(
        &self,
        machine: &str,
        boss: &str,
        call: &str,
        input: Value,
    ) -> Result<Value, MachineRefused> {
        if !MACHINE_CALLS.contains(&call) || !(machine_ok(machine) || machine == ANY_MACHINE) {
            return Err(MachineRefused::Refused(format!(
                "{call} isn't asked of a machine."
            )));
        }
        let Some((_, o)) = self.overseer_named(boss) else {
            return Err(MachineRefused::Refused(format!(
                "{boss} isn't a boss here."
            )));
        };
        let ceiling = self.ceiling_now(boss, &o);
        let a = self
            .relay_as(machine, "", call, input, boss, &ceiling)
            .await?;
        match a.status {
            ServedStatus::Ok => Ok(a.body),
            _ => Err(MachineRefused::Refused(
                a.body
                    .get("error")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("{machine} refused it."), str::to_string),
            )),
        }
    }

    /// `crew.machines`, `crew.machine_folders` and
    /// `crew.machine_make_folder`, as the module says.
    pub(super) async fn machine_tool(
        &self,
        name: &str,
        b: &Bound,
        input: &Value,
    ) -> Result<Value, String> {
        let boss = b.service.agent_id.as_str();
        let plain = |e: MachineRefused| {
            match e {
            MachineRefused::Refused(why) if why.contains("is older") || why.contains("(404)") => {
                "That machine's Desk is older and can't answer this yet: the person updates Desk there.".to_string()
            }
            e => e.to_string(),
        }
        };
        match name {
            MACHINES => {
                let got = self
                    .machine_ask(ANY_MACHINE, boss, "desk.machines", json!({}))
                    .await
                    .map_err(plain)?;
                let listed = got
                    .get("machines")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new()));
                Ok(json!({
                    "machines": reported(&listed.to_string(), "the person's machines"),
                    "note": "Name a machine by its name here in crew.machine_folders and a plan ticket's machine.",
                }))
            }
            FOLDERS => {
                let machine = machine_of(input)?;
                let repo = input
                    .get("repo")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|r| !r.is_empty());
                let (call, body, what) = match repo {
                    Some(r) => {
                        let repo = workforce_desk::repos::repo_of(r).ok_or(
                            "Say the repository as host/owner/name, like github.com/driftwerk/vellev, or owner/name for GitHub.",
                        )?;
                        (
                            "desk.repos",
                            json!({ "remote": repo.key() }),
                            format!("{repo}'s checkouts on {machine}"),
                        )
                    }
                    None => (
                        "desk.folders",
                        json!({ "path": path_of(input, false)? }),
                        format!("folders on {machine}"),
                    ),
                };
                let got = self
                    .machine_ask(machine, boss, call, body)
                    .await
                    .map_err(plain)?;
                // Names on the machine's disk are its, not instructions.
                Ok(json!({
                    "machine": machine,
                    "found": reported(&got.to_string(), &what),
                }))
            }
            _ => {
                let o = &b.oversight;
                let who = super::name_of(&b.service);
                let reason = input
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .ok_or("Give the reason: the person reads it in Activity.")?;
                match o.level {
                    Level::Watch => {
                        return Err(format!(
                            "{who} only watches: tell the person which folder you'd make instead."
                        ))
                    }
                    Level::Suggest if !self.unbound(o) => {
                        return Err(
                            "At Suggest you don't make folders: ask the person (crew.ask) to make it, or to let you act."
                                .into(),
                        )
                    }
                    _ => {}
                }
                let machine = machine_of(input)?;
                let path = path_of(input, true)?.unwrap_or_default();
                let got = self
                    .machine_ask(machine, boss, "desk.folder_make", json!({ "path": path }))
                    .await;
                let made = got
                    .as_ref()
                    .ok()
                    .and_then(|g| g.get("path"))
                    .and_then(Value::as_str);
                let mut e = crew::Event::new(
                    Kind::OnMachine,
                    Party::manager(boss, &who),
                    None,
                    match (&got, made) {
                        (Ok(_), Some(at)) => format!("made the folder {at} on {machine}"),
                        (Ok(_), None) => format!("made the folder {path} on {machine}"),
                        (Err(e), _) => format!(
                            "couldn't make the folder {path} on {machine}: {}",
                            crew::one_line(&e.to_string(), 200)
                        ),
                    },
                );
                e.reason = Some(reason.to_string());
                self.desk.note(e);
                let got = got.map_err(plain)?;
                let made = got
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or(&path)
                    .to_string();
                Ok(json!({ "machine": machine, "made": reported(&made, "the folder made") }))
            }
        }
    }
}
