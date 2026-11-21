//! What an agent does on its own on this computer: for each kind of action
//! (commands, other machines, the browser, files, the Trash) the level the
//! person chose, and overrides for one folder, host or site. [`lift`] turns
//! the verdict's ask into a run where they cover the call, on the turns the
//! person or the agent's own jobs started. It never touches a refusal, and
//! never what stays the person's at every level ([`HELD`]): a level never
//! adds an ask either.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use workforce_core::computer::{
    Access, Always, Asking, By, Choice, Effect, Kind, Level, Refusal, Refused, Scope, Verdict, Why,
};

use super::asks::{always_asks, hosts, Asks, Host, Place};
use super::policy::{browser_origin, is_browser_key, under, AgentPolicy, CallFacts};

/// An agent's levels and overrides. No level for a kind is "ask each time".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OnItsOwn {
    pub levels: BTreeMap<Kind, Level>,
    /// The kinds it does on its own in one folder, host or site.
    pub scoped: Vec<(Kind, Scope)>,
}

impl OnItsOwn {
    /// From stored `(kind, scope, level)` rows; one this hub can't read is
    /// left out, and so asks.
    pub fn from_rows<'a>(rows: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>) -> Self {
        rows.into_iter()
            .fold(Self::default(), |mut out, (kind, scope, level)| {
                match (Kind::parse(kind), Scope::parse(scope), Level::parse(level)) {
                    (Some(kind), Some(Scope::Kind), Some(level)) => {
                        out.levels.insert(kind, level);
                    }
                    (Some(kind), Some(scope), Some(Level::Anywhere)) if scope.fits(kind) => {
                        out.scoped.push((kind, scope));
                    }
                    _ => tracing::warn!(kind, scope, level, "an on-its-own row was left out"),
                }
                out
            })
    }
}

/// Why an ask stays the person's whatever the levels say: a page that read
/// as hostile, the person's own sign-in, what reaches other people or a
/// connected service, what a tool can't name, and the computer's own
/// settings and security.
pub const HELD: [Why; 6] = [
    Why::HostilePage,
    Why::SignIn,
    Why::SendsToPeople,
    Why::ConnectorWrite,
    Why::Unknown,
    Why::System,
];

/// What a call reaches, as a level or an override would cover it.
enum Targets {
    Paths(Vec<PathBuf>),
    Hosts(Vec<Host>),
    Site(Option<String>),
}

struct Reach {
    kind: Kind,
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

fn reach(kind: Kind, targets: Targets) -> Option<Reach> {
    Some(Reach {
        kind,
        targets,
        by_command: false,
    })
}

/// The kind of action `call` is and what it reaches; `None` for what no
/// level covers.
fn reach_of(p: &AgentPolicy, call: &CallFacts) -> Option<Reach> {
    match &call.effect {
        Effect::Exec { command, cwd } => {
            let place = Place::new(cwd.clone(), &p.own, &p.scratch, &p.home);
            let remote = hosts(command);
            // What it does here and can be read (a delete, a force push)
            // is decided as that; only what can't be read is the other
            // machine's when it reaches one.
            match always_asks(command, &place) {
                Some(Asks::System) => None,
                Some(Asks::Deletes(targets)) => Some(Reach {
                    kind: Kind::Delete,
                    targets: Targets::Paths(targets),
                    by_command: true,
                }),
                Some(Asks::Unclear) | None if !remote.is_empty() => {
                    reach(Kind::Network, Targets::Hosts(remote))
                }
                _ => reach(
                    Kind::Commands,
                    Targets::Paths(cwd.iter().cloned().collect()),
                ),
            }
        }
        Effect::Session { cwd } => reach(
            Kind::Commands,
            Targets::Paths(cwd.iter().cloned().collect()),
        ),
        Effect::Write { paths } => reach(Kind::Files, Targets::Paths(paths.clone())),
        Effect::Move { from, to } => reach(
            Kind::Files,
            Targets::Paths(from.iter().chain([to]).cloned().collect()),
        ),
        Effect::Trash { paths } => reach(Kind::Delete, Targets::Paths(paths.clone())),
        Effect::Connector { connector, .. } if is_browser_key(connector) => reach(
            Kind::Browser,
            Targets::Site(browser_origin(connector).map(str::to_string)),
        ),
        _ => None,
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
                    && p.offerable(folder)
            })
            .map(|folder| Scope::Folder(folder.clone())),
    }
}

fn to_trash(p: &AgentPolicy, reach: &Reach) -> Verdict {
    let named = match &reach.targets {
        Targets::Paths(paths) => paths
            .iter()
            .map(|path| p.shown(path))
            .collect::<Vec<_>>()
            .join(", "),
        _ => String::new(),
    };
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
/// covers runs (a command's delete is sent to the Trash instead); one it
/// doesn't may offer the override that would. Only at Full access asking
/// before irreversible things, on the person's own work.
pub fn lift(p: &AgentPolicy, call: &CallFacts, verdict: Verdict) -> Verdict {
    let applies =
        call.own_work && p.access.access == Access::Full && p.access.asks == Asking::Irreversible;
    let (mut choices, why) = match verdict {
        Verdict::Ask { choices, why } if applies && !HELD.contains(&why) => (choices, why),
        other => return other,
    };
    let Some(reach) = reach_of(p, call) else {
        return Verdict::Ask { choices, why };
    };
    if covers(p, &reach) {
        return match reach.by_command {
            true => to_trash(p, &reach),
            false => Verdict::Run { by: By::OnItsOwn },
        };
    }
    let lasting = choices.iter().any(|c| matches!(c, Choice::Always(_)));
    if let Some(scope) = (!lasting && !reach.by_command)
        .then(|| offer(p, call, &reach))
        .flatten()
    {
        let at = choices
            .iter()
            .position(|c| *c == Choice::Deny)
            .unwrap_or(choices.len());
        choices.insert(
            at,
            Choice::Always(Always::OnItsOwn {
                kind: reach.kind,
                scope,
            }),
        );
    }
    Verdict::Ask { choices, why }
}
