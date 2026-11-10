//! A grant: what the home (the hub on the person's Mac, which keeps the one
//! sidebar) tells one engine about one group (remote bosses, RB1). It names
//! the group, which of this engine's sessions are in it (`members`, and
//! `born`: those started here in the group that the home hasn't listed
//! yet), the boss, what the boss may do and see, and when it expires.
//!
//! The engine keeps its own copy and decides from it, so a grant works
//! while the home is asleep; it expires by itself if the home doesn't renew
//! it. On a machine the grants are the only place its sessions' groups come
//! from: the machine's own old layout is no longer read.
//!
//! Everything here is pure: what a write from the home does to the copy,
//! whether a grant is live, and the layout the grants make on a machine.
//! The routes that write grants are the hub's own token's alone (Desk's
//! native side, never the page, an agent or a session), and nothing a boss
//! says reaches them.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::service::{Group, Layout, Ungrouped, UNGROUPED};

/// The grant shape this engine reads. One it doesn't know serves nothing.
pub const GRANT_VERSION: u32 = 1;
/// How long a grant lasts after the home last wrote or renewed it, unless
/// the home says otherwise (`[desk.bosses] grant_hours`).
pub const DEFAULT_HOURS: u32 = 72;
/// The longest a grant may last: 14 days.
pub const MAX_HOURS: u32 = 336;
/// The most sessions one grant may name.
pub const MAX_MEMBERS: usize = 5000;
const MAX_NAME: usize = 200;
const MAX_ID: usize = 100;
const MAX_LIST: usize = 64;
const MAX_ENTRY: usize = 1024;

/// What the engine is told of grants by its install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BossOptions {
    /// Whether this hub is a machine's, as Desk's helper wrote it
    /// (`[desk] machine = true`): only then do grants say its sessions'
    /// groups. Unset is not a machine here: a grant must never rewrite the
    /// person's own sidebar.
    pub machine: bool,
    /// How long a grant lasts when the home doesn't say (1 to 336 hours).
    pub grant_hours: u32,
}

impl Default for BossOptions {
    fn default() -> Self {
        Self {
            machine: false,
            grant_hours: DEFAULT_HOURS,
        }
    }
}

/// How much the boss may do, as the person chose it in Desk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    #[default]
    Watch,
    Suggest,
    Act,
    Full,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Watch => "watch",
            Self::Suggest => "suggest",
            Self::Act => "act",
            Self::Full => "full",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "watch" => Self::Watch,
            "suggest" => Self::Suggest,
            "act" => Self::Act,
            "full" => Self::Full,
            _ => return None,
        })
    }
}

/// What the boss may see of the group's sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Sees {
    #[default]
    Status,
    Replies,
    Everything,
}

impl Sees {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Replies => "replies",
            Self::Everything => "everything",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "status" => Self::Status,
            "replies" => Self::Replies,
            "everything" => Self::Everything,
            _ => return None,
        })
    }
}

/// Who the boss is: an agent on the Mac's hub, or one on a paired hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BossRef {
    Local { agent: String },
    Remote { hub: String, agent: String },
}

impl BossRef {
    /// `local:<agent>` or `remote:<hub>/<agent>`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("{s:?} isn't a boss: say local:<agent> or remote:<hub>/<agent>");
        let name_ok = |n: &str| {
            !n.is_empty()
                && n.len() <= MAX_ID
                && n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        match s.split_once(':') {
            Some(("local", agent)) if name_ok(agent) => Ok(Self::Local {
                agent: agent.into(),
            }),
            Some(("remote", rest)) => match rest.split_once('/') {
                Some((hub, agent)) if name_ok(hub) && name_ok(agent) => Ok(Self::Remote {
                    hub: hub.into(),
                    agent: agent.into(),
                }),
                _ => Err(bad()),
            },
            _ => Err(bad()),
        }
    }

    pub fn key(&self) -> String {
        match self {
            Self::Local { agent } => format!("local:{agent}"),
            Self::Remote { hub, agent } => format!("remote:{hub}/{agent}"),
        }
    }
}

/// Who stopped a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoppedFrom {
    /// Desk on the Mac (Stop on the group, or Stop all remote bosses).
    Desk,
    /// The machine itself (`desk-helper bosses stop` over SSH).
    Machine,
}

impl StoppedFrom {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desk => "desk",
            Self::Machine => "machine",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "desk" => Some(Self::Desk),
            "machine" => Some(Self::Machine),
            _ => None,
        }
    }
}

/// A stop: when, and from where. A stopped grant stays stopped until the
/// home writes it again saying `resume`, which only the person's own Desk
/// does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stopped {
    pub at: DateTime<Utc>,
    pub from: StoppedFrom,
}

/// One group's grant, as this engine keeps it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    /// The home's id for the group (`ungrouped` for the ungrouped scope).
    pub group: String,
    pub name: String,
    /// The home's count of its writes of this grant: only ever higher.
    pub revision: i64,
    pub grant_version: u32,
    /// None: the grant only says who is in the group, and serves no one.
    #[serde(serialize_with = "boss_key")]
    pub boss: Option<BossRef>,
    pub level: Level,
    pub sees: Sees,
    /// Whether the boss may search what Desk remembers of the group.
    pub recall: bool,
    /// Where the boss may start sessions on this engine.
    pub folders: Vec<String>,
    /// The repositories Act fully may ship from.
    pub repos: Vec<String>,
    /// This engine's sessions in the group, as the home listed them.
    pub members: BTreeSet<Uuid>,
    /// This engine's sessions started in the group that the home hasn't
    /// listed yet. The engine adds them; the home takes them over.
    pub born: BTreeSet<Uuid>,
    /// The level's own switches, as the home's `[oversight]` has them.
    pub act: serde_json::Value,
    pub stopped: Option<Stopped>,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

fn boss_key<S: serde::Serializer>(b: &Option<BossRef>, s: S) -> Result<S::Ok, S::Error> {
    match b {
        Some(b) => s.serialize_some(&b.key()),
        None => s.serialize_none(),
    }
}

/// What the home writes: `PUT /api/desk/grants/{group}`. A field this
/// engine doesn't know is refused, since it might narrow what it can't
/// honour.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantPut {
    pub grant_version: u32,
    pub revision: i64,
    pub name: String,
    #[serde(default)]
    pub boss: Option<String>,
    /// Watch only, unless said: a grant only ever narrows what is said.
    #[serde(default)]
    pub level: Level,
    #[serde(default)]
    pub sees: Sees,
    #[serde(default)]
    pub recall: bool,
    #[serde(default)]
    pub folders: Vec<String>,
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default)]
    pub members: Vec<Uuid>,
    /// Sessions born here that the home has taken out of the group.
    #[serde(default)]
    pub leaving: Vec<Uuid>,
    #[serde(default = "empty_object")]
    pub act: serde_json::Value,
    /// How long it lasts from now, in hours (1 to 336); unset, the
    /// engine's `grant_hours`.
    #[serde(default)]
    pub hours: Option<u32>,
    /// Starts a stopped grant again: only the person, in Desk, says so.
    #[serde(default)]
    pub resume: bool,
}

fn empty_object() -> serde_json::Value {
    serde_json::json!({})
}

/// The part of a write every version has: read first, so a grant of a
/// version this engine doesn't know is still refused in words.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub grant_version: u32,
    #[serde(default)]
    pub revision: i64,
}

/// Why a write from the home wasn't taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// It is older than what the engine has.
    Stale { have: i64 },
    /// It disagrees with what the engine has (409).
    Conflict(String),
    /// It can't be read as a grant (400).
    Invalid(String),
}

/// What a write does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    /// The group had no grant here.
    New,
    /// The same revision again: only its expiry moves.
    Renewed,
    /// A newer revision.
    Revised,
}

/// A write as taken: the grant as it now stands, and the born sessions it
/// takes from other grants (the home listed them in this group).
#[derive(Debug, Clone, PartialEq)]
pub struct Accepted {
    pub grant: Grant,
    pub change: Change,
    pub taken: Vec<(String, Uuid)>,
}

fn invalid(why: impl Into<String>) -> Refusal {
    Refusal::Invalid(why.into())
}

/// Whether `id` can name a group here: as the layout's ids are.
pub fn group_id_ok(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= MAX_ID && !id.contains('/') && !id.contains(':')
}

fn check_list(what: &str, list: &[String]) -> Result<(), Refusal> {
    if list.len() > MAX_LIST {
        return Err(invalid(format!("at most {MAX_LIST} {what}")));
    }
    if list
        .iter()
        .any(|e| e.trim().is_empty() || e.len() > MAX_ENTRY || e.contains('\0'))
    {
        return Err(invalid(format!("one of the {what} is empty or too long")));
    }
    Ok(())
}

impl GrantPut {
    /// Refuses what can't be a grant, in plain words.
    fn check(&self, group: &str) -> Result<Option<BossRef>, Refusal> {
        if !group_id_ok(group) {
            return Err(invalid(format!("{group:?} can't be a group's id")));
        }
        if self.revision < 1 {
            return Err(invalid("a grant's revision starts at 1"));
        }
        let name = self.name.trim();
        if name.is_empty() || name.len() > MAX_NAME {
            return Err(invalid("a grant needs its group's name"));
        }
        if self.members.len() > MAX_MEMBERS {
            return Err(invalid(format!(
                "a grant names at most {MAX_MEMBERS} sessions"
            )));
        }
        let members: BTreeSet<Uuid> = self.members.iter().copied().collect();
        if members.len() != self.members.len() {
            return Err(invalid("a session is named twice"));
        }
        if self.leaving.iter().any(|s| members.contains(s)) {
            return Err(invalid(
                "a session can't be both in the group and leaving it",
            ));
        }
        check_list("folders", &self.folders)?;
        if let Some(f) = self.folders.iter().find(|f| !f.starts_with('/')) {
            return Err(invalid(format!("{f:?} isn't a whole path")));
        }
        check_list("repositories", &self.repos)?;
        if !self.act.is_object() {
            return Err(invalid("act is an object of switches"));
        }
        if serde_json::to_vec(&self.act).map_or(true, |b| b.len() > 16 * 1024) {
            return Err(invalid("act is too large"));
        }
        let boss = self
            .boss
            .as_deref()
            .map(BossRef::parse)
            .transpose()
            .map_err(Refusal::Invalid)?;
        if group == UNGROUPED && matches!(boss, Some(BossRef::Remote { .. })) {
            // The ungrouped scope holds the person's unsorted work: its
            // boss stays on the Mac (open decision 11).
            return Err(invalid(
                "a boss on another hub runs a group, never Ungrouped",
            ));
        }
        Ok(boss)
    }

    /// Whether this says what `g` says, apart from when it was written.
    fn says(&self, g: &Grant, boss: &Option<BossRef>) -> bool {
        let members: BTreeSet<Uuid> = self.members.iter().copied().collect();
        g.name == self.name.trim()
            && g.boss == *boss
            && g.level == self.level
            && g.sees == self.sees
            && g.recall == self.recall
            && g.folders == self.folders
            && g.repos == self.repos
            && g.members == members
            && g.act == self.act
            && self.leaving.iter().all(|s| !g.born.contains(s))
    }
}

/// How many hours `asked` gives, or why it can't.
pub fn hours_of(asked: Option<u32>, default: u32) -> Result<u32, Refusal> {
    let h = asked.unwrap_or(default);
    if (1..=MAX_HOURS).contains(&h) {
        Ok(h)
    } else {
        Err(invalid(format!(
            "a grant lasts 1 to {MAX_HOURS} hours (14 days), not {h}"
        )))
    }
}

/// What the home's write `put` for `group` makes of `old`, this engine's
/// copy, at `now`; `others` are the engine's other grants. Revisions only
/// go up; the same one again renews it. A session is in one grant at most:
/// one another grant lists is refused, one only born there moves here. A
/// stop holds until `resume`, and nothing here clears the engine's pause.
pub fn accept(
    old: Option<&Grant>,
    put: &GrantPut,
    group: &str,
    others: &[&Grant],
    now: DateTime<Utc>,
    default_hours: u32,
) -> Result<Accepted, Refusal> {
    if put.grant_version != GRANT_VERSION {
        return Err(invalid(unknown_version(put.grant_version)));
    }
    let boss = put.check(group)?;
    let hours = hours_of(put.hours, default_hours)?;
    let expires_at = now + Duration::hours(i64::from(hours));
    let members: BTreeSet<Uuid> = put.members.iter().copied().collect();
    let mut taken = Vec::new();
    for other in others.iter().filter(|o| o.group != group) {
        if let Some(s) = other.members.iter().find(|s| members.contains(s)) {
            return Err(Refusal::Conflict(format!(
                "session {s} is in {}'s grant: take it out of that one first",
                other.name
            )));
        }
        for s in other.born.iter().filter(|s| members.contains(s)) {
            taken.push((other.group.clone(), *s));
        }
    }
    let Some(old) = old else {
        return Ok(Accepted {
            grant: Grant {
                group: group.into(),
                name: put.name.trim().into(),
                revision: put.revision,
                grant_version: GRANT_VERSION,
                boss,
                level: put.level,
                sees: put.sees,
                recall: put.recall,
                folders: put.folders.clone(),
                repos: put.repos.clone(),
                members,
                born: BTreeSet::new(),
                act: put.act.clone(),
                stopped: None,
                issued_at: now,
                expires_at,
            },
            change: Change::New,
            taken,
        });
    };
    if put.revision < old.revision {
        return Err(Refusal::Stale { have: old.revision });
    }
    if put.revision == old.revision {
        // A grant this engine couldn't read is never renewed into one.
        if old.grant_version != GRANT_VERSION || !put.says(old, &boss) || put.resume {
            return Err(Refusal::Conflict(format!(
                "revision {} of {} already says otherwise: send a newer one",
                old.revision, old.name
            )));
        }
        let mut grant = old.clone();
        grant.issued_at = now;
        grant.expires_at = expires_at;
        return Ok(Accepted {
            grant,
            change: Change::Renewed,
            taken,
        });
    }
    let born = old
        .born
        .iter()
        .filter(|s| !members.contains(s) && !put.leaving.contains(s))
        .copied()
        .collect();
    Ok(Accepted {
        grant: Grant {
            group: group.into(),
            name: put.name.trim().into(),
            revision: put.revision,
            grant_version: GRANT_VERSION,
            boss,
            level: put.level,
            sees: put.sees,
            recall: put.recall,
            folders: put.folders.clone(),
            repos: put.repos.clone(),
            members,
            born,
            act: put.act.clone(),
            stopped: if put.resume { None } else { old.stopped },
            issued_at: now,
            expires_at,
        },
        change: Change::Revised,
        taken,
    })
}

/// What a write of a version this engine doesn't know does to its copy:
/// the copy stays the group's members (so its rows stay where they are)
/// and serves nothing until this engine is upgraded. None when there is no
/// copy, or the write is older than it.
pub fn unreadable(old: Option<&Grant>, version: u32, revision: i64) -> Option<Grant> {
    let old = old?;
    if revision < old.revision {
        return None;
    }
    let mut g = old.clone();
    g.grant_version = version;
    g.revision = revision;
    Some(g)
}

/// What an engine says of a grant version it doesn't know.
pub fn unknown_version(version: u32) -> String {
    format!(
        "This engine reads grants of version {GRANT_VERSION}, not {version}: it serves nothing \
         for this group until Desk upgrades it."
    )
}

/// `g` stopped at `now` from `from`; one already stopped keeps its first
/// stop.
pub fn stopped(g: &Grant, now: DateTime<Utc>, from: StoppedFrom) -> Grant {
    let mut g = g.clone();
    g.stopped.get_or_insert(Stopped { at: now, from });
    g
}

/// `g` with `session`, started here in the group, born in it.
pub fn with_born(g: &Grant, session: Uuid) -> Grant {
    let mut g = g.clone();
    if !g.members.contains(&session) {
        g.born.insert(session);
    }
    g
}

/// Why a grant serves nothing now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotLive {
    Paused,
    Version(u32),
    Stopped(Stopped),
    Expired(DateTime<Utc>),
    NoBoss,
}

impl NotLive {
    /// In plain words, for the boss and the person.
    pub fn say(&self, name: &str) -> String {
        match self {
            Self::Paused => {
                "Every boss on this engine is paused: only Desk on the person's Mac can let them \
                 go on."
                    .into()
            }
            Self::Version(v) => unknown_version(*v),
            Self::Stopped(s) => match s.from {
                StoppedFrom::Desk => format!("{name}'s boss was stopped in Desk."),
                StoppedFrom::Machine => format!("{name}'s boss was stopped on this machine."),
            },
            Self::Expired(_) => {
                format!("{name}'s grant expired: open Desk on the Mac to renew it.")
            }
            Self::NoBoss => format!("{name} has no boss here."),
        }
    }
}

/// Whether `g` serves its boss at `now`: the engine isn't paused, the grant
/// is of a version it reads, it isn't stopped, it hasn't expired (expiry
/// fails closed: at its moment it is over), and it names a boss.
pub fn live(g: &Grant, now: DateTime<Utc>, paused: bool) -> Result<(), NotLive> {
    if paused {
        return Err(NotLive::Paused);
    }
    if g.grant_version != GRANT_VERSION {
        return Err(NotLive::Version(g.grant_version));
    }
    if let Some(s) = g.stopped {
        return Err(NotLive::Stopped(s));
    }
    if now >= g.expires_at {
        return Err(NotLive::Expired(g.expires_at));
    }
    if g.boss.is_none() {
        return Err(NotLive::NoBoss);
    }
    Ok(())
}

/// Each session the grants name, with its group: members and born alike.
/// A stopped, expired or unreadable grant still says who is in its group:
/// those stop serving, never where rows are stamped.
pub fn membership<'a>(grants: impl IntoIterator<Item = &'a Grant>) -> BTreeMap<Uuid, String> {
    let mut out = BTreeMap::new();
    for g in grants {
        for s in g.members.iter().chain(g.born.iter()) {
            out.insert(*s, g.group.clone());
        }
    }
    out
}

/// The layout a machine's grants make: one group per grant (but
/// Ungrouped's), named as the home names it, holding its sessions; nothing
/// of the machine's own old groups. Each group keeps how it was shown
/// (collapsed) in `before`.
pub fn layout_of<'a>(grants: impl IntoIterator<Item = &'a Grant>, before: &Layout) -> Layout {
    let mut grants: Vec<&Grant> = grants
        .into_iter()
        .filter(|g| g.group != UNGROUPED)
        .collect();
    grants.sort_by(|a, b| (&a.name, &a.group).cmp(&(&b.name, &b.group)));
    let groups: Vec<Group> = grants
        .iter()
        .map(|g| Group {
            id: g.group.clone(),
            name: g.name.clone(),
            collapsed: before
                .groups
                .iter()
                .find(|b| b.id == g.group)
                .is_some_and(|b| b.collapsed),
            items: g
                .members
                .iter()
                .chain(g.born.iter())
                .map(|s| crate::service::session_item(*s))
                .collect(),
        })
        .collect();
    let grouped: BTreeSet<&String> = groups.iter().flat_map(|g| g.items.iter()).collect();
    Layout {
        ungrouped: Ungrouped {
            collapsed: before.ungrouped.collapsed,
            items: before
                .ungrouped
                .items
                .iter()
                .filter(|i| i.starts_with("session:") && !grouped.contains(i))
                .cloned()
                .collect(),
        },
        groups,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(h: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap() + Duration::hours(h)
    }

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn put(revision: i64, members: &[u128]) -> GrantPut {
        GrantPut {
            grant_version: GRANT_VERSION,
            revision,
            name: "Acme".into(),
            boss: Some("remote:driftwerk/allan".into()),
            level: Level::Suggest,
            sees: Sees::Replies,
            recall: false,
            folders: vec!["/home/sam/src/acme-api".into()],
            repos: vec![],
            members: members.iter().map(|n| id(*n)).collect(),
            leaving: vec![],
            act: empty_object(),
            hours: None,
            resume: false,
        }
    }

    fn take(old: Option<&Grant>, p: &GrantPut, now: DateTime<Utc>) -> Result<Accepted, Refusal> {
        accept(old, p, "g-acme", &[], now, DEFAULT_HOURS)
    }

    #[test]
    fn a_new_grant_lasts_72_hours_from_the_engines_own_clock() {
        let a = take(None, &put(1, &[1, 2]), at(0)).unwrap();
        assert_eq!(a.change, Change::New);
        assert_eq!(a.grant.expires_at, at(72));
        assert_eq!(a.grant.members, [id(1), id(2)].into());
        assert_eq!(live(&a.grant, at(71), false), Ok(()));
    }

    #[test]
    fn expiry_fails_closed_at_its_moment() {
        let g = take(None, &put(1, &[1]), at(0)).unwrap().grant;
        assert_eq!(live(&g, at(72), false), Err(NotLive::Expired(at(72))));
        assert!(live(&g, at(400), false).is_err());
    }

    #[test]
    fn hours_go_to_14_days_and_no_further() {
        let mut p = put(1, &[1]);
        p.hours = Some(336);
        assert_eq!(take(None, &p, at(0)).unwrap().grant.expires_at, at(336));
        for bad in [0, 337, u32::MAX] {
            p.hours = Some(bad);
            assert!(
                matches!(take(None, &p, at(0)), Err(Refusal::Invalid(_))),
                "{bad}"
            );
        }
        assert_eq!(hours_of(None, 24), Ok(24));
    }

    #[test]
    fn the_pause_stops_every_grant_whatever_else_holds() {
        let g = take(None, &put(1, &[1]), at(0)).unwrap().grant;
        assert_eq!(live(&g, at(1), true), Err(NotLive::Paused));
    }

    #[test]
    fn revisions_only_go_up_and_the_same_one_again_only_renews() {
        let g = take(None, &put(2, &[1]), at(0)).unwrap().grant;
        assert_eq!(
            take(Some(&g), &put(1, &[1, 2]), at(1)),
            Err(Refusal::Stale { have: 2 })
        );
        let renewed = take(Some(&g), &put(2, &[1]), at(10)).unwrap();
        assert_eq!(renewed.change, Change::Renewed);
        assert_eq!(renewed.grant.expires_at, at(82));
        assert_eq!(renewed.grant.members, g.members);
        assert!(matches!(
            take(Some(&g), &put(2, &[1, 2]), at(10)),
            Err(Refusal::Conflict(_))
        ));
        let revised = take(Some(&g), &put(3, &[1, 2]), at(10)).unwrap();
        assert_eq!(revised.change, Change::Revised);
        assert_eq!(revised.grant.members, [id(1), id(2)].into());
    }

    #[test]
    fn a_stop_holds_through_renewals_and_revisions_until_resume() {
        let g = take(None, &put(1, &[1]), at(0)).unwrap().grant;
        let s = stopped(&g, at(1), StoppedFrom::Machine);
        assert!(matches!(live(&s, at(2), false), Err(NotLive::Stopped(_))));
        // The home's next sync renews and revises it: still stopped.
        let renewed = take(Some(&s), &put(1, &[1]), at(3)).unwrap().grant;
        assert!(renewed.stopped.is_some());
        let revised = take(Some(&s), &put(2, &[1, 2]), at(3)).unwrap().grant;
        assert_eq!(revised.stopped, s.stopped);
        // A renewal can't resume it either: only a revision that says so.
        let mut resume = put(1, &[1]);
        resume.resume = true;
        assert!(take(Some(&s), &resume, at(3)).is_err());
        resume.revision = 2;
        let resumed = take(Some(&s), &resume, at(3)).unwrap().grant;
        assert_eq!(live(&resumed, at(4), false), Ok(()));
        // Stopping twice keeps the first stop.
        let again = stopped(&s, at(9), StoppedFrom::Desk);
        assert_eq!(again.stopped, s.stopped);
    }

    #[test]
    fn a_session_is_in_one_grant_at_most() {
        let globex = accept(None, &put(1, &[7]), "g-globex", &[], at(0), 72)
            .unwrap()
            .grant;
        let refused = accept(None, &put(1, &[1, 7]), "g-acme", &[&globex], at(0), 72);
        assert!(matches!(refused, Err(Refusal::Conflict(_))), "{refused:?}");
        // One only born there is the home's to place: it moves.
        let mut born = globex.clone();
        born.members.clear();
        born.born.insert(id(7));
        let a = accept(None, &put(1, &[1, 7]), "g-acme", &[&born], at(0), 72).unwrap();
        assert_eq!(a.taken, [("g-globex".to_string(), id(7))]);
    }

    #[test]
    fn born_sessions_stay_until_the_home_lists_them_or_takes_them_out() {
        let g = take(None, &put(1, &[1]), at(0)).unwrap().grant;
        let g = with_born(&with_born(&g, id(5)), id(6));
        assert_eq!(g.born, [id(5), id(6)].into());
        assert_eq!(with_born(&g, id(1)).born, g.born, "a member isn't born");
        let mut p = put(2, &[1, 5]);
        p.leaving = vec![id(6)];
        let next = take(Some(&g), &p, at(1)).unwrap().grant;
        assert_eq!(next.members, [id(1), id(5)].into());
        assert!(next.born.is_empty());
        // Unlisted and not taken out: still born, still in the group.
        let kept = take(Some(&g), &put(2, &[1]), at(1)).unwrap().grant;
        assert_eq!(kept.born, [id(5), id(6)].into());
        assert_eq!(
            membership([&kept]).get(&id(6)).map(String::as_str),
            Some("g-acme")
        );
    }

    #[test]
    fn what_cant_be_a_grant_is_refused_in_words() {
        let bad = |f: fn(&mut GrantPut)| {
            let mut p = put(1, &[1]);
            f(&mut p);
            take(None, &p, at(0))
        };
        assert!(bad(|p| p.revision = 0).is_err());
        assert!(bad(|p| p.name = " ".into()).is_err());
        assert!(bad(|p| p.members = vec![id(1), id(1)]).is_err());
        assert!(bad(|p| p.leaving = vec![id(1)]).is_err());
        assert!(bad(|p| p.folders = vec!["src/acme".into()]).is_err());
        assert!(bad(|p| p.boss = Some("allan".into())).is_err());
        assert!(bad(|p| p.boss = Some("remote:driftwerk/".into())).is_err());
        assert!(bad(|p| p.act = serde_json::json!([1])).is_err());
        assert!(accept(None, &put(1, &[1]), "devbox/g", &[], at(0), 72).is_err());
        let mut remote_ungrouped = put(1, &[1]);
        remote_ungrouped.name = "Ungrouped".into();
        assert!(accept(None, &remote_ungrouped, UNGROUPED, &[], at(0), 72).is_err());
        remote_ungrouped.boss = Some("local:desk".into());
        assert!(accept(None, &remote_ungrouped, UNGROUPED, &[], at(0), 72).is_ok());
    }

    #[test]
    fn a_field_this_engine_doesnt_know_is_refused() {
        let body = serde_json::json!({
            "grantVersion": 1, "revision": 1, "name": "Acme", "members": [],
            "onlyOnWeekdays": true,
        });
        assert!(serde_json::from_value::<GrantPut>(body).is_err());
    }

    #[test]
    fn a_grant_left_unset_watches_and_sees_status_only() {
        let body = serde_json::json!({"grantVersion": 1, "revision": 1, "name": "Acme"});
        let p: GrantPut = serde_json::from_value(body).unwrap();
        assert_eq!(
            (p.level, p.sees, p.recall),
            (Level::Watch, Sees::Status, false)
        );
        let g = take(None, &p, at(0)).unwrap().grant;
        assert_eq!(live(&g, at(1), false), Err(NotLive::NoBoss));
    }

    #[test]
    fn an_unknown_version_keeps_the_members_and_serves_nothing() {
        assert!(matches!(
            take(
                None,
                &GrantPut {
                    grant_version: 2,
                    ..put(1, &[1])
                },
                at(0)
            ),
            Err(Refusal::Invalid(_))
        ));
        let g = take(None, &put(1, &[1]), at(0)).unwrap().grant;
        assert!(unreadable(Some(&g), 2, 0).is_none(), "older than the copy");
        let u = unreadable(Some(&g), 2, 2).unwrap();
        assert_eq!(u.members, g.members);
        assert_eq!(live(&u, at(1), false), Err(NotLive::Version(2)));
        assert!(unreadable(None, 2, 2).is_none());
        // Nor is the same revision renewed into a readable one.
        assert!(take(Some(&u), &put(2, &[1]), at(1)).is_err());
        assert!(take(Some(&u), &put(3, &[1]), at(1)).is_ok());
    }

    #[test]
    fn a_stopped_or_expired_grant_still_says_who_is_in_the_group() {
        let g = take(None, &put(1, &[1]), at(0)).unwrap().grant;
        let s = stopped(&g, at(1), StoppedFrom::Desk);
        assert_eq!(membership([&s]), [(id(1), "g-acme".to_string())].into());
    }

    #[test]
    fn a_machines_layout_is_its_grants_and_nothing_of_its_old_groups() {
        let acme = take(None, &put(1, &[1, 2]), at(0)).unwrap().grant;
        let mut globex = accept(None, &put(1, &[3]), "g-globex", &[], at(0), 72)
            .unwrap()
            .grant;
        globex.name = "Globex".into();
        let mut desk = put(1, &[4]);
        desk.boss = None;
        let ungrouped = accept(None, &desk, UNGROUPED, &[], at(0), 72)
            .unwrap()
            .grant;
        let before = Layout {
            groups: vec![
                Group {
                    id: "old-acme".into(),
                    name: "Acme".into(),
                    collapsed: false,
                    items: vec![crate::service::session_item(id(1))],
                },
                Group {
                    id: "g-globex".into(),
                    name: "Globex".into(),
                    collapsed: true,
                    items: vec![],
                },
            ],
            ungrouped: Ungrouped {
                collapsed: false,
                items: vec![
                    crate::service::session_item(id(2)),
                    crate::service::session_item(id(9)),
                    "agent:local/desk".into(),
                ],
            },
        };
        let l = layout_of([&globex, &ungrouped, &acme], &before);
        let ids: Vec<&str> = l.groups.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, ["g-acme", "g-globex"]);
        assert_eq!(
            l.groups[0].items,
            [
                crate::service::session_item(id(1)),
                crate::service::session_item(id(2))
            ]
        );
        assert!(l.groups[1].collapsed, "kept as shown");
        assert_eq!(l.ungrouped.items, [crate::service::session_item(id(9))]);
    }
}
