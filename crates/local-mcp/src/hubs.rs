//! Registered remote workforce hubs: the on-disk layout shared by the
//! `config workforce` CLI (writer) and the MCP runtime (reader).
//!
//!   - `hubs_index`   — JSON array of hub names
//!   - `hubs/<name>`  — JSON `{"url","token"}` for one hub
//!   - `hubs/active`  — name of the hub the `wf.*` tools currently target

use std::collections::BTreeSet;

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use workforce_common::settings::SecretStore;

use crate::paths;

const TOKEN_PREFIX: &str = "wfr_";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HubEntry {
    #[serde(skip)]
    pub name: String,
    pub url: String,
    pub token: String,
}

impl std::fmt::Debug for HubEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubEntry")
            .field("name", &self.name)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl HubEntry {
    pub fn mcp_endpoint(&self) -> String {
        format!("{}/mcp", self.url)
    }
}

pub fn validate_hub_name(raw: &str) -> Result<&str> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(eyre::eyre!("hub name cannot be empty"));
    }
    if name.contains('/') || name.contains(char::is_whitespace) {
        return Err(eyre::eyre!(
            "hub name cannot contain '/' or whitespace (got {name:?})"
        ));
    }
    Ok(name)
}

/// Accepts the hub's public URL or its `/mcp` endpoint; stores the base URL
/// (scheme, host, port and any mount path) without a trailing slash.
pub fn normalise_hub_url(raw: &str) -> Result<String> {
    let parsed = url::Url::parse(raw.trim())
        .wrap_err_with(|| format!("hub URL {raw:?} is not a valid URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(eyre::eyre!(
            "hub URL must start with https:// or http:// (got {raw:?})"
        ));
    }
    if parsed.host_str().is_none() {
        return Err(eyre::eyre!("hub URL has no host (got {raw:?})"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(eyre::eyre!(
            "hub URL must not embed credentials; the token is stored separately"
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(eyre::eyre!(
            "hub URL must not have a query string or fragment (got {raw:?})"
        ));
    }
    let base = parsed.as_str().trim_end_matches('/');
    Ok(base.strip_suffix("/mcp").unwrap_or(base).to_string())
}

pub fn validate_hub_token(raw: &str) -> Result<&str> {
    let token = raw.trim();
    if !token.starts_with(TOKEN_PREFIX) || token.len() <= TOKEN_PREFIX.len() {
        return Err(eyre::eyre!(
            "that is not a remote MCP token: expected one starting with {TOKEN_PREFIX}, minted by `cmd: remote-mcp token` or the hub's Remote MCP tokens settings card"
        ));
    }
    Ok(token)
}

pub fn list_hub_names(store: &dyn SecretStore) -> Result<Vec<String>> {
    match store.get(paths::HUBS_INDEX) {
        Some(raw) => serde_json::from_str(&raw).wrap_err("parsing hubs_index"),
        None => Ok(Vec::new()),
    }
}

pub fn get_hub(store: &dyn SecretStore, name: &str) -> Result<Option<HubEntry>> {
    store
        .get(&paths::hub_entry(name))
        .map(|raw| {
            serde_json::from_str::<HubEntry>(&raw)
                .map(|entry| HubEntry {
                    name: name.to_string(),
                    ..entry
                })
                .wrap_err_with(|| format!("parsing hub entry {name}"))
        })
        .transpose()
}

pub fn list_hubs(store: &dyn SecretStore) -> Result<Vec<HubEntry>> {
    list_hub_names(store)?
        .iter()
        .filter_map(|name| get_hub(store, name).transpose())
        .collect()
}

/// Saves the hub and makes it active when it is the only one registered.
pub fn save_hub(store: &dyn SecretStore, entry: &HubEntry) -> Result<()> {
    validate_hub_name(&entry.name)?;
    let encoded = serde_json::to_string(entry).expect("HubEntry always serializes");
    store.set(&paths::hub_entry(&entry.name), &encoded)?;
    let names = with_name(list_hub_names(store)?, &entry.name, true);
    write_index(store, &names)?;
    if names.len() == 1 {
        store.set(paths::ACTIVE_HUB, &entry.name)?;
    }
    Ok(())
}

/// Removes the hub; clears the active pointer when it pointed here.
pub fn remove_hub(store: &dyn SecretStore, name: &str) -> Result<bool> {
    let removed = store.delete(&paths::hub_entry(name))?;
    let names = with_name(list_hub_names(store)?, name, false);
    write_index(store, &names)?;
    if active_hub_name(store).as_deref() == Some(name) {
        store.delete(paths::ACTIVE_HUB)?;
    }
    Ok(removed)
}

pub fn active_hub_name(store: &dyn SecretStore) -> Option<String> {
    store.get(paths::ACTIVE_HUB)
}

pub fn set_active_hub(store: &dyn SecretStore, name: &str) -> Result<()> {
    if !list_hub_names(store)?.iter().any(|n| n == name) {
        return Err(eyre::eyre!(
            "hub {name:?} is not registered; run `workforce-local-mcp config workforce add {name} --url <url>`"
        ));
    }
    store.set(paths::ACTIVE_HUB, name)
}

pub fn active_hub(store: &dyn SecretStore) -> Result<Option<HubEntry>> {
    active_hub_name(store)
        .map(|name| get_hub(store, &name))
        .transpose()
        .map(Option::flatten)
}

pub fn clear_hubs(store: &dyn SecretStore) -> Result<usize> {
    let names = list_hub_names(store).unwrap_or_default();
    let mut removed = 0;
    for name in &names {
        if store.delete(&paths::hub_entry(name))? {
            removed += 1;
        }
    }
    if store.delete(paths::HUBS_INDEX)? {
        removed += 1;
    }
    if store.delete(paths::ACTIVE_HUB)? {
        removed += 1;
    }
    Ok(removed)
}

fn with_name(names: Vec<String>, name: &str, present: bool) -> Vec<String> {
    let mut set: BTreeSet<String> = names.into_iter().collect();
    if present {
        set.insert(name.to_string());
    } else {
        set.remove(name);
    }
    set.into_iter().collect()
}

fn write_index(store: &dyn SecretStore, names: &[String]) -> Result<()> {
    if names.is_empty() {
        store.delete(paths::HUBS_INDEX).map(drop)
    } else {
        let encoded = serde_json::to_string(names).expect("Vec<String> always serializes");
        store.set(paths::HUBS_INDEX, &encoded)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use workforce_common::settings::SecretStore;

    #[derive(Default)]
    pub struct MemoryStore(Mutex<BTreeMap<String, String>>);

    impl SecretStore for MemoryStore {
        fn get(&self, path: &str) -> Option<String> {
            self.0.lock().unwrap().get(path).cloned()
        }
        fn set(&self, path: &str, value: &str) -> eyre::Result<()> {
            self.0.lock().unwrap().insert(path.into(), value.into());
            Ok(())
        }
        fn delete(&self, path: &str) -> eyre::Result<bool> {
            Ok(self.0.lock().unwrap().remove(path).is_some())
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .keys()
                .filter(|k| prefix.is_none_or(|p| k.starts_with(p)))
                .cloned()
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::MemoryStore;
    use super::*;

    fn entry(name: &str) -> HubEntry {
        HubEntry {
            name: name.into(),
            url: format!("https://{name}.example.com"),
            token: format!("wfr_{name}"),
        }
    }

    #[test]
    fn hub_name_validation() {
        assert!(validate_hub_name("").is_err());
        assert!(validate_hub_name("  ").is_err());
        assert!(validate_hub_name("a/b").is_err());
        assert!(validate_hub_name("a b").is_err());
        assert_eq!(validate_hub_name(" driftwerk ").unwrap(), "driftwerk");
    }

    #[test]
    fn hub_url_normalisation() {
        assert_eq!(
            normalise_hub_url("https://wf.example.com/").unwrap(),
            "https://wf.example.com"
        );
        assert_eq!(
            normalise_hub_url("https://wf.example.com/mcp").unwrap(),
            "https://wf.example.com"
        );
        assert_eq!(
            normalise_hub_url("http://localhost:8091").unwrap(),
            "http://localhost:8091"
        );
        assert_eq!(
            normalise_hub_url("https://wf.example.com/workforce/mcp/").unwrap(),
            "https://wf.example.com/workforce"
        );
        assert!(normalise_hub_url("wf.example.com").is_err());
        assert!(normalise_hub_url("https://").is_err());
        assert!(normalise_hub_url("ftp://wf.example.com").is_err());
        assert!(normalise_hub_url("https://user:pw@wf.example.com").is_err());
        assert!(normalise_hub_url("https://wf.example.com/mcp?x=1").is_err());
        assert!(normalise_hub_url("https://wf.example.com/#frag").is_err());
        assert_eq!(entry("x").mcp_endpoint(), "https://x.example.com/mcp");
    }

    #[test]
    fn hub_token_validation() {
        assert_eq!(validate_hub_token(" wfr_abc ").unwrap(), "wfr_abc");
        assert!(validate_hub_token("wfr_").is_err());
        assert!(validate_hub_token("wfp_abc").is_err());
        assert!(validate_hub_token("").is_err());
    }

    #[test]
    fn first_saved_hub_becomes_active_and_round_trips() {
        let store = MemoryStore::default();
        save_hub(&store, &entry("driftwerk")).unwrap();
        save_hub(&store, &entry("harbor")).unwrap();

        assert_eq!(list_hub_names(&store).unwrap(), vec!["driftwerk", "harbor"]);
        assert_eq!(active_hub_name(&store).as_deref(), Some("driftwerk"));
        assert_eq!(get_hub(&store, "harbor").unwrap(), Some(entry("harbor")));
        assert_eq!(list_hubs(&store).unwrap().len(), 2);
        assert!(get_hub(&store, "nope").unwrap().is_none());
        assert!(!store.get("hubs/harbor").unwrap().contains("\"name\""));
    }

    #[test]
    fn set_active_requires_a_registered_hub() {
        let store = MemoryStore::default();
        save_hub(&store, &entry("driftwerk")).unwrap();
        save_hub(&store, &entry("harbor")).unwrap();
        assert!(set_active_hub(&store, "contoso").is_err());
        set_active_hub(&store, "harbor").unwrap();
        assert_eq!(active_hub(&store).unwrap(), Some(entry("harbor")));
    }

    #[test]
    fn removing_the_active_hub_clears_the_pointer() {
        let store = MemoryStore::default();
        save_hub(&store, &entry("driftwerk")).unwrap();
        save_hub(&store, &entry("harbor")).unwrap();
        assert!(remove_hub(&store, "driftwerk").unwrap());
        assert!(!remove_hub(&store, "driftwerk").unwrap());
        assert_eq!(list_hub_names(&store).unwrap(), vec!["harbor"]);
        assert_eq!(active_hub_name(&store), None);
        assert!(remove_hub(&store, "harbor").unwrap());
        assert_eq!(store.get(paths::HUBS_INDEX), None);
    }

    #[test]
    fn clear_removes_entries_index_and_pointer() {
        let store = MemoryStore::default();
        save_hub(&store, &entry("driftwerk")).unwrap();
        save_hub(&store, &entry("harbor")).unwrap();
        assert_eq!(clear_hubs(&store).unwrap(), 4);
        assert!(store.list(None).is_empty());
    }
}
