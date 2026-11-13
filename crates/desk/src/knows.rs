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
}

/// The sessions in a grid's panes, from the value the page keeps under
/// `grid.<id>` (`src/grids/model.ts` in workforce-desk): `{id, name,
/// panes: [{content: {kind: "session", sessionId, machine?}}]}`. A
/// machine's session is left for that machine (Sees K7); a value that
/// isn't a grid holds none.
pub fn grid_facts(value: &serde_json::Value) -> Option<GridFacts> {
    let id = value.get("id")?.as_str()?.to_string();
    let panes = value.get("panes")?.as_array()?;
    let sessions = panes
        .iter()
        .filter_map(|p| {
            let c = p.get("content")?;
            (c.get("kind")?.as_str()? == "session" && c.get("machine").is_none_or(|m| m.is_null()))
                .then(|| c.get("sessionId")?.as_str()?.parse().ok())
                .flatten()
        })
        .collect();
    Some(GridFacts { id, sessions })
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
            grid_facts(&v),
            Some(GridFacts {
                id: "g1".into(),
                sessions: vec![u(1)]
            })
        );
        assert_eq!(grid_facts(&serde_json::json!({"id": 3})), None);
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
        for bad in ["allan", "/allan", "a/b/c", "a b/c", ""] {
            assert!(!agent_key_ok(bad), "{bad}");
        }
        assert_eq!(boss_id_of("local/sarah"), "sarah");
        assert_eq!(boss_id_of("driftwerk/allan"), "driftwerk/allan");
    }
}
