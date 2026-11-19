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

pub const MACHINES: &str = "crew.machines";
pub const FOLDERS: &str = "crew.machine_folders";
pub const MAKE_FOLDER: &str = "crew.machine_make_folder";

/// What a boss's machine tools ask of a machine through the relay, each
/// carried to its own route there; never one of [`super::relay::DESK_CALLS`].
pub const MACHINE_CALLS: [&str; 4] = [
    "desk.machines",
    "desk.folders",
    "desk.folder_make",
    "desk.repos",
];

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

fn machine_of(input: &Value) -> Result<&str, String> {
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

impl CrewHub {
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
            MachineRefused::Refused(why) if why.contains("404") || why.contains("not found") => {
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
                Ok(json!({
                    "machines": got.get("machines").cloned().unwrap_or(Value::Array(Vec::new())),
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
                Ok(
                    json!({ "machine": machine, "made": got.get("path").cloned().unwrap_or(json!(path)) }),
                )
            }
        }
    }
}
