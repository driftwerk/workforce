//! An agent's Connectors: which of the person's connections it uses, per
//! service, whether each takes writes, and which it uses when a request
//! doesn't say.
//!
//! - `GET /api/desk/agents/{agent}/connections`: every service, the
//!   connections the agent can reach (the person's for all their agents, and
//!   its own), and which it uses.
//! - `PUT /api/desk/agents/{agent}/connections` `{service, connections:
//!   [{id, write}], default?}`: the ones it uses of that service.
//!
//! What it uses is its `[skills]` (AC9a's grammar, with
//! `[skills.connections]`), written only through the hub's one write of its
//! `config.toml`, which loads the connector again as it changes.

use serde::{Deserialize, Serialize};
use toml_edit::{Array, DocumentMut, Item, Table, Value as TomlValue};
use workforce_common::connector_binding::{CONNECTORS, READ_ONLY_EACH};

use super::*;

/// One connection, as an agent's Connectors shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pick {
    pub id: String,
    pub name: String,
    pub site: Option<String>,
    pub account: Option<String>,
    /// `all`: the person's for all their agents; `agent`: this agent's own.
    pub level: &'static str,
    pub status: Status,
    /// The agent uses it.
    pub picked: bool,
    /// It takes the agent's writes.
    pub write: bool,
    /// The one a request that doesn't say goes to.
    pub default: bool,
}

/// One service, as an agent's Connectors shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServicePicks {
    pub id: &'static str,
    pub name: &'static str,
    /// It holds several connections.
    pub several: bool,
    /// Each connection takes writes or not on its own; otherwise the
    /// service does, all together.
    pub read_only_each: bool,
    /// The agent's connections of it are chosen one by one, rather than
    /// following the levels it reads from.
    pub chosen: bool,
    pub connections: Vec<Pick>,
}

/// The connections of one service an agent is to use: each instance, and
/// whether it takes writes; `default` first. None turns the service off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub service: &'static str,
    pub picked: Vec<(String, bool)>,
    pub default: Option<String>,
}

impl Choice {
    /// `doc`'s `[skills]` with this choice made.
    pub fn apply(&self, doc: &mut DocumentMut) -> Result<(), String> {
        choose(
            doc,
            self.service,
            &self.picked,
            self.default.as_deref(),
            READ_ONLY_EACH.contains(&self.service),
        )
    }
}

/// One connection the agent is to use.
#[derive(Debug, Clone, Deserialize)]
pub struct Use {
    pub id: String,
    #[serde(default = "yes")]
    pub write: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct PicksBody {
    pub service: String,
    #[serde(default)]
    pub connections: Vec<Use>,
    #[serde(default)]
    pub default: Option<String>,
}

impl Connections {
    fn agent_named(&self, agent: &str) -> Result<Agent, Refusal> {
        self.agents
            .get(agent)
            .ok_or_else(|| Refusal::NotFound(format!("There's no agent {agent}")))
    }

    /// Every connection of `c` the agent `agent` could use: the person's
    /// for all their agents, and its own.
    fn reachable(&self, c: &Connector, agent: &str, person: &str) -> Vec<(Conn, VaultSource)> {
        [Level::All, Level::Agent(agent.to_string())]
            .into_iter()
            .flat_map(|level| {
                self.instances(c, &level, person)
                    .into_iter()
                    .map(move |(key, tier)| {
                        (
                            Conn {
                                level: level.clone(),
                                key,
                            },
                            tier,
                        )
                    })
            })
            .collect()
    }

    /// Every service, and the connections the agent `agent` uses of each.
    pub async fn agent_picks(
        &self,
        person: &str,
        agent: &str,
    ) -> Result<Vec<ServicePicks>, Refusal> {
        let a = self.agent_named(agent)?;
        self.migrate(person).await;
        let checks = self.statuses.all(person).await.unwrap_or_default();
        let agents = self.agents.all();
        Ok(CATALOG
            .iter()
            .map(|c| {
                let reachable = self.reachable(c, agent, person);
                let picked: Vec<&str> = reachable
                    .iter()
                    .filter(|(conn, _)| a.binding.reaches(agent, c.id, &conn.level, &conn.key))
                    .map(|(conn, _)| conn.key.as_str())
                    .collect();
                // The one a request that doesn't say goes to: the one it
                // names first, else the adapters' rule (`default`, else the
                // only one).
                let default = a
                    .binding
                    .picks(c.id)
                    .and_then(|p| p.first().map(String::as_str))
                    .or_else(|| picked.iter().copied().find(|k| *k == DEFAULT_INSTANCE))
                    .or_else(|| (picked.len() == 1).then(|| picked[0]));
                let connections = reachable
                    .iter()
                    .map(|(conn, tier)| {
                        let checked = checks.iter().find(|k| {
                            k.connector == c.id && k.level == conn.level && k.instance == conn.key
                        });
                        let v = self.view(c, conn, *tier, person, checked, &agents);
                        let picked = picked.contains(&conn.key.as_str());
                        Pick {
                            id: v.id,
                            name: v.name,
                            site: v.site,
                            account: v.account,
                            level: v.level,
                            status: v.status,
                            picked,
                            write: !a.binding.reads_only(c.id, &conn.key),
                            default: picked && default == Some(conn.key.as_str()),
                        }
                    })
                    .collect();
                ServicePicks {
                    id: c.id,
                    name: c.name,
                    several: c.named(),
                    read_only_each: READ_ONLY_EACH.contains(&c.id),
                    chosen: a.binding.picks(c.id).is_some(),
                    connections,
                }
            })
            .collect())
    }

    /// Sets which connections of `body.service` the agent `agent` uses.
    pub async fn set_picks(
        &self,
        person: &str,
        agent: &str,
        body: PicksBody,
    ) -> Result<Vec<ServicePicks>, Refusal> {
        self.agent_named(agent)?;
        let c = Self::connector(&body.service)?;
        if !c.named() {
            return Err(Refusal::Invalid(format!(
                "{} has one connection: turn it on or off for the agent",
                c.name
            )));
        }
        self.migrate(person).await;
        let mut picked: Vec<(String, bool)> = Vec::new();
        for u in &body.connections {
            let conn = self.conn(&u.id)?;
            if matches!(&conn.level, Level::Agent(a) if a != agent) {
                return Err(Refusal::Invalid(format!(
                    "That {} connection is another agent's own. Connect one for this agent, or for all your agents.",
                    c.name
                )));
            }
            if self.held_at(c, &conn.level, &conn.key, person).is_none() {
                return Err(Refusal::NotFound(format!(
                    "That {} connection isn't there any more. Reload Connections.",
                    c.name
                )));
            }
            if !picked.iter().any(|(k, _)| *k == conn.key) {
                picked.push((conn.key, u.write));
            }
        }
        let default = match body.default.as_deref() {
            Some(id) => {
                let key = self.conn(id)?.key;
                if !picked.iter().any(|(k, _)| *k == key) {
                    return Err(Refusal::Invalid(
                        "The default has to be one of the connections it uses".into(),
                    ));
                }
                Some(key)
            }
            None => None,
        };
        let choice = Choice {
            service: c.id,
            picked,
            default,
        };
        self.agents
            .choose(agent, person, &choice)
            .await
            .map_err(Refusal::Invalid)?;
        self.agent_picks(person, agent).await
    }

    /// Takes instance `key` of `c` out of what every agent uses, once it is
    /// disconnected and none of that name is left for it.
    pub(super) async fn unpick(&self, c: &Connector, key: &str, person: &str) {
        for a in self.agents.all() {
            let Some(picks) = a.binding.picks(c.id) else {
                continue;
            };
            if !picks.iter().any(|p| p == key) {
                continue;
            }
            let still = self
                .reachable(c, &a.id, person)
                .iter()
                .any(|(conn, _)| conn.key == key);
            if still {
                continue;
            }
            let left: Vec<(String, bool)> = picks
                .iter()
                .filter(|p| *p != key)
                .map(|p| (p.clone(), !a.binding.reads_only(c.id, p)))
                .collect();
            let choice = Choice {
                service: c.id,
                default: left.first().map(|(k, _)| k.clone()),
                picked: left,
            };
            if let Err(e) = self.agents.choose(&a.id, person, &choice).await {
                warn!(agent = %a.id, connector = c.id, error = %e, "a disconnected connection is still named in an agent's connections");
            }
        }
    }
}

fn strings(item: Option<&Item>) -> Vec<String> {
    item.and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn array(list: &[String]) -> Item {
    let mut a = Array::new();
    for s in list {
        a.push(s.as_str());
    }
    Item::Value(TomlValue::Array(a))
}

/// `doc`'s `[skills]` with `service` using `picked` (each instance, and
/// whether it takes writes), `default` first. None picked turns it off.
/// Everything else in the file stays as it was.
pub fn choose(
    doc: &mut DocumentMut,
    service: &str,
    picked: &[(String, bool)],
    default: Option<&str>,
    read_only_each: bool,
) -> Result<(), String> {
    let wrong = || {
        "Their settings file keeps its connectors in a way Desk can't change. Fix it in Settings → This computer, or ask the agent."
            .to_string()
    };
    let skills = doc
        .entry("skills")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(wrong)?;
    // An agent nobody has chosen connectors for has every one: choosing one
    // changes nothing about the rest.
    let mut enabled = match skills.get("enabled") {
        None => CONNECTORS.iter().map(|c| c.to_string()).collect(),
        Some(item) if item.is_array() => strings(Some(item)),
        Some(_) => return Err(wrong()),
    };
    enabled.retain(|c| c.as_str() != service);
    if !picked.is_empty() {
        enabled.push(service.to_string());
    }
    enabled.sort();
    skills.insert("enabled", array(&enabled));

    // The connections it names decide; whose tier it reads no longer does.
    let mut own = strings(skills.get("own_credential"));
    own.retain(|c| c.as_str() != service);
    match own.is_empty() {
        true => {
            skills.remove("own_credential");
        }
        false => {
            skills.insert("own_credential", array(&own));
        }
    }

    let mut read_only = strings(skills.get("read_only"));
    read_only.retain(|r| r.as_str() != service && !r.starts_with(&format!("{service}:")));
    let reading: Vec<&str> = picked
        .iter()
        .filter(|(_, write)| !write)
        .map(|(k, _)| k.as_str())
        .collect();
    if !picked.is_empty() && reading.len() == picked.len() {
        read_only.push(service.to_string());
    } else if read_only_each {
        read_only.extend(reading.iter().map(|k| format!("{service}:{k}")));
    } else if !reading.is_empty() {
        return Err(format!(
            "{service}'s connections take writes all together or not at all"
        ));
    }
    match read_only.is_empty() {
        true => {
            skills.remove("read_only");
        }
        false => {
            skills.insert("read_only", array(&read_only));
        }
    }

    let connections = skills
        .entry("connections")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(false);
            Item::Table(t)
        })
        .as_table_mut()
        .ok_or_else(wrong)?;
    match picked.is_empty() {
        true => {
            connections.remove(service);
        }
        false => {
            let mut keys: Vec<String> = Vec::new();
            if let Some(d) = default {
                keys.push(d.to_string());
            }
            for (k, _) in picked {
                if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
            connections.insert(service, array(&keys));
        }
    }
    if connections.is_empty() {
        skills.remove("connections");
    }
    Ok(())
}

pub(super) async fn get_picks(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(agent): Path<String>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .agent_picks(&person, &agent)
        .await
        .map(|services| Json(json!({ "services": services })))
        .into_response()
}

pub(super) async fn put_picks(
    State(r): S,
    h: HeaderMap,
    Query(q): Query<TokenQuery>,
    Path(agent): Path<String>,
    Json(body): Json<PicksBody>,
) -> Response {
    let person = person!(r, h, q);
    r.connections
        .set_picks(&person, &agent, body)
        .await
        .map(|services| Json(json!({ "services": services })))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chosen(
        body: &str,
        service: &str,
        picked: &[(&str, bool)],
        default: Option<&str>,
    ) -> Result<String, String> {
        let mut doc: DocumentMut = body.parse().unwrap();
        let picked: Vec<(String, bool)> = picked.iter().map(|(k, w)| (k.to_string(), *w)).collect();
        choose(
            &mut doc,
            service,
            &picked,
            default,
            READ_ONLY_EACH.contains(&service),
        )?;
        Ok(doc.to_string())
    }

    fn binding(body: &str) -> workforce_common::connector_binding::ConnectorBinding {
        let doc: toml::Table = body.parse().unwrap();
        workforce_common::connector_binding::of(doc.get("skills")).unwrap()
    }

    #[test]
    fn a_first_choice_keeps_every_other_connector_and_names_the_connections() {
        let body = chosen(
            "name = \"Allan\"\n",
            "jira",
            &[("globex", false), ("acme", true)],
            Some("acme"),
        )
        .unwrap();
        assert!(body.starts_with("name = \"Allan\"\n"), "{body}");
        let b = binding(&body);
        assert_eq!(b.enabled.as_ref().unwrap().len(), CONNECTORS.len());
        assert_eq!(b.picks("jira").unwrap(), ["acme", "globex"]);
        assert!(b.reads_only_instance("jira", "globex"));
        assert!(!b.reads_only_instance("jira", "acme"));
        assert!(!b.reads_only("jira"));
    }

    #[test]
    fn choosing_again_replaces_only_that_service() {
        let first = chosen(
            "[skills]\nenabled = [\"linear\"]\nown_credential = [\"jira\", \"linear\"]\nread_only = [\"github\", \"jira:old\"]\n",
            "jira",
            &[("acme", false)],
            None,
        )
        .unwrap();
        let b = binding(&first);
        assert_eq!(b.enabled.as_deref().unwrap(), ["jira", "linear"]);
        assert_eq!(b.own_credential, ["linear"]);
        // All it uses only reads: the service does.
        assert_eq!(b.read_only, ["github", "jira"]);
        // None: off, and nothing of it left.
        let off = chosen(&first, "jira", &[], None).unwrap();
        let b = binding(&off);
        assert!(!b.is_on("jira"));
        assert_eq!(b.read_only, ["github"]);
        assert!(b.picks("jira").is_none());
        assert!(!off.contains("[skills.connections]"), "{off}");
    }

    #[test]
    fn email_takes_writes_all_together() {
        assert!(
            chosen("", "email", &[("work", true), ("home", false)], None)
                .unwrap_err()
                .contains("all together")
        );
        let all = chosen("", "email", &[("work", false), ("home", false)], None).unwrap();
        assert!(binding(&all).reads_only("email"));
    }

    #[test]
    fn a_file_desk_cant_change_is_said_so() {
        assert!(chosen("skills = 1\n", "jira", &[("acme", true)], None)
            .unwrap_err()
            .contains("can't change"));
    }
}
