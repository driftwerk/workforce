//! What an agent knows of the work in Desk ("What they see", the Sees
//! design `designs/agent-sees.md` in workforce-desk): one rule per agent,
//! kept on this hub and never on the agent's own, naming the places whose
//! work is shared with it, read-only. Pure: the grammar, what a rule covers
//! of the work as it stands ([`resolve`]), and what an agent already holds
//! that a narrower rule no longer covers ([`held_outside`]).
//!
//! It is not a boss's scope ([`crate::scope`]) and not a remote boss's
//! `Sees` ([`crate::grant::Sees`]): it lets no one act, and Everything is
//! everything, a group with a boss of its own included, because the person
//! chose it for this agent.
//!
//! The grammar is a preference's ([`crate::preference::PrefScope`]: `all`,
//! `group:<id>`) with the sidebar's own item keys added: `session:<id>`,
//! `session:<machine>/<id>`, `agent:<hub>/<agent>`, and `board:<id>`,
//! `grid:<id>`. A rule is a list of them; an empty one is the agent's own
//! work alone.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::text_enum;
use crate::service::Layout;

/// The most places one rule may name.
pub const MAX_PLACES: usize = 64;
/// The longest id a place may carry.
const MAX_ID: usize = 200;

text_enum!(Digest {
    // Nothing is added to the agent's turns.
    Off => "off",
    // A short account of the work now, in its turns with the person.
    Live => "live",
    // The same, made once a day.
    Daily => "daily",
});

// `text_enum!` makes the enum, so its default can't be derived.
#[allow(clippy::derivable_impls)]
impl Default for Digest {
    fn default() -> Self {
        Self::Off
    }
}

/// One place a rule names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Place {
    /// Every session, board, decision and need.
    All,
    /// A sidebar group's sessions as they are now, and its boards.
    Group(String),
    /// A board's tickets, and the sessions its tickets are assigned to.
    Board(Uuid),
    /// The sessions in a grid's panes.
    Grid(String),
    /// One session: this computer's (`<id>`) or a machine's
    /// (`<machine>/<id>`).
    Session { machine: Option<String>, id: Uuid },
    /// The sessions an agent runs, and its decisions.
    Agent { hub: String, agent: String },
}

fn name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= MAX_ID
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        // Never a path's `.` or `..`.
        && !n.chars().all(|c| c == '.')
}

fn id_ok(n: &str) -> bool {
    !n.is_empty() && n.len() <= MAX_ID && !n.chars().any(|c| c.is_control() || c.is_whitespace())
}

impl Place {
    /// `all`, `group:<id>`, `board:<id>`, `grid:<id>`, `session:<id>`,
    /// `session:<machine>/<id>` or `agent:<hub>/<agent>`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let t = text.trim();
        if t.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }
        let Some((kind, rest)) = t.split_once(':') else {
            return Err(format!(
                "{t:?} isn't a place: say all, group:<id>, board:<id>, grid:<id>, session:<id> or agent:<hub>/<agent>."
            ));
        };
        let rest = rest.trim();
        match kind {
            "group" if id_ok(rest) => Ok(Self::Group(rest.to_string())),
            "group" => Err("A group names its id: group:<id>.".into()),
            "board" => rest
                .parse()
                .map(Self::Board)
                .map_err(|_| "A board names its id: board:<id>.".into()),
            "grid" if id_ok(rest) => Ok(Self::Grid(rest.to_string())),
            "grid" => Err("A grid names its id: grid:<id>.".into()),
            "session" => {
                let bad = || {
                    "A session names its id: session:<id> or session:<machine>/<id>.".to_string()
                };
                match rest.split_once('/') {
                    Some((m, id)) if name_ok(m) => Ok(Self::Session {
                        machine: Some(m.to_string()),
                        id: id.parse().map_err(|_| bad())?,
                    }),
                    Some(_) => Err(bad()),
                    None => Ok(Self::Session {
                        machine: None,
                        id: rest.parse().map_err(|_| bad())?,
                    }),
                }
            }
            "agent" => match rest.split_once('/') {
                Some((hub, agent)) if name_ok(hub) && name_ok(agent) => Ok(Self::Agent {
                    hub: hub.to_string(),
                    agent: agent.to_string(),
                }),
                _ => Err("An agent names its hub and itself: agent:<hub>/<agent>.".into()),
            },
            _ => Err(format!(
                "{kind:?} isn't a kind of place: say group, board, grid, session or agent."
            )),
        }
    }

    /// As it is kept and given back.
    pub fn text(&self) -> String {
        match self {
            Self::All => "all".into(),
            Self::Group(g) => format!("group:{g}"),
            Self::Board(b) => format!("board:{b}"),
            Self::Grid(g) => format!("grid:{g}"),
            Self::Session { machine: None, id } => format!("session:{id}"),
            Self::Session {
                machine: Some(m),
                id,
            } => format!("session:{m}/{id}"),
            Self::Agent { hub, agent } => format!("agent:{hub}/{agent}"),
        }
    }
}

/// What an agent knows of the work in Desk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rule {
    /// The places whose work it is told of. Empty: only its own work.
    /// `All` is never beside another place.
    pub places: Vec<Place>,
    /// Whether copies of each covered session's conversation are shared.
    pub transcripts: bool,
    pub digest: Digest,
}

impl Rule {
    /// A rule from the places as written: each parsed, the same place once,
    /// and `all` alone when it is among them.
    pub fn new(places: &[String], transcripts: bool, digest: Digest) -> Result<Self, String> {
        if places.len() > MAX_PLACES {
            return Err(format!("A rule names at most {MAX_PLACES} places."));
        }
        let parsed = places
            .iter()
            .map(|p| Place::parse(p))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let places = if parsed.contains(&Place::All) {
            vec![Place::All]
        } else {
            parsed.into_iter().collect()
        };
        Ok(Self {
            places,
            transcripts,
            digest,
        })
    }

    pub fn everything(&self) -> bool {
        self.places.first() == Some(&Place::All)
    }

    /// The places as they are kept.
    pub fn texts(&self) -> Vec<String> {
        self.places.iter().map(Place::text).collect()
    }
}

/// Whether `key` is an agent's member key, `<hub>/<agent>` (`local` for
/// this hub's own), as rules are kept by.
pub fn agent_key_ok(key: &str) -> bool {
    matches!(key.split_once('/'), Some((hub, agent)) if name_ok(hub) && name_ok(agent))
}

/// The id a boss's charges name the agent `key` by: a local agent by its
/// own id, an agent on another hub as a served boss (`<hub>/<agent>`).
pub fn boss_id_of(key: &str) -> String {
    match key.split_once('/') {
        Some((crate::rooms::LOCAL_HUB, agent)) => agent.to_string(),
        _ => key.to_string(),
    }
}

/// A board, as a rule reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardFacts {
    pub id: Uuid,
    pub group: Option<String>,
    /// The sessions its tickets are assigned to now.
    pub sessions: Vec<Uuid>,
}

/// A grid, as a rule reads it: the sessions in its panes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridFacts {
    pub id: String,
    pub sessions: Vec<Uuid>,
    /// Its panes' sessions on machines: (machine, session).
    pub machines: Vec<(String, Uuid)>,
}

/// The sessions in the panes of grid `id`, from the value the page keeps
/// under `grid.<id>` (`src/grids/model.ts` in workforce-desk): `{id, name,
/// panes: [{content: {kind: "session", sessionId, machine?}}]}`. The grid
/// is the key's, whatever id its value names. A machine's session is kept
/// apart, for that machine to share (Sees K7); a value that isn't a grid
/// holds none.
pub fn grid_facts(id: &str, value: &serde_json::Value) -> Option<GridFacts> {
    let id = id.to_string();
    let panes = value.get("panes")?.as_array()?;
    let mut sessions = Vec::new();
    let mut machines = Vec::new();
    for c in panes.iter().filter_map(|p| p.get("content")) {
        if c.get("kind").and_then(|k| k.as_str()) != Some("session") {
            continue;
        }
        let Some(session) = c.get("sessionId").and_then(|s| s.as_str()?.parse().ok()) else {
            continue;
        };
        match c.get("machine").and_then(|m| m.as_str()) {
            Some(m) if name_ok(m) => machines.push((m.to_string(), session)),
            Some(_) => {}
            None => sessions.push(session),
        }
    }
    Some(GridFacts {
        id,
        sessions,
        machines,
    })
}

/// The work as it stands, for [`resolve`].
#[derive(Debug, Clone, Default)]
pub struct World {
    pub layout: Layout,
    pub boards: Vec<BoardFacts>,
    pub grids: Vec<GridFacts>,
    /// Who runs which session now: (boss id as charges name it, session).
    pub runs: Vec<(String, Uuid)>,
}

/// What a rule covers of the work now. A machine's sessions are never in
/// it yet (Sees K7).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Covered {
    pub everything: bool,
    pub sessions: HashSet<Uuid>,
    pub groups: HashSet<String>,
    pub boards: HashSet<Uuid>,
    /// The bosses whose decisions are covered, as charges name them.
    pub bosses: HashSet<String>,
}

impl Covered {
    /// Whether a row about `session` (if any), of `group`, `board` and by
    /// `boss` (if any), is covered.
    pub fn covers(
        &self,
        session: Option<Uuid>,
        group: Option<&str>,
        board: Option<Uuid>,
        boss: Option<&str>,
    ) -> bool {
        if self.everything {
            return true;
        }
        if let Some(s) = session {
            return self.sessions.contains(&s);
        }
        board.is_some_and(|b| self.boards.contains(&b))
            || group.is_some_and(|g| self.groups.contains(g))
            || boss.is_some_and(|b| self.bosses.contains(b))
    }
}

/// This computer's session a sidebar item names.
fn session_of(item: &str) -> Option<Uuid> {
    item.strip_prefix("session:")?.parse().ok()
}

/// A machine's session in a sidebar item: `session:<machine>/<id>`.
fn machine_session_of(item: &str) -> Option<(String, Uuid)> {
    let (m, id) = item.strip_prefix("session:")?.split_once('/')?;
    name_ok(m)
        .then(|| Some((m.to_string(), id.parse().ok()?)))
        .flatten()
}

/// What a rule covers of the machines' sessions, which each machine's
/// engine shares itself (Sees K7): every session of every machine under
/// Everything; else, by machine, the sessions of a chosen group or grid
/// that are on it and the ones chosen by name. Boards and agents reach a
/// machine's sessions through its own engine's rule alone: the Mac's
/// facts don't name them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct MachineShare {
    pub everything: bool,
    pub machines: std::collections::BTreeMap<String, BTreeSet<Uuid>>,
}

impl MachineShare {
    /// The places machine `m`'s copy of the rule names: `all`, or its
    /// sessions by id (none: nothing of it but the agent's own work there).
    pub fn places_for(&self, m: &str) -> Vec<String> {
        if self.everything {
            return vec!["all".to_string()];
        }
        self.machines
            .get(m)
            .map(|ids| ids.iter().map(|id| format!("session:{id}")).collect())
            .unwrap_or_default()
    }
}

pub fn machine_share(rule: &Rule, world: &World) -> MachineShare {
    let mut share = MachineShare {
        everything: rule.everything(),
        ..MachineShare::default()
    };
    if share.everything {
        return share;
    }
    let mut add = |m: String, id: Uuid| {
        share.machines.entry(m).or_default().insert(id);
    };
    for place in &rule.places {
        match place {
            Place::Group(g) => {
                if let Some(group) = world.layout.groups.iter().find(|x| &x.id == g) {
                    for (m, id) in group.items.iter().filter_map(|i| machine_session_of(i)) {
                        add(m, id);
                    }
                }
            }
            Place::Grid(id) => {
                if let Some(g) = world.grids.iter().find(|g| &g.id == id) {
                    for (m, s) in &g.machines {
                        add(m.clone(), *s);
                    }
                }
            }
            Place::Session {
                machine: Some(m),
                id,
            } => add(m.clone(), *id),
            _ => {}
        }
    }
    share
}

/// What `rule` covers for the agent `agent` (its member key) in `world`:
/// its own work always, and each place's work as the sidebar, the boards,
/// the grids and the charges stand now, so a session joining a chosen group
/// is covered from then on, and one leaving it isn't.
pub fn resolve(agent: &str, rule: &Rule, world: &World) -> Covered {
    let mut c = Covered {
        everything: rule.everything(),
        ..Covered::default()
    };
    let runs_of = |boss: &str| -> Vec<Uuid> {
        world
            .runs
            .iter()
            .filter(|(b, _)| b == boss)
            .map(|(_, s)| *s)
            .collect()
    };
    // Its own work.
    let own = boss_id_of(agent);
    c.sessions.extend(runs_of(&own));
    c.bosses.insert(own);
    for place in &rule.places {
        match place {
            Place::All => {}
            Place::Group(g) => {
                if let Some(group) = world.layout.groups.iter().find(|x| &x.id == g) {
                    c.sessions
                        .extend(group.items.iter().filter_map(|i| session_of(i)));
                }
                for b in world
                    .boards
                    .iter()
                    .filter(|b| b.group.as_deref() == Some(g))
                {
                    c.boards.insert(b.id);
                    c.sessions.extend(&b.sessions);
                }
                c.groups.insert(g.clone());
            }
            Place::Board(id) => {
                if let Some(b) = world.boards.iter().find(|b| b.id == *id) {
                    c.boards.insert(b.id);
                    c.sessions.extend(&b.sessions);
                }
            }
            Place::Grid(id) => {
                if let Some(g) = world.grids.iter().find(|g| &g.id == id) {
                    c.sessions.extend(&g.sessions);
                }
            }
            Place::Session { machine: None, id } => {
                c.sessions.insert(*id);
            }
            // A machine's session is that machine's to share (Sees K7).
            Place::Session {
                machine: Some(_), ..
            } => {}
            Place::Agent { hub, agent } => {
                let boss = boss_id_of(&format!("{hub}/{agent}"));
                c.sessions.extend(runs_of(&boss));
                c.bosses.insert(boss);
            }
        }
    }
    c
}

/// One thing an agent was sent, as a narrowing reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    pub kind: String,
    pub reference: String,
    pub session: Option<Uuid>,
    pub group: Option<String>,
    pub board: Option<Uuid>,
    pub boss: Option<String>,
}

/// The kind a session's conversation is shared as.
pub const TRANSCRIPT: &str = "transcript";

/// What `held` holds that `rule`, covering `covered`, no longer would: a
/// transcript once transcripts are off, and anything of a place no longer
/// covered.
pub fn held_outside<'a>(held: &'a [Held], rule: &Rule, covered: &Covered) -> Vec<&'a Held> {
    held.iter()
        .filter(|h| {
            (h.kind == TRANSCRIPT && !rule.transcripts)
                || !covered.covers(h.session, h.group.as_deref(), h.board, h.boss.as_deref())
        })
        .collect()
}

/// Counts of what is held, by kind, and the sessions it is of.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct HeldCount {
    pub items: usize,
    pub transcripts: usize,
    pub sessions: usize,
    pub by_kind: HashMap<String, usize>,
}

pub fn count(held: &[&Held]) -> HeldCount {
    let mut c = HeldCount {
        items: held.len(),
        ..HeldCount::default()
    };
    let mut sessions = HashSet::new();
    for h in held {
        *c.by_kind.entry(h.kind.clone()).or_default() += 1;
        if h.kind == TRANSCRIPT {
            c.transcripts += 1;
        }
        if let Some(s) = h.session {
            sessions.insert(s);
        }
    }
    c.sessions = sessions.len();
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::Group;

    fn u(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn world() -> World {
        World {
            layout: Layout {
                groups: vec![
                    Group {
                        id: "patchwork".into(),
                        name: "Patchwork".into(),
                        collapsed: false,
                        items: vec![
                            format!("session:{}", u(1)),
                            format!("session:devbox/{}", u(9)),
                            "agent:local/sarah".into(),
                        ],
                    },
                    Group {
                        id: "acme".into(),
                        name: "Acme".into(),
                        collapsed: false,
                        items: vec![format!("session:{}", u(2))],
                    },
                ],
                ..Layout::default()
            },
            boards: vec![
                BoardFacts {
                    id: u(100),
                    group: Some("patchwork".into()),
                    sessions: vec![u(3)],
                },
                BoardFacts {
                    id: u(200),
                    group: None,
                    sessions: vec![u(4)],
                },
            ],
            grids: vec![GridFacts {
                id: "g1".into(),
                sessions: vec![u(5)],
                machines: vec![("devbox".into(), u(8))],
            }],
            runs: vec![("sarah".into(), u(6)), ("driftwerk/allan".into(), u(7))],
        }
    }

    fn rule(places: &[&str]) -> Rule {
        Rule::new(
            &places.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            false,
            Digest::Off,
        )
        .unwrap()
    }

    #[test]
    fn each_place_round_trips_and_a_bad_one_is_said_plainly() {
        for t in [
            "all",
            "group:patchwork",
            &format!("board:{}", u(1)),
            "grid:g1",
            &format!("session:{}", u(2)),
            &format!("session:devbox/{}", u(3)),
            "agent:driftwerk/allan",
        ] {
            assert_eq!(Place::parse(t).unwrap().text(), *t);
        }
        assert_eq!(Place::parse(" ALL ").unwrap(), Place::All);
        for bad in [
            "",
            "everything",
            "group:",
            "board:x",
            "session:nope",
            "session:dev box/1",
            "agent:allan",
            "repo:a/b",
        ] {
            let why = Place::parse(bad).unwrap_err();
            assert!(why.ends_with('.'), "{bad:?}: {why}");
        }
    }

    #[test]
    fn all_among_places_is_all_alone_and_a_place_is_named_once() {
        let r = rule(&["group:acme", "all", "grid:g1"]);
        assert!(r.everything());
        assert_eq!(r.texts(), ["all"]);
        let r = rule(&["group:acme", "group:acme"]);
        assert_eq!(r.texts(), ["group:acme"]);
        let many: Vec<String> = (0..=MAX_PLACES).map(|i| format!("grid:g{i}")).collect();
        assert!(Rule::new(&many, false, Digest::Off).is_err());
    }

    #[test]
    fn everything_covers_a_bossed_group_and_every_row() {
        let c = resolve("driftwerk/allan", &rule(&["all"]), &world());
        assert!(c.everything);
        assert!(c.covers(Some(u(2)), Some("acme"), None, None));
        assert!(c.covers(None, None, Some(u(200)), None));
        assert!(c.covers(None, None, None, Some("sarah")));
    }

    #[test]
    fn a_group_covers_its_sessions_now_and_its_boards_and_nothing_else() {
        let mut w = world();
        let c = resolve("patchwork/lattice", &rule(&["group:patchwork"]), &w);
        assert!(c.sessions.contains(&u(1)), "in the group");
        assert!(c.sessions.contains(&u(3)), "on its board");
        assert!(
            !c.sessions.contains(&u(9)),
            "a machine's session is the machine's to share"
        );
        assert!(!c.covers(Some(u(2)), Some("acme"), None, None), "Acme's");
        assert!(
            !c.covers(Some(u(2)), Some("patchwork"), None, None),
            "a session row is read by its session"
        );
        assert!(c.covers(None, Some("patchwork"), None, None));
        assert!(c.covers(None, None, Some(u(100)), None));
        assert!(!c.covers(None, None, Some(u(200)), None));
        // A session that joins the group is covered from then on.
        w.layout.groups[0].items.push(format!("session:{}", u(2)));
        w.layout.groups[1].items.clear();
        let c = resolve("patchwork/lattice", &rule(&["group:patchwork"]), &w);
        assert!(c.covers(Some(u(2)), None, None, None));
    }

    #[test]
    fn boards_grids_sessions_and_agents_cover_what_they_name() {
        let w = world();
        let c = resolve("local/mia", &rule(&[&format!("board:{}", u(200))]), &w);
        assert!(c.sessions.contains(&u(4)) && c.boards.contains(&u(200)));
        let c = resolve("local/mia", &rule(&["grid:g1"]), &w);
        assert_eq!(c.sessions, [u(5)].into());
        let c = resolve("local/mia", &rule(&[&format!("session:{}", u(2))]), &w);
        assert!(c.sessions.contains(&u(2)));
        let c = resolve("local/mia", &rule(&["agent:local/sarah"]), &w);
        assert!(c.sessions.contains(&u(6)));
        assert!(c.covers(None, None, None, Some("sarah")));
        let c = resolve("local/mia", &rule(&["agent:driftwerk/allan"]), &w);
        assert!(c.sessions.contains(&u(7)));
    }

    #[test]
    fn its_own_work_is_always_covered_and_only_that_by_default() {
        let w = world();
        let c = resolve("local/sarah", &Rule::default(), &w);
        assert_eq!(c.sessions, [u(6)].into());
        assert!(c.covers(None, None, None, Some("sarah")));
        assert!(!c.covers(None, Some("patchwork"), None, None));
        let c = resolve("driftwerk/allan", &Rule::default(), &w);
        assert_eq!(c.sessions, [u(7)].into(), "a served boss by its party id");
    }

    #[test]
    fn a_grid_is_read_from_the_pages_value_and_a_broken_one_holds_nothing() {
        let v = serde_json::json!({
            "id": "g1", "name": "Main",
            "panes": [
                {"id": "a", "content": {"kind": "session", "sessionId": u(1).to_string()}},
                {"id": "b", "content": {"kind": "session", "sessionId": u(2).to_string(), "machine": "devbox"}},
                {"id": "c", "content": {"kind": "agent", "agentId": "x"}},
                {"id": "d"}
            ],
            "placement": {"kind": "main"}
        });
        assert_eq!(
            grid_facts("g1", &v),
            Some(GridFacts {
                id: "g1".into(),
                sessions: vec![u(1)],
                machines: vec![("devbox".into(), u(2))],
            })
        );
        // The key's grid, whatever its value says.
        assert_eq!(grid_facts("g2", &v).unwrap().id, "g2");
        assert_eq!(grid_facts("g1", &serde_json::json!({"id": 3})), None);
    }

    #[test]
    fn each_machine_is_given_the_sessions_of_its_own_that_a_rule_covers() {
        let w = world();
        let share = machine_share(&rule(&["all"]), &w);
        assert!(share.everything);
        assert_eq!(share.places_for("devbox"), vec!["all".to_string()]);
        assert_eq!(share.places_for("laptop"), vec!["all".to_string()]);
        // A group's, a grid's and one named, each on its machine.
        let share = machine_share(
            &rule(&[
                "group:patchwork",
                "grid:g1",
                &format!("session:laptop/{}", u(11)),
            ]),
            &w,
        );
        assert!(!share.everything);
        assert_eq!(
            share.places_for("devbox"),
            vec![format!("session:{}", u(8)), format!("session:{}", u(9))]
        );
        assert_eq!(
            share.places_for("laptop"),
            vec![format!("session:{}", u(11))]
        );
        // The Mac's own sessions and boards aren't a machine's.
        let share = machine_share(
            &rule(&["group:acme", "board:00000000-0000-0000-0000-000000000064"]),
            &w,
        );
        assert!(share.machines.is_empty());
        assert!(share.places_for("devbox").is_empty());
        assert!(machine_share(&Rule::default(), &w).machines.is_empty());
    }

    #[test]
    fn narrowing_finds_what_is_held_outside_by_session_and_by_transcripts() {
        let held = |kind: &str, session: Option<Uuid>| Held {
            kind: kind.into(),
            reference: format!("{kind}-{session:?}"),
            session,
            group: None,
            board: None,
            boss: None,
        };
        let held = vec![
            held("account", Some(u(1))),
            held(TRANSCRIPT, Some(u(1))),
            held(TRANSCRIPT, Some(u(2))),
            held("account", Some(u(2))),
        ];
        let w = world();
        let mut r = rule(&["all"]);
        r.transcripts = true;
        let c = resolve("driftwerk/allan", &r, &w);
        assert!(held_outside(&held, &r, &c).is_empty());
        // Transcripts off: every transcript.
        r.transcripts = false;
        let out = held_outside(&held, &r, &c);
        assert_eq!(count(&out).transcripts, 2);
        assert_eq!(count(&out).items, 2);
        // Narrowed to Patchwork: everything of Acme's session too.
        let mut r = rule(&["group:patchwork"]);
        r.transcripts = true;
        let c = resolve("driftwerk/allan", &r, &w);
        let out = held_outside(&held, &r, &c);
        let n = count(&out);
        assert_eq!((n.items, n.sessions, n.transcripts), (2, 1, 1));
        assert!(out.iter().all(|h| h.session == Some(u(2))));
    }

    #[test]
    fn agent_keys_are_member_keys() {
        assert!(agent_key_ok("local/sarah") && agent_key_ok("driftwerk/allan"));
        for bad in [
            "allan", "/allan", "a/b/c", "a b/c", "", "../allan", "dw/..", "./x",
        ] {
            assert!(!agent_key_ok(bad), "{bad}");
        }
        assert_eq!(boss_id_of("local/sarah"), "sarah");
        assert_eq!(boss_id_of("driftwerk/allan"), "driftwerk/allan");
    }
}
