//! What an agent does on its own on this computer: for each kind of action
//! (commands, other machines, the browser, files, the Trash) the level the
//! person chose, and overrides for one folder, host or site. [`lift`] turns
//! the verdict's ask into a run where they cover the call, on the turns the
//! person or the agent's own jobs started. It never touches a refusal, and
//! never what stays the person's at every level ([`Why::held`]): a level
//! never adds an ask either.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use workforce_core::computer::{
    Access, ActionKind, Always, Asking, By, Choice, Effect, Level, Refusal, Refused, Scope,
    Verdict, Why,
};

use super::asks::{always_asks, Asks, Host, Place};
use super::policy::{browser_origin, is_browser_key, under, AgentPolicy, CallFacts};

/// An agent's levels and overrides. No level for a kind is "ask each time".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OnItsOwn {
    pub levels: BTreeMap<ActionKind, Level>,
    /// The kinds it does on its own in one folder, host or site.
    pub scoped: Vec<(ActionKind, Scope)>,
}

impl OnItsOwn {
    /// One stored `(kind, scope, level)` row as this hub reads it: `None`
    /// for one it can't, which then asks.
    pub fn row(kind: &str, scope: &str, level: &str) -> Option<(ActionKind, Scope, Level)> {
        match (
            ActionKind::parse(kind),
            Scope::parse(scope),
            Level::parse(level),
        ) {
            (Some(kind), Some(Scope::Whole), Some(level)) => Some((kind, Scope::Whole, level)),
            (Some(kind), Some(scope), Some(Level::Anywhere)) if scope.fits(kind) => {
                Some((kind, scope, Level::Anywhere))
            }
            _ => {
                tracing::warn!(kind, scope, level, "an on-its-own row was left out");
                None
            }
        }
    }

    /// From stored `(kind, scope, level)` rows ([`Self::row`]).
    pub fn from_rows<'a>(rows: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>) -> Self {
        rows.into_iter()
            .filter_map(|(kind, scope, level)| Self::row(kind, scope, level))
            .fold(Self::default(), |mut out, (kind, scope, level)| {
                match scope {
                    Scope::Whole => {
                        out.levels.insert(kind, level);
                    }
                    scope => out.scoped.push((kind, scope)),
                }
                out
            })
    }
}

/// What a call reaches, as a level or an override would cover it.
enum Targets {
    Paths(Vec<PathBuf>),
    Hosts(Vec<Host>),
    Site(Option<String>),
}

struct Reach {
    kind: ActionKind,
    targets: Targets,
    /// A delete a command makes: on its own, the Trash does it instead.
    by_command: bool,
}

/// One thing a call reaches.
enum Target<'a> {
    Path(&'a Path),
    Host(&'a Host),
    Site(Option<&'a str>),
}

impl Targets {
    fn each(&self) -> Vec<Target<'_>> {
        match self {
            Self::Paths(paths) => paths.iter().map(|p| Target::Path(p)).collect(),
            Self::Hosts(hosts) => hosts.iter().map(Target::Host).collect(),
            Self::Site(site) => vec![Target::Site(site.as_deref())],
        }
    }
}

fn reach(kind: ActionKind, targets: Targets) -> Vec<Reach> {
    vec![Reach {
        kind,
        targets,
        by_command: false,
    }]
}

/// What a command's always-asks class reaches, each part as its kind.
fn reaches_of(asks: Asks) -> Vec<Reach> {
    match asks {
        Asks::Deletes(targets) => vec![Reach {
            kind: ActionKind::Delete,
            targets: Targets::Paths(targets),
            by_command: true,
        }],
        Asks::Overwrites(targets) => reach(ActionKind::Files, Targets::Paths(targets)),
        Asks::Irreversible { at } => {
            reach(ActionKind::Commands, Targets::Paths(at.unwrap_or_default()))
        }
        Asks::Remote(hosts) => reach(ActionKind::Network, Targets::Hosts(hosts)),
        Asks::Many(parts) => parts.into_iter().flat_map(reaches_of).collect(),
        Asks::ForGood | Asks::Unclear | Asks::System => vec![],
    }
}

/// The kinds of action `call` is and what each reaches; none for what no
/// level covers.
fn reach_of(p: &AgentPolicy, call: &CallFacts) -> Vec<Reach> {
    match &call.effect {
        Effect::Exec { command, cwd } => {
            let place = Place::new(cwd.clone(), &p.own, &p.scratch, &p.home);
            always_asks(command, &place)
                .map(reaches_of)
                .unwrap_or_default()
        }
        Effect::Session { cwd } => reach(
            ActionKind::Commands,
            Targets::Paths(cwd.iter().cloned().collect()),
        ),
        Effect::Write { paths } => reach(ActionKind::Files, Targets::Paths(paths.clone())),
        Effect::Move { from, to } => reach(
            ActionKind::Files,
            Targets::Paths(from.iter().chain([to]).cloned().collect()),
        ),
        Effect::Trash { paths } => reach(ActionKind::Delete, Targets::Paths(paths.clone())),
        Effect::Connector { connector, .. } if is_browser_key(connector) => reach(
            ActionKind::Browser,
            Targets::Site(browser_origin(connector).map(str::to_string)),
        ),
        _ => vec![],
    }
}

/// Whether one override holds for `target`.
fn holds(scope: &Scope, target: &Target<'_>) -> bool {
    match (scope, target) {
        (Scope::Folder(folder), Target::Path(path)) => under(path, folder),
        (Scope::Host(host), Target::Host(Host::Named(named))) => host.eq_ignore_ascii_case(named),
        (Scope::Site(site), Target::Site(Some(origin))) => site == origin,
        _ => false,
    }
}

/// Whether what the agent does on its own covers everything `reach`
/// reaches: anywhere for its kind, inside its folders where that is its
/// level, or an override for each target. A call that names nothing is
/// covered only anywhere.
fn covers(p: &AgentPolicy, reach: &Reach) -> bool {
    let level = p.on_its_own.levels.get(&reach.kind).copied();
    if level == Some(Level::Anywhere) {
        return true;
    }
    let in_folders = level == Some(Level::InFolders) && reach.kind.takes_folders();
    let targets = reach.targets.each();
    !targets.is_empty()
        && targets.iter().all(|target| {
            (in_folders && matches!(target, Target::Path(path) if p.inside(path)))
                || p.on_its_own
                    .scoped
                    .iter()
                    .any(|(kind, scope)| *kind == reach.kind && holds(scope, target))
        })
}

/// The override a card may offer for `reach`: its one named host, its site,
/// or the call's folder where every target is in it and it may be offered.
fn offer(p: &AgentPolicy, call: &CallFacts, reach: &Reach) -> Option<Scope> {
    match &reach.targets {
        Targets::Hosts(hosts) => match hosts.as_slice() {
            [Host::Named(host)] => Some(Scope::Host(host.clone())),
            _ => None,
        },
        Targets::Site(site) => site.clone().map(Scope::Site),
        Targets::Paths(paths) => call
            .folder
            .as_ref()
            .filter(|folder| {
                !paths.is_empty()
                    && paths.iter().all(|path| under(path, folder))
                    && workforce_common::computer_access::works_in_refusal(
                        folder,
                        &p.home,
                        &p.withheld,
                    )
                    .is_none()
            })
            .map(|folder| Scope::Folder(folder.clone())),
    }
}

fn to_trash(p: &AgentPolicy, reaches: &[Reach]) -> Verdict {
    let named = reaches
        .iter()
        .filter(|r| r.by_command)
        .flat_map(|r| match &r.targets {
            Targets::Paths(paths) => paths.iter().map(|path| p.shown(path)).collect(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    Verdict::Refuse {
        refusal: Refusal {
            rule: Refused::UseTrash,
            sentence: format!(
                "{} moves things to the Trash on its own rather than deleting them for good, so the command didn't run. Use filesystem.trash on {named} instead.",
                p.name
            ),
        },
    }
}

/// `verdict`, with what the agent does on its own laid over it: an ask it
/// covers runs (a command's delete is sent to the Trash instead, where the
/// computer has one); one it doesn't may offer the override that would.
/// Only at Full access asking before irreversible things, on the person's
/// own work. Using the person's own Chrome is never offered.
pub fn lift(p: &AgentPolicy, call: &CallFacts, verdict: Verdict) -> Verdict {
    let applies =
        call.own_work && p.access.access == Access::Full && p.access.asks == Asking::Irreversible;
    let (mut choices, why) = match verdict {
        Verdict::Ask { choices, why } if applies && !why.held() => (choices, why),
        other => return other,
    };
    let reaches = reach_of(p, call);
    let deletes = reaches.iter().any(|r| r.by_command);
    if reaches.is_empty() || (deletes && !p.trash) {
        return Verdict::Ask { choices, why };
    }
    if reaches.iter().all(|r| covers(p, r)) {
        return match deletes {
            true => to_trash(p, &reaches),
            false => Verdict::Run { by: By::OnItsOwn },
        };
    }
    let lasting = choices.iter().any(|c| matches!(c, Choice::Always(_)));
    let offered = match reaches.as_slice() {
        [one] if !lasting && !one.by_command && why != Why::TheirChrome => {
            offer(p, call, one).map(|scope| (one.kind, scope))
        }
        _ => None,
    };
    if let Some((kind, scope)) = offered {
        let at = choices
            .iter()
            .position(|c| *c == Choice::Deny)
            .unwrap_or(choices.len());
        choices.insert(at, Choice::Always(Always::OnItsOwn { kind, scope }));
    }
    Verdict::Ask { choices, why }
}
