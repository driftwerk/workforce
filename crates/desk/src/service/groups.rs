//! What the person calls things in Desk, kept by the hub so every agent
//! knows it: a session's own name, and the sidebar's groups of sessions and
//! agents. The layout has Desk's shape (`groups`, then the fixed
//! `ungrouped`), each group's members in order; a member is in one group
//! at most. Every change is committed, then announced as
//! `{type: "layout", layout}`.

use std::collections::{BTreeMap, HashSet};

use chrono::Utc;
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::model::{DeskEvent, GroupRef, SessionPatch, SessionView};
use crate::store::DeskStore;

/// The fixed group of whatever is in no other; always last.
pub const UNGROUPED: &str = "ungrouped";
const MAX_GROUPS: usize = 200;
const MAX_ITEMS: usize = 5000;
const MAX_NAME: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
    /// `session:<id>` or `agent:<hub>/<agent>`, in order.
    #[serde(default)]
    pub items: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ungrouped {
    #[serde(default)]
    pub collapsed: bool,
    /// The order of what was placed here; anything else in no group follows.
    #[serde(default)]
    pub items: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Layout {
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub ungrouped: Ungrouped,
}

/// A session's sidebar key.
pub fn session_item(id: Uuid) -> String {
    format!("session:{id}")
}

/// One of this hub's agents' sidebar key (Desk calls its local hub `local`).
pub fn local_agent_item(id: &str) -> String {
    format!("agent:local/{id}")
}

fn bad(why: impl Into<String>) -> DeskError {
    DeskError::Invalid(why.into())
}

fn check_item(item: &str) -> Result<(), DeskError> {
    let ok = match (item.strip_prefix("session:"), item.strip_prefix("agent:")) {
        (Some(id), _) => Uuid::parse_str(id).is_ok(),
        (_, Some(key)) => key
            .split_once('/')
            .is_some_and(|(hub, agent)| !hub.is_empty() && !agent.is_empty()),
        _ => false,
    };
    if ok && item.len() <= 300 {
        Ok(())
    } else {
        Err(bad(format!(
            "{item:?} is not session:<id> or agent:<hub>/<agent>"
        )))
    }
}

fn check_name(name: &str) -> Result<String, DeskError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME {
        return Err(bad(format!("a name is 1 to {MAX_NAME} characters")));
    }
    Ok(name.to_string())
}

fn without(items: &[String], item: &str) -> Vec<String> {
    items.iter().filter(|k| *k != item).cloned().collect()
}

fn insert(items: &[String], item: &str, before: Option<&str>) -> Vec<String> {
    let mut rest = without(items, item);
    let at = before.and_then(|b| rest.iter().position(|k| k == b));
    match at {
        Some(at) => rest.insert(at, item.to_string()),
        None => rest.push(item.to_string()),
    }
    rest
}

impl Layout {
    /// The group `item` is in (none: Ungrouped).
    pub fn group_of(&self, item: &str) -> Option<&Group> {
        self.groups
            .iter()
            .find(|g| g.items.iter().any(|k| k == item))
    }

    fn has(&self, id: &str) -> bool {
        id == UNGROUPED || self.groups.iter().any(|g| g.id == id)
    }

    /// Checked and tidied: names trimmed, ids unique, every member valid
    /// and in one place only (its first).
    pub fn checked(mut self) -> Result<Self, DeskError> {
        if self.groups.len() > MAX_GROUPS {
            return Err(bad(format!("at most {MAX_GROUPS} groups")));
        }
        let mut ids = HashSet::new();
        let mut seen = HashSet::new();
        let mut count = 0;
        for g in &mut self.groups {
            if g.id.trim().is_empty() || g.id.len() > 100 || g.id == UNGROUPED {
                return Err(bad(format!("{:?} can't be a group's id", g.id)));
            }
            if !ids.insert(g.id.clone()) {
                return Err(bad(format!("two groups are called {:?}", g.id)));
            }
            g.name = check_name(&g.name)?;
            g.items.retain(|k| seen.insert(k.clone()));
            count += g.items.len();
        }
        self.ungrouped.items.retain(|k| seen.insert(k.clone()));
        count += self.ungrouped.items.len();
        if count > MAX_ITEMS {
            return Err(bad(format!("at most {MAX_ITEMS} members")));
        }
        seen.iter().try_for_each(|k| check_item(k))?;
        Ok(self)
    }

    /// A new group named `name` before `before` (at the end for none),
    /// taking `items` from wherever they were.
    pub fn create(
        &self,
        id: &str,
        name: &str,
        items: &[String],
        before: Option<&str>,
    ) -> Result<Self, DeskError> {
        if self.has(id) {
            return Err(DeskError::Conflict(format!(
                "there is already a group {id:?}"
            )));
        }
        let moved: HashSet<&String> = items.iter().collect();
        let mut out = self.clone();
        for g in &mut out.groups {
            g.items.retain(|k| !moved.contains(k));
        }
        out.ungrouped.items.retain(|k| !moved.contains(k));
        let group = Group {
            id: id.to_string(),
            name: check_name(name)?,
            collapsed: false,
            items: items.to_vec(),
        };
        let at = before.and_then(|b| out.groups.iter().position(|g| g.id == b));
        match at {
            Some(at) => out.groups.insert(at, group),
            None => out.groups.push(group),
        }
        Ok(out)
    }

    /// Renames a group, opens or closes it (Ungrouped: only that).
    pub fn edit(
        &self,
        id: &str,
        name: Option<&str>,
        collapsed: Option<bool>,
    ) -> Result<Self, DeskError> {
        let mut out = self.clone();
        if id == UNGROUPED {
            if name.is_some() {
                return Err(bad("Ungrouped can't be renamed"));
            }
            if let Some(c) = collapsed {
                out.ungrouped.collapsed = c;
            }
            return Ok(out);
        }
        let g = out
            .groups
            .iter_mut()
            .find(|g| g.id == id)
            .ok_or(DeskError::NotFound)?;
        if let Some(name) = name {
            g.name = check_name(name)?;
        }
        if let Some(c) = collapsed {
            g.collapsed = c;
        }
        Ok(out)
    }

    /// Removes a group; its members go to the end of Ungrouped, in order.
    pub fn delete(&self, id: &str) -> Result<Self, DeskError> {
        if id == UNGROUPED {
            return Err(bad("Ungrouped is always there"));
        }
        let mut out = self.clone();
        let at = out
            .groups
            .iter()
            .position(|g| g.id == id)
            .ok_or(DeskError::NotFound)?;
        let gone = out.groups.remove(at);
        out.ungrouped.items.extend(gone.items);
        Ok(out)
    }

    /// Puts a group before another (at the end, above Ungrouped, for none).
    pub fn move_group(&self, id: &str, before: Option<&str>) -> Result<Self, DeskError> {
        let mut out = self.clone();
        let at = out
            .groups
            .iter()
            .position(|g| g.id == id)
            .ok_or(DeskError::NotFound)?;
        if before == Some(id) {
            return Ok(out);
        }
        let group = out.groups.remove(at);
        let to = before.and_then(|b| out.groups.iter().position(|g| g.id == b));
        match to {
            Some(to) => out.groups.insert(to, group),
            None => out.groups.push(group),
        }
        Ok(out)
    }

    /// Puts `item` in group `to` (or Ungrouped) before `before` (at the end
    /// for none), out of wherever it was. Moving into a closed group opens
    /// it, as Desk does.
    pub fn put(&self, item: &str, to: &str, before: Option<&str>) -> Result<Self, DeskError> {
        check_item(item)?;
        if !self.has(to) {
            return Err(DeskError::NotFound);
        }
        if before == Some(item) {
            return Ok(self.clone());
        }
        let mut out = self.clone();
        for g in &mut out.groups {
            if g.id == to {
                g.collapsed = false;
                g.items = insert(&g.items, item, before);
            } else {
                g.items = without(&g.items, item);
            }
        }
        out.ungrouped.items = if to == UNGROUPED {
            out.ungrouped.collapsed = false;
            insert(&out.ungrouped.items, item, before)
        } else {
            without(&out.ungrouped.items, item)
        };
        Ok(out)
    }

    /// Takes `item` out of its group, to the end of Ungrouped.
    pub fn remove(&self, item: &str) -> Result<Self, DeskError> {
        match self.group_of(item) {
            Some(_) => self.put(item, UNGROUPED, None),
            None => Ok(self.clone()),
        }
    }
}

impl DeskStore {
    pub async fn layout(&self) -> Result<Layout> {
        let client = self.pool().get().await?;
        let groups = client
            .query(
                "SELECT id, name, collapsed FROM groups ORDER BY position, id",
                &[],
            )
            .await?;
        let members = client
            .query(
                "SELECT item, group_id FROM group_members ORDER BY group_id, position",
                &[],
            )
            .await?;
        let mut layout = Layout::default();
        for g in groups {
            let id: String = g.get(0);
            let collapsed: bool = g.get(2);
            if id == UNGROUPED {
                layout.ungrouped.collapsed = collapsed;
            } else {
                layout.groups.push(Group {
                    id,
                    name: g.get(1),
                    collapsed,
                    items: Vec::new(),
                });
            }
        }
        for m in members {
            let (item, group): (String, String) = (m.get(0), m.get(1));
            if group == UNGROUPED {
                layout.ungrouped.items.push(item);
            } else if let Some(g) = layout.groups.iter_mut().find(|g| g.id == group) {
                g.items.push(item);
            }
        }
        Ok(layout)
    }

    /// Replaces the layout, in one transaction.
    pub async fn put_layout(&self, layout: &Layout) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        tx.execute("DELETE FROM groups", &[]).await?;
        let now = Utc::now();
        let ungrouped = Group {
            id: UNGROUPED.into(),
            name: "Ungrouped".into(),
            collapsed: layout.ungrouped.collapsed,
            items: layout.ungrouped.items.clone(),
        };
        for (position, g) in layout.groups.iter().chain([&ungrouped]).enumerate() {
            let position = i32::try_from(position).unwrap_or(i32::MAX);
            tx.execute(
                "INSERT INTO groups (id, name, position, collapsed, updated_at) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[&g.id, &g.name, &position, &g.collapsed, &now],
            )
            .await?;
            for (at, item) in g.items.iter().enumerate() {
                let at = i32::try_from(at).unwrap_or(i32::MAX);
                tx.execute(
                    "INSERT INTO group_members (item, group_id, position) VALUES ($1, $2, $3)",
                    &[item, &g.id, &at],
                )
                .await?;
            }
        }
        tx.commit().await.context("saving the sidebar's groups")?;
        Ok(())
    }

    /// Whether the person has named or grouped anything yet.
    pub async fn layout_is_empty(&self) -> Result<bool> {
        let client = self.pool().get().await?;
        let row = client
            .query_one(
                "SELECT NOT EXISTS (SELECT 1 FROM group_members) \
                 AND NOT EXISTS (SELECT 1 FROM groups WHERE id <> 'ungrouped') \
                 AND NOT EXISTS (SELECT 1 FROM sessions WHERE display_name IS NOT NULL)",
                &[],
            )
            .await?;
        Ok(row.get(0))
    }
}

/// What Desk pushes the first time it meets a hub: its local layout and
/// session names.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Import {
    #[serde(default)]
    pub layout: Layout,
    /// Session id to the person's name for it.
    #[serde(default)]
    pub session_names: BTreeMap<String, String>,
    /// Replace what the hub has even when it already has names or groups.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Imported {
    /// False when the hub already had names or groups (and `force` was
    /// not set): it kept them.
    pub imported: bool,
    pub layout: Layout,
    pub session_names: BTreeMap<String, String>,
}

impl Desk {
    /// The sidebar's groups as the hub has them.
    pub fn layout(&self) -> Layout {
        self.0.state.lock().layout.clone()
    }

    /// The group a session is in.
    pub(super) fn group_ref(layout: &Layout, item: &str) -> Option<GroupRef> {
        layout.group_of(item).map(|g| GroupRef {
            id: g.id.clone(),
            name: g.name.clone(),
        })
    }

    /// Applies `f` to the layout, commits it, then announces it (and the
    /// sessions whose group it changed). One change at a time.
    pub async fn change_layout(
        &self,
        f: impl FnOnce(&Layout) -> Result<Layout, DeskError>,
    ) -> Result<Layout, DeskError> {
        let _one = self.0.layout_write.lock().await;
        let before = self.layout();
        let after = f(&before)?.checked()?;
        if after == before {
            return Ok(after);
        }
        self.0.store.put_layout(&after).await?;
        self.0.state.lock().layout = after.clone();
        self.announce(DeskEvent::Layout {
            layout: after.clone(),
        });
        let ids: Vec<Uuid> = self.0.state.lock().records.keys().copied().collect();
        for id in ids {
            let item = session_item(id);
            let moved = before.group_of(&item).map(|g| (&g.id, &g.name))
                != after.group_of(&item).map(|g| (&g.id, &g.name));
            if moved {
                self.publish(id);
            }
        }
        Ok(after)
    }

    /// Sets (or, with `None` or a blank name, clears) the person's name for
    /// a session.
    pub fn name_session(&self, id: Uuid, name: Option<String>) -> Result<SessionView, DeskError> {
        self.update(
            id,
            SessionPatch {
                display_name: Some(name),
                ..SessionPatch::default()
            },
        )
    }

    /// Desk's first push of its local names and groups: taken when the hub
    /// has none yet (or `force`), else the hub's kept. Repeating it changes
    /// nothing.
    pub async fn import_layout(&self, import: Import) -> Result<Imported, DeskError> {
        let fresh = self.0.store.layout_is_empty().await?;
        let layout = import.layout.checked()?;
        let imported = fresh || import.force;
        if imported {
            self.change_layout(|_| Ok(layout)).await?;
            for (id, name) in import.session_names {
                let Ok(id) = Uuid::parse_str(&id) else {
                    continue;
                };
                if self.view(id).is_ok() {
                    self.name_session(id, Some(name))?;
                }
            }
        }
        let session_names = self
            .list()
            .into_iter()
            .filter_map(|v| Some((v.id.to_string(), v.display_name?)))
            .collect();
        Ok(Imported {
            imported,
            layout: self.layout(),
            session_names,
        })
    }

    /// Sessions the words in `query` pick out, best first: matched against
    /// the person's name for each, the hub's, its folder, its group and its
    /// goal, case-insensitively and on parts of words ("ledger", "the
    /// Atlas sessions"). Every session for an empty query.
    pub fn find_sessions(&self, query: &str) -> Vec<SessionView> {
        const FILLER: [&str; 16] = [
            "the", "a", "an", "my", "our", "of", "in", "on", "for", "and", "to", "all", "any",
            "session", "sessions", "group",
        ];
        let words: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
            .map(str::to_lowercase)
            .filter(|w| !w.is_empty() && !FILLER.contains(&w.as_str()))
            .collect();
        let all = self.list();
        if words.is_empty() {
            return all;
        }
        let mut scored: Vec<(usize, SessionView)> = all
            .into_iter()
            .filter_map(|v| {
                let text = v.search_text();
                let hits = words.iter().filter(|w| text.contains(w.as_str())).count();
                (hits > 0).then_some((hits, v))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, v)| v).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeskEvent;

    fn s(n: u128) -> String {
        session_item(Uuid::from_u128(n))
    }

    #[test]
    fn groups_move_their_members_as_desk_moves_them() {
        let l = Layout::default()
            .create("g1", " Atlas ", &[s(1), s(2)], None)
            .unwrap()
            .create("g2", "Infra", &["agent:local/scout".into()], Some("g1"))
            .unwrap();
        assert_eq!(
            l.groups.iter().map(|g| g.id.as_str()).collect::<Vec<_>>(),
            ["g2", "g1"]
        );
        assert_eq!(l.groups[1].name, "Atlas");
        assert!(l.create("g1", "again", &[], None).is_err());

        let l = l.put(&s(3), "g1", Some(&s(2))).unwrap();
        assert_eq!(l.groups[1].items, [s(1), s(3), s(2)]);
        let l = l
            .edit("g1", None, Some(true))
            .unwrap()
            .put(&s(1), "g2", None)
            .unwrap();
        assert_eq!(l.group_of(&s(1)).unwrap().id, "g2");
        assert_eq!(l.groups[1].items, [s(3), s(2)]);
        let l = l
            .edit("g1", None, Some(true))
            .unwrap()
            .put(&s(2), "g1", None)
            .unwrap();
        assert!(!l.groups[1].collapsed, "moving in opens it");

        let l = l.remove(&s(3)).unwrap();
        assert_eq!(l.ungrouped.items, [s(3)]);
        let l = l.move_group("g2", None).unwrap();
        assert_eq!(l.groups[1].id, "g2");
        let l = l.delete("g1").unwrap();
        assert_eq!(l.ungrouped.items, [s(3), s(2)]);
        assert!(l.delete(UNGROUPED).is_err());
        assert!(l.edit(UNGROUPED, Some("x"), None).is_err());
        assert!(l.put("nonsense", "g2", None).is_err());
        assert!(l.put(&s(9), "nowhere", None).is_err());
    }

    #[test]
    fn a_layout_is_tidied_and_checked() {
        let l = Layout {
            groups: vec![
                Group {
                    id: "a".into(),
                    name: " A ".into(),
                    collapsed: false,
                    items: vec![s(1), s(1)],
                },
                Group {
                    id: "b".into(),
                    name: "B".into(),
                    collapsed: true,
                    items: vec![s(1), s(2)],
                },
            ],
            ungrouped: Ungrouped {
                collapsed: false,
                items: vec![s(2), s(3)],
            },
        }
        .checked()
        .unwrap();
        assert_eq!(l.groups[0].items, [s(1)]);
        assert_eq!(l.groups[1].items, [s(2)]);
        assert_eq!(l.ungrouped.items, [s(3)]);
        assert_eq!(l.groups[0].name, "A");
        let dup = Layout {
            groups: vec![
                Group {
                    id: "a".into(),
                    name: "A".into(),
                    ..Group::default()
                };
                2
            ],
            ..Layout::default()
        };
        assert!(dup.checked().is_err());
        let wire: Layout = serde_json::from_value(serde_json::json!({
            "groups": [{ "id": "g", "name": "Atlas", "collapsed": true, "items": ["agent:dw/allan"] }],
            "ungrouped": { "collapsed": false, "items": [] }
        }))
        .unwrap();
        assert_eq!(wire.groups[0].items, ["agent:dw/allan"]);
    }

    async fn desk_with(
        pool: workforce_postgres::PostgresPool,
        dir: &std::path::Path,
    ) -> (Desk, Uuid) {
        DeskStore::migrate(&pool).await.unwrap();
        let store = DeskStore::from_pool(pool);
        let mut r = crate::store::tests::record_fixture("monorepo-fd");
        r.cwd = "/Users/k/dev/monorepo".into();
        r.state = crate::model::SessionState::Stopped;
        store.put(&r).await.unwrap();
        let desk = Desk::start(
            store,
            crate::testing::options(dir),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        (desk, r.id)
    }

    /// The person renamed monorepo-fd "Ledger migration" in a group they
    /// called "Atlas": every agent finds it by those words and lists it
    /// under Atlas with both names.
    #[tokio::test]
    async fn a_renamed_grouped_session_is_found_and_described_by_what_the_person_calls_it() {
        use crate::crew::{Crew, CrewAgent, Overseer};
        struct NoCrew;
        #[async_trait::async_trait]
        impl Crew for NoCrew {
            fn overseer(&self) -> Option<Overseer> {
                None
            }
            fn agents(&self) -> Vec<CrewAgent> {
                vec![CrewAgent {
                    id: "scout".into(),
                    name: "Scout".into(),
                    live: None,
                }]
            }
            fn midnight(&self) -> chrono::DateTime<Utc> {
                Utc::now()
            }
            async fn accept(&self, _: &crate::crew::Suggestion) -> Result<(), String> {
                Ok(())
            }
        }
        workforce_postgres::test_support::with_test_pool(
            crate::store::build_pool,
            |pool| async move {
                let dir = tempfile::tempdir().unwrap();
                let (d, id) = desk_with(pool.clone(), dir.path()).await;
                let (_, mut events) = d.subscribe();
                assert!(d.find_sessions("ledger").is_empty());

                d.name_session(id, Some(" Ledger migration ".into()))
                    .unwrap();
                d.change_layout(|l| {
                    l.create(
                        "g-atlas",
                        "Atlas",
                        &[session_item(id), local_agent_item("scout")],
                        None,
                    )
                })
                .await
                .unwrap();
                let v = d.view(id).unwrap();
                assert_eq!(
                    (
                        v.label(),
                        v.name.as_str(),
                        v.group.as_ref().map(|g| g.name.as_str())
                    ),
                    ("Ledger migration", "monorepo-fd", Some("Atlas"))
                );
                for query in [
                    "ledger",
                    "LEDGER migration",
                    "the ledger migration",
                    "the Atlas sessions",
                    "monorepo",
                    "atl",
                ] {
                    let found: Vec<_> = d.find_sessions(query).into_iter().map(|v| v.id).collect();
                    assert_eq!(found, vec![id], "{query:?}");
                }
                assert!(d.find_sessions("payments").is_empty());

                let o = d.overview(&NoCrew).await.unwrap();
                assert_eq!(o.groups, vec!["Atlas".to_string()]);
                let item = o.items.iter().find(|i| i.id == id.to_string()).unwrap();
                assert_eq!(
                    (
                        item.name.as_str(),
                        item.hub_name.as_deref(),
                        item.group.as_deref()
                    ),
                    ("Ledger migration", Some("monorepo-fd"), Some("Atlas"))
                );
                let scout = o.items.iter().find(|i| i.id == "scout").unwrap();
                assert_eq!(scout.group.as_deref(), Some("Atlas"));

                let mut heard_layout = false;
                let mut heard_group = false;
                while let Ok(e) = events.try_recv() {
                    match e {
                        DeskEvent::Layout { layout } => {
                            heard_layout |= layout.groups[0].name == "Atlas"
                        }
                        DeskEvent::Session { session } => heard_group |= session.group.is_some(),
                        _ => {}
                    }
                }
                assert!(heard_layout && heard_group, "changes go out live");

                // Kept: a restarted hub has them.
                let again = Desk::start(
                    DeskStore::from_pool(pool),
                    crate::testing::options(dir.path()),
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let v = again.view(id).unwrap();
                assert_eq!(v.display_name.as_deref(), Some("Ledger migration"));
                assert_eq!(again.layout(), d.layout());

                // Cleared, the hub's name is what it is called.
                d.name_session(id, None).unwrap();
                assert_eq!(d.view(id).unwrap().label(), "monorepo-fd");
            },
        )
        .await;
    }

    #[tokio::test]
    async fn desks_first_push_is_taken_once_and_repeating_it_changes_nothing() {
        workforce_postgres::test_support::with_test_pool(
            crate::store::build_pool,
            |pool| async move {
                let dir = tempfile::tempdir().unwrap();
                let (d, id) = desk_with(pool, dir.path()).await;
                let push = |name: &str| Import {
                    layout: serde_json::from_value(serde_json::json!({
                        "groups": [{ "id": "g1", "name": name, "collapsed": true,
                            "items": [session_item(id), "agent:dw/allan"] }],
                        "ungrouped": { "collapsed": false, "items": [] }
                    }))
                    .unwrap(),
                    session_names: BTreeMap::from([
                        (id.to_string(), "Ledger migration".to_string()),
                        (Uuid::new_v4().to_string(), "gone".to_string()),
                    ]),
                    force: false,
                };
                let first = d.import_layout(push("Atlas")).await.unwrap();
                assert!(first.imported);
                assert_eq!(
                    first.layout.groups[0].items,
                    [session_item(id), "agent:dw/allan".to_string()]
                );
                assert_eq!(
                    first.session_names,
                    BTreeMap::from([(id.to_string(), "Ledger migration".to_string())])
                );
                let second = d.import_layout(push("Other")).await.unwrap();
                assert!(!second.imported, "the hub's are kept");
                assert_eq!(second.layout.groups[0].name, "Atlas");
                let forced = d
                    .import_layout(Import {
                        force: true,
                        ..push("Other")
                    })
                    .await
                    .unwrap();
                assert!(forced.imported);
                assert_eq!(forced.layout.groups[0].name, "Other");
            },
        )
        .await;
    }
}
