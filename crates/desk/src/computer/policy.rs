//! What an agent may do on this computer: one pure function, [`verdict`],
//! that both callers use. The local hub's dispatch gate calls it for its
//! own agents; Desk's backend calls it for a remote agent's `computer.*`.
//!
//! The rules, checked in order:
//! 1. Access is stopped: refuse.
//! 2. A path is in the withheld set: refuse.
//! 3. The call is outside the agent's level: refuse, in a plain sentence
//!    naming the level and Settings.
//! 4. A connector that is off, or read only and given a write: refuse.
//! 5. The call is on the always-asks list (it can't be undone, or can't be
//!    read as simple commands): ask, offering Allow once and Deny. An agent
//!    at Full access that asks never skips this.
//! 6. Anything at Full access; a read, a change inside the own folder, or a
//!    contained command at a lower level: run.
//! 7. A grant or a session allowance covers it: run.
//! 8. The agent works on its own: the check reviews it (not reached while
//!    Full access runs everything past rule 5; kept for AC11).
//! 9. Otherwise: ask, offering Allow once, for this session, always in the
//!    folder, and Deny.
//!
//! A call with no effect here (`memory.*`, `web.*`, …) runs before any of
//! them. The kill switch stops what touches this computer, not connectors.
//!
//! Paths are canonical by the time they reach here: the caller resolves
//! them, finds the folder a grant would cover, and drops grants that have
//! expired. Nothing here reads the filesystem or the clock.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use workforce_core::computer::{
    Ability, Access, Always, Asking, By, Choice, ComputerAccess, Effect, Refusal, Refused, Verdict,
    Why,
};

use super::asks::{always_asks, Asks, Place};

/// What a namespace's tools touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    /// Files, commands and sessions on this computer.
    Computer,
    /// A connected service, through its adapter.
    Connector,
}

/// Every namespace whose tools the verdict decides. Every other namespace
/// (`memory`, `web`, `artifact`, `messaging`, `knowledge`, …) touches
/// nothing here and never asks.
pub const NAMESPACES: &[(&str, Namespace)] = &[
    ("filesystem", Namespace::Computer),
    ("code", Namespace::Computer),
    ("exec", Namespace::Computer),
    ("desk", Namespace::Computer),
    ("coding", Namespace::Computer),
    ("claude_code", Namespace::Computer),
    ("wasm_skill", Namespace::Computer),
    ("attachments", Namespace::Computer),
    // Every built-in adapter's `tool_prefix`.
    ("github", Namespace::Connector),
    ("google", Namespace::Connector),
    ("linear", Namespace::Connector),
    ("jira", Namespace::Connector),
    ("datadog", Namespace::Connector),
    ("metabase", Namespace::Connector),
    ("xero", Namespace::Connector),
    ("spotify", Namespace::Connector),
    ("freeagent", Namespace::Connector),
    ("postmark", Namespace::Connector),
    ("sendgrid", Namespace::Connector),
    ("hubspot", Namespace::Connector),
    ("notion", Namespace::Connector),
    ("browser", Namespace::Connector),
    ("obscura", Namespace::Connector),
    ("email", Namespace::Connector),
    ("slack", Namespace::Connector),
];

/// The namespace `tool` (`filesystem.read`) is in, if the verdict decides it.
pub fn namespace(tool: &str) -> Option<Namespace> {
    let prefix = tool.split_once('.').map_or(tool, |(p, _)| p);
    NAMESPACES
        .iter()
        .find(|(name, _)| *name == prefix)
        .map(|(_, ns)| *ns)
}

/// Whether a connector tool sends to other people: on the always-asks list.
pub fn sends_to_people(tool: &str) -> bool {
    match tool.split_once('.') {
        Some(("slack", name)) => {
            name.contains("post") || name == "send" || name.starts_with("send_")
        }
        Some(("email", name)) => matches!(name, "send" | "forward"),
        // `google.gmail.send`, `google.gmail.draft_send`, `google.chat.dm_send`.
        Some(("google", name)) => name
            .rsplit('.')
            .next()
            .is_some_and(|n| n == "send" || n.ends_with("_send") || n == "forward"),
        Some(("postmark" | "sendgrid", name)) => name.starts_with("email_send"),
        _ => false,
    }
}

/// An agent's `[skills]` binding, where it has one: only `enabled`
/// connectors reach it, and `read_only` ones take no writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connectors {
    pub enabled: Vec<String>,
    pub read_only: Vec<String>,
}

/// Everything the verdict knows about the agent.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentPolicy {
    /// `local`, or the remote workspace a `computer.*` call came from.
    pub hub: String,
    pub agent: String,
    /// What the person calls it ("Allan").
    pub name: String,
    pub access: ComputerAccess,
    /// Its workspace and project work trees, canonical.
    pub own: Vec<PathBuf>,
    /// What no agent reaches at any level: Desk's data (other agents'
    /// folders and their config files with it), the keychains. The own
    /// folder is carved out of it: it may lie inside Desk's data.
    pub withheld: Vec<PathBuf>,
    /// Its own config files and folders, inside its own folder: never
    /// changed at any level. Reading them stays open, as before.
    pub config: Vec<PathBuf>,
    pub home: PathBuf,
    /// Whether this computer can contain a command to the level (AC5).
    pub contained: bool,
    /// `None` keeps today's behaviour: every connector is on.
    pub connectors: Option<Connectors>,
}

/// The job or workflow run a call is made in, for a grant held by one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOf {
    pub kind: String,
    pub id: String,
}

/// One call, as the gate computed it.
#[derive(Debug, Clone, PartialEq)]
pub struct CallFacts {
    pub tool: String,
    pub effect: Effect,
    /// Whose turn it is.
    pub person: Option<String>,
    pub session_id: Option<String>,
    pub run: Option<RunOf>,
    /// The folder an "always" answer would cover: the git work tree holding
    /// the target, or its parent; `None` when the caller found none.
    pub folder: Option<PathBuf>,
}

/// A standing grant, or a session allowance when `session_id` is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub id: Uuid,
    pub hub: String,
    pub agent: String,
    pub person: String,
    pub ability: Ability,
    pub folder: Option<PathBuf>,
    pub connector: Option<String>,
    pub session_id: Option<String>,
    pub run: Option<RunOf>,
}

/// One thing a call changes, for the grant that would cover it.
enum Change<'a> {
    Path(Ability, &'a Path),
    Connector(&'a str),
    /// Nothing a grant can name.
    Unnamed,
}

fn refuse(rule: Refused, sentence: String) -> Verdict {
    Verdict::Refuse {
        refusal: Refusal { rule, sentence },
    }
}

fn ask(choices: Vec<Choice>, why: Why) -> Verdict {
    Verdict::Ask { choices, why }
}

/// `path` as the person reads it: under their home, with `~`.
pub fn shown(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

impl AgentPolicy {
    fn inside(&self, path: &Path) -> bool {
        self.own.iter().any(|root| path.starts_with(root))
    }

    /// Whether `path` is kept from the agent; `change` when the call would
    /// change it.
    fn withheld(&self, path: &Path, change: bool) -> bool {
        (change && self.config.iter().any(|c| path.starts_with(c)))
            || (self.withheld.iter().any(|w| path.starts_with(w)) && !self.inside(path))
    }

    fn shown(&self, path: &Path) -> String {
        shown(path, &self.home)
    }

    fn level(&self) -> &'static str {
        self.access.access.label()
    }

    fn connector(&self, id: &str) -> (bool, bool) {
        match &self.connectors {
            None => (true, false),
            Some(c) => (
                c.enabled.iter().any(|e| e == id),
                c.read_only.iter().any(|e| e == id),
            ),
        }
    }

    /// Whether `folder` may be offered as an "always" scope: never home, the
    /// root, a volume root, or anything above home.
    fn offerable(&self, folder: &Path) -> bool {
        let depth = folder
            .components()
            .filter(|c| matches!(c, std::path::Component::Normal(_)))
            .count();
        let volume_root = folder.starts_with("/Volumes") && depth <= 2;
        depth > 0 && !volume_root && !self.home.starts_with(folder) && !self.withheld(folder, true)
    }
}

/// Every path a call names, for the withheld check.
fn paths(effect: &Effect) -> Vec<&Path> {
    match effect {
        Effect::Read { paths } | Effect::Write { paths } | Effect::Delete { paths } => {
            paths.iter().map(PathBuf::as_path).collect()
        }
        Effect::Move { from, to } => from
            .iter()
            .map(PathBuf::as_path)
            .chain([to.as_path()])
            .collect(),
        Effect::Exec { cwd, .. } | Effect::Session { cwd } => {
            cwd.iter().map(PathBuf::as_path).collect()
        }
        Effect::None | Effect::Connector { .. } | Effect::Unknown => vec![],
    }
}

/// What a call changes, each as a grant would name it; empty for a read.
fn changes(effect: &Effect) -> Vec<Change<'_>> {
    fn each(ability: Ability, paths: &[PathBuf]) -> Vec<Change<'_>> {
        paths
            .iter()
            .map(|p| Change::Path(ability, p.as_path()))
            .collect()
    }
    match effect {
        Effect::Write { paths } => each(Ability::Write, paths),
        Effect::Delete { paths } => each(Ability::Delete, paths),
        Effect::Move { from, to } => {
            let mut out = each(Ability::Delete, from);
            out.push(Change::Path(Ability::Write, to));
            out
        }
        Effect::Exec { cwd, .. } => vec![cwd
            .as_deref()
            .map_or(Change::Unnamed, |c| Change::Path(Ability::Run, c))],
        Effect::Session { cwd } => vec![cwd
            .as_deref()
            .map_or(Change::Unnamed, |c| Change::Path(Ability::Session, c))],
        Effect::Connector {
            connector,
            write: true,
        } => vec![Change::Connector(connector)],
        Effect::Unknown => vec![Change::Unnamed],
        Effect::None | Effect::Read { .. } | Effect::Connector { write: false, .. } => vec![],
    }
}

/// What lets this call run without asking, if anything: a standing grant
/// first, then a session allowance. Every change must be covered.
fn granted(p: &AgentPolicy, grants: &[Grant], call: &CallFacts) -> Option<By> {
    let changes = changes(&call.effect);
    if changes.is_empty() {
        return None;
    }
    let applies = |g: &&Grant| {
        g.hub == p.hub
            && g.agent == p.agent
            && call.person.as_deref() == Some(g.person.as_str())
            && g.run
                .as_ref()
                .is_none_or(|run| call.run.as_ref() == Some(run))
            && g.session_id
                .as_ref()
                .is_none_or(|s| call.session_id.as_ref() == Some(s))
    };
    let covers = |g: &Grant, change: &Change<'_>| match change {
        Change::Path(ability, path) => {
            g.ability == *ability && g.folder.as_deref().is_some_and(|f| path.starts_with(f))
        }
        Change::Connector(id) => {
            g.ability == Ability::ConnectorWrite && g.connector.as_deref() == Some(*id)
        }
        Change::Unnamed => false,
    };
    let mut by: Option<By> = None;
    for change in &changes {
        // A file change inside the own folder needs no grant (rule 6).
        if let Change::Path(Ability::Write | Ability::Delete, path) = change {
            if p.inside(path) && !p.access.ask_in_own_folder {
                continue;
            }
        }
        let mut found = grants.iter().filter(applies).filter(|g| covers(g, change));
        let standing = found.clone().find(|g| g.session_id.is_none());
        let pick = standing.or_else(|| found.next())?;
        let this = if pick.session_id.is_some() {
            By::Session(pick.id)
        } else {
            By::Grant(pick.id)
        };
        // A session allowance anywhere makes the whole call the session's.
        by = Some(match (by, this) {
            (Some(By::Session(id)), _) => By::Session(id),
            (_, this) => this,
        });
    }
    by
}

/// What happens to `call`, given the kill switch and the agent's live
/// grants. See the module for the rules.
pub fn verdict(p: &AgentPolicy, stopped: bool, grants: &[Grant], call: &CallFacts) -> Verdict {
    let name = &p.name;
    let settings = "Settings → This computer";
    let access = p.access.access;
    let lower = access != Access::Full;

    // A call that touches nothing here.
    if call.effect == Effect::None {
        return Verdict::Run { by: By::Rule };
    }
    let connector = match &call.effect {
        Effect::Connector { connector, write } => Some((connector.as_str(), *write)),
        _ => None,
    };

    // 1. The kill switch.
    if stopped && connector.is_none() {
        return refuse(
            Refused::Stopped,
            format!(
                "Access to this computer is stopped, so nothing was read, changed or run. Allow it again in {settings}."
            ),
        );
    }

    // 2. The withheld set.
    let change = matches!(
        call.effect,
        Effect::Write { .. } | Effect::Delete { .. } | Effect::Move { .. }
    );
    if let Some(path) = paths(&call.effect)
        .into_iter()
        .find(|p_| p.withheld(p_, change))
    {
        return refuse(
            Refused::Withheld,
            format!(
                "{name} can't use {}: Desk keeps it from every agent.",
                p.shown(path)
            ),
        );
    }

    // 3. The level.
    let outside_level = |what: &str, path: Option<&Path>| {
        let target = path.map_or_else(String::new, |path| format!(" {}", p.shown(path)));
        refuse(
            Refused::OutsideLevel,
            format!(
                "{name} can only {what} its own folder at {}. To let it{}, change its access in {settings}.",
                p.level(),
                match what {
                    "read" => format!(" read{target}"),
                    _ => format!(" change{target}"),
                }
            ),
        )
    };
    match &call.effect {
        Effect::Read { paths } if access == Access::OwnFolder => {
            if let Some(path) = paths.iter().find(|path| !p.inside(path)) {
                return outside_level("read", Some(path));
            }
        }
        Effect::Write { .. } | Effect::Delete { .. } | Effect::Move { .. } if lower => {
            if let Some(path) = paths(&call.effect).into_iter().find(|path| !p.inside(path)) {
                return outside_level("change", Some(path));
            }
        }
        Effect::Unknown if lower => {
            return refuse(
                Refused::OutsideLevel,
                format!(
                    "{name} can only change its own folder at {}, and this tool can't say what it changes. To let it, change its access in {settings}.",
                    p.level()
                ),
            );
        }
        Effect::Exec { cwd, .. } | Effect::Session { cwd } if lower => {
            let what = if matches!(call.effect, Effect::Exec { .. }) {
                "run commands"
            } else {
                "start sessions"
            };
            if !p.contained {
                return refuse(
                    Refused::OutsideLevel,
                    format!(
                        "{name} can't {what} at {} on this computer. Choose Full access in {settings} to let it.",
                        p.level()
                    ),
                );
            }
            let must_be_inside =
                access == Access::OwnFolder || matches!(call.effect, Effect::Session { .. });
            if must_be_inside && !cwd.as_deref().is_some_and(|c| p.inside(c)) {
                return refuse(
                    Refused::OutsideLevel,
                    format!(
                        "{name} can only {what} in its own folder at {}. To let it go further, change its access in {settings}.",
                        p.level()
                    ),
                );
            }
        }
        _ => {}
    }

    // 4. The connector's own switch.
    if let Some((id, write)) = connector {
        let (enabled, read_only) = p.connector(id);
        if !enabled {
            return refuse(
                Refused::ConnectorOff,
                format!("{id} is off for {name}. Turn it on in {name}'s Connectors."),
            );
        }
        if write && read_only {
            return refuse(
                Refused::ConnectorReadOnly,
                format!("{name} can only read {id}. To let it make changes, allow writes in {name}'s Connectors."),
            );
        }
    }

    // 5. The always-asks list, unless the agent at Full access asks never.
    let once = || vec![Choice::AllowOnce, Choice::Deny];
    let never = access == Access::Full && p.access.asks == Asking::Never;
    match &call.effect {
        _ if never => {}
        Effect::Delete { paths } | Effect::Move { from: paths, .. }
            if paths.iter().any(|path| !p.inside(path)) =>
        {
            return ask(once(), Why::CantBeUndone);
        }
        Effect::Exec { command, cwd } => {
            let place = Place {
                cwd: cwd.as_deref(),
                own: &p.own,
                home: &p.home,
            };
            match always_asks(command, &place) {
                Some(Asks::Irreversible) => return ask(once(), Why::CantBeUndone),
                Some(Asks::Unclear) => return ask(once(), Why::Unclear),
                None => {}
            }
        }
        Effect::Connector { write: true, .. } if sends_to_people(&call.tool) => {
            return ask(once(), Why::SendsToPeople);
        }
        _ => {}
    }

    // 6. What the level allows as it is.
    // A change that names no path is not known to be inside.
    let inside = || {
        let paths = paths(&call.effect);
        !paths.is_empty() && paths.iter().all(|path| p.inside(path))
    };
    let changes_inside = || inside() && !p.access.ask_in_own_folder;
    let runs = match &call.effect {
        // Full access: whatever passed the always-asks list runs.
        _ if access == Access::Full => true,
        Effect::Read { .. } | Effect::Connector { write: false, .. } => true,
        Effect::Write { .. } | Effect::Delete { .. } | Effect::Move { .. } => changes_inside(),
        // Contained to the level (checked in 3).
        Effect::Exec { .. } | Effect::Session { .. } => lower,
        _ => false,
    };
    if runs {
        return Verdict::Run { by: By::Rule };
    }

    // 7. A grant or a session allowance.
    if let Some(by) = granted(p, grants, call) {
        return Verdict::Run { by };
    }

    // 8. Working on its own.
    if p.access.autonomous && access == Access::Full {
        return Verdict::Review;
    }

    // 9. Ask.
    let why = match &call.effect {
        Effect::Exec { .. } | Effect::Session { .. } => Why::Runs,
        Effect::Connector { .. } => Why::ConnectorWrite,
        Effect::Unknown => Why::Unknown,
        _ if inside() => Why::InOwnFolder,
        _ => Why::OutsideFolder,
    };
    let mut choices = vec![Choice::AllowOnce];
    if call.session_id.is_some() {
        choices.push(Choice::AllowSession);
    }
    let always = match (&call.effect, connector) {
        (_, Some((id, _))) => Some(Always::Connector(id.to_string())),
        (Effect::Unknown, _) => None,
        _ => call
            .folder
            .as_ref()
            .filter(|folder| {
                let targets = paths(&call.effect);
                !targets.is_empty()
                    && targets.iter().all(|t| t.starts_with(folder))
                    && p.offerable(folder)
            })
            .map(|folder| Always::Folder(folder.clone())),
    };
    if let Some(always) = always {
        choices.push(Choice::Always(always));
    }
    choices.push(Choice::Deny);
    ask(choices, why)
}
