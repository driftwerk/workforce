//! The one way an agent's workspace `config.toml` is replaced, whoever asks:
//! the config API, a revision restore, or the agent's own `system.config`
//! tool. Each write is checked against the body its writer read, admitted
//! only if the agent would load it, recorded as a revision before it lands,
//! and hot-applied to the running agent.
//!
//! Where the file is guarded (a hub running Desk, whose agents' commands run
//! as the hub), a change made to it outside the hub is held: it is not
//! applied, no write here is laid over it, and the person uses it or keeps
//! the saved file ([`changed_elsewhere`], [`use_changed`], [`keep_saved`]).

use std::path::Path;

use workforce_common::config_files::{self, Over};

use workforce_orchestrator::workflow_store::{
    content_hash, EditContext, WorkflowStore, KIND_AGENT_CONFIG,
};

use crate::serve::AgentService;
use crate::subscription::Finding;
use crate::workspace_admission::ConfigRefusal;

/// Why a config write did not land. The file and its revision history are as
/// they were in every case.
#[derive(Debug)]
pub enum Refused {
    /// The file is no longer the one the writer read: someone saved since.
    Stale { current_hash: String },
    /// The agent would not load the body.
    Unloadable(ConfigRefusal),
    /// History could not record the write, so it was not made.
    Revision(String),
    /// The file holds a change made outside the hub, which waits on the
    /// person: nothing is saved over it until they decide.
    ChangedElsewhere,
    /// The file could not be read or replaced.
    Io(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stale { .. } => {
                f.write_str("config.toml changed while this edit was made; get it again and retry")
            }
            Self::Unloadable(refusal) => write!(f, "the agent would not load that: {refusal}"),
            Self::Revision(e) => write!(f, "not saved: its history could not record it ({e})"),
            Self::ChangedElsewhere => f.write_str(config_files::CHANGED_ELSEWHERE),
            Self::Io(e) => write!(f, "not saved: {e}"),
        }
    }
}

/// A write that landed.
#[derive(Debug)]
pub struct Saved {
    /// What admission noted about the body, none of it refusing.
    pub findings: Vec<Finding>,
    /// Fields that change only once the agent restarts.
    pub restart_required: Vec<String>,
    /// The content hash of the body now on disk.
    pub hash: String,
}

/// Replace `service`'s config.toml with `body`, recorded in history under
/// `ref_id` (the agent's id), provided `expects` accepts the
/// file it replaces (`None`: there is none), both before the revision is
/// recorded and again under the lock every writer holds as it lands.
///
/// A body that makes the agent the crew's overseer switches the overseer
/// before it off, in its own config.toml: one agent oversees at a time.
pub async fn save(
    service: &AgentService,
    ref_id: &str,
    store: Option<&WorkflowStore>,
    body: &str,
    edit: EditContext<'_>,
    expects: impl Fn(Option<&str>) -> bool,
) -> Result<Saved, Refused> {
    save_over(service, ref_id, store, body, edit, expects, Over::Ours).await
}

/// [`save`], over the change made elsewhere `over` names too.
async fn save_over(
    service: &AgentService,
    ref_id: &str,
    store: Option<&WorkflowStore>,
    body: &str,
    edit: EditContext<'_>,
    expects: impl Fn(Option<&str>) -> bool,
    over: Over<'_>,
) -> Result<Saved, Refused> {
    let was = config_files::read(&service.workspace.join(config_files::WORKSPACE_CONFIG))
        .ok()
        .flatten();
    #[cfg(feature = "postgres")]
    if let Some(why) =
        crate::desk_crew::oversight_refused(service, was.as_deref(), body, edit.edited_via)
    {
        return Err(Refused::Unloadable(ConfigRefusal::Said(why)));
    }
    // A group getting a boss is closed to the main overseer before the file
    // is written, so nothing reaches it in between; opened again if the
    // write doesn't happen and no file names it.
    #[cfg(feature = "postgres")]
    let closed = crate::desk_crew::closing(service, body);
    let before = crate::connector_pins::binding_in(&service.workspace);
    let saved = save_one(service, ref_id, store, body, edit, expects, over).await;
    #[cfg(feature = "postgres")]
    if saved.is_err() {
        if let Some(g) = &closed {
            crate::desk_crew::not_closed(service, g).await;
        }
    }
    let saved = saved?;
    if service.config.desk.enabled {
        let after = crate::connector_pins::binding_in(&service.workspace);
        crate::connector_pins::rebind(service, &before, &after).await;
    }
    if oversees(body) {
        one_overseer(service, store).await;
    }
    #[cfg(feature = "postgres")]
    {
        crate::desk_crew::oversight_saved(service, was.as_deref(), body).await;
        if closed.is_some() {
            crate::desk_crew::keep_closed(service).await;
        }
        crate::desk_crew::config_saved(service);
    }
    #[cfg(not(feature = "postgres"))]
    let _ = was;
    Ok(saved)
}

/// Whether a config.toml body makes its agent the crew's overseer.
pub(crate) fn oversees(body: &str) -> bool {
    overseen(body).is_some()
}

/// The scope a config.toml body makes its agent oversee, if it does.
pub(crate) fn overseen(body: &str) -> Option<workforce_common::oversight::Scope> {
    workforce_common::settings::AgentWorkspaceConfig::parse(body)
        .ok()
        .and_then(|c| c.oversight().ok())
        .filter(|o| o.enabled)
        .map(|o| o.scope)
}

/// `body` with its `[oversight]` switched off, naming no group, and
/// everything else as it was, the table itself kept (an absent one reads as
/// never set, which Desk's hub would turn back on for its own agent). A
/// group any config names stays closed to everyone, so one switched off
/// names none. `None` when it doesn't parse.
pub(crate) fn oversight_off(body: &str) -> Option<String> {
    let mut doc: toml_edit::DocumentMut = body.parse().ok()?;
    match doc.get_mut("oversight")? {
        toml_edit::Item::Table(t) => {
            t.insert("enabled", toml_edit::value(false));
            t.remove("group");
        }
        toml_edit::Item::Value(toml_edit::Value::InlineTable(t)) => {
            t.insert("enabled", false.into());
            t.remove("group");
        }
        _ => return None,
    }
    Some(doc.to_string())
}

/// `body` with its `[oversight]` on and over `group` (none: everything
/// ungrouped), everything else as it was. `None` when it doesn't parse or
/// has no `[oversight]` table.
pub(crate) fn oversight_of_group(body: &str, group: Option<&str>) -> Option<String> {
    let mut doc: toml_edit::DocumentMut = body.parse().ok()?;
    match doc.get_mut("oversight")? {
        toml_edit::Item::Table(t) => {
            t.insert("enabled", toml_edit::value(true));
            match group {
                Some(g) => {
                    t.insert("group", toml_edit::value(g));
                }
                None => {
                    t.remove("group");
                }
            }
        }
        toml_edit::Item::Value(toml_edit::Value::InlineTable(t)) => {
            t.insert("enabled", true.into());
            match group {
                Some(g) => {
                    t.insert("group", g.into());
                }
                None => {
                    t.remove("group");
                }
            }
        }
        _ => return None,
    }
    Some(doc.to_string())
}

/// Before `service`'s config goes from `before` to `body`: when it stops
/// being the boss of a scope (moved to another, or switched off), what it
/// remembered with no scope's tag is tagged with the scope it leaves, so
/// nothing it learned there goes along (boss-gaps W8). A store that can't
/// refuses the change.
async fn keep_memory_to_scope(
    service: &AgentService,
    before: Option<&str>,
    body: &str,
) -> Result<(), Refused> {
    use workforce_memory::group_tagged::{Fence, GROUP_TAG_PREFIX};
    let fence = |b: Option<&str>| {
        b.and_then(|b| workforce_common::settings::AgentWorkspaceConfig::parse(b).ok())
            .and_then(|c| c.oversight().ok())
            .map_or(Fence::Open, |o| crate::memory_fence::fence_of(&o))
    };
    let Fence::Boss(left) = fence(before) else {
        return Ok(());
    };
    if fence(Some(body)) == Fence::Boss(left.clone()) {
        return Ok(());
    }
    let Some(store) = service.memory_store.as_ref() else {
        return crate::memory_fence::leave_sessions(
            service.sessions.as_ref(),
            service.agent_id.as_str(),
            &left,
        )
        .map(drop)
        .map_err(|e| Refused::Io(format!("{e:#}")));
    };
    match store
        .add_tag(&service.agent_id, &left, GROUP_TAG_PREFIX)
        .await
    {
        Ok(tagged) => {
            tracing::info!(agent_id = %service.agent_id, scope = %left, tagged = tagged.len(), "A boss's memory is kept to the scope it leaves");
            // Its conversations too: none is carried on, digested or
            // searched in the next scope.
            crate::memory_fence::leave_sessions(
                service.sessions.as_ref(),
                service.agent_id.as_str(),
                &left,
            )
            .map(drop)
            .map_err(|e| {
                Refused::Io(format!(
                    "{}'s conversations couldn't be left with the group it leaves, so its settings weren't changed: {e:#}",
                    service.agent_id
                ))
            })
        }
        Err(e) => Err(Refused::Io(format!(
            "what {} remembered couldn't be kept to the group it leaves, so its settings weren't changed: {e:#}",
            service.agent_id
        ))),
    }
}

/// The group `body` names in `[oversight] group`, switched on or not.
fn named_in(body: &str) -> Option<String> {
    let doc: toml::Table = toml::from_str(body).ok()?;
    let g = doc.get("oversight")?.get("group")?.as_str()?.trim();
    (!g.is_empty()).then(|| g.to_string())
}

/// Switches every agent but `keep` that oversees what `keep` oversees off,
/// through the same write path (history, admission, hot reload): one
/// overseer per scope, the main one and each group's boss apart.
pub(crate) async fn one_overseer(keep: &AgentService, store: Option<&WorkflowStore>) {
    let Some(registry) = keep.agent_registry.get().and_then(std::sync::Weak::upgrade) else {
        return;
    };
    let mine = workforce_common::config_files::read(
        &keep
            .workspace
            .join(workforce_common::config_files::WORKSPACE_CONFIG),
    )
    .ok()
    .flatten()
    .and_then(|body| overseen(&body));
    let Some(mine) = mine else {
        return;
    };
    for other in registry.all_services() {
        if other.agent_id == keep.agent_id {
            continue;
        }
        let path = other
            .workspace
            .join(workforce_common::config_files::WORKSPACE_CONFIG);
        let Ok(Some(current)) = workforce_common::config_files::read(&path) else {
            continue;
        };
        // Another that oversees the same, or is switched off but still
        // names the same group (which keeps it closed to everyone).
        let same = overseen(&current).as_ref() == Some(&mine)
            || matches!(&mine, workforce_common::oversight::Scope::Group(g)
                if named_in(&current).as_deref() == Some(g.as_str()));
        if !same {
            continue;
        }
        let Some(off) = oversight_off(&current) else {
            continue;
        };
        let id = other.agent_id.as_str().to_string();
        match save_one(
            &other,
            &id,
            store,
            &off,
            EditContext::new("oversight", None),
            |now| now == Some(current.as_str()),
            Over::Ours,
        )
        .await
        {
            Ok(_) => tracing::info!(
                agent_id = %id,
                overseer = %keep.agent_id,
                "Another agent oversees the crew now; this one no longer does"
            ),
            Err(e) => tracing::warn!(agent_id = %id, error = %e, "Could not switch oversight off"),
        }
    }
    // An agent the hub doesn't serve now, whose config still names the
    // group, would keep it closed to everyone: its name goes too.
    if let workforce_common::oversight::Scope::Group(g) = &mine {
        for u in unserved_beside(&registry.all_services()) {
            if let Err(e) = unname_unserved(&u, store, g).await {
                tracing::warn!(workspace = %u.workspace.display(), error = %e, "Could not take a group off an agent's config");
            }
        }
    }
}

/// An agent workspace beside the served ones that the hub doesn't serve
/// now, and the agent's id when it can be told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unserved {
    pub workspace: std::path::PathBuf,
    /// From the hub's own config when it lists an agent there; else the
    /// folder's name where the folders beside it are named after their
    /// agents (`{data_dir}/agents/{id}`); else unknown.
    pub id: Option<String>,
}

impl Unserved {
    /// What the agent is called in words: its id, else its folder's name.
    pub fn who(&self) -> String {
        self.id.clone().unwrap_or_else(|| {
            self.workspace
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
    }
}

/// Every agent workspace beside the `served` agents' (the folders they sit
/// in, as Desk's crew scans for the groups named for a boss), the served
/// ones left out, and only folders holding an agent's config, so nothing
/// else there is touched.
pub(crate) fn unserved_beside(served: &[std::sync::Arc<AgentService>]) -> Vec<Unserved> {
    let workspaces: Vec<&Path> = served.iter().map(|s| s.workspace.as_path()).collect();
    let mut dirs: Vec<&Path> = workspaces.iter().filter_map(|w| w.parent()).collect();
    dirs.sort();
    dirs.dedup();
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_dir() || workspaces.contains(&path.as_path()) || !agent_config_in(&path) {
                continue;
            }
            let id = agent_id_of(&path, served);
            out.push(Unserved {
                workspace: path,
                id,
            });
        }
    }
    out.sort_by(|a, b| a.workspace.cmp(&b.workspace));
    out
}

/// Whether `workspace` holds an agent's config.toml: a TOML table. Not
/// whether the agent would load it: one that wouldn't still keeps a group it
/// names closed (as Desk's crew reads it), so it must still be found.
fn agent_config_in(workspace: &Path) -> bool {
    let path = workspace.join(config_files::WORKSPACE_CONFIG);
    config_files::read(&path)
        .ok()
        .flatten()
        .is_some_and(|body| toml::from_str::<toml::Table>(&body).is_ok())
}

/// The agent id of the unserved workspace `path`: the hub's config's agent
/// whose workspace it is; else its folder's name, where every served agent
/// in the same folder is named after its id.
pub(crate) fn agent_id_of(path: &Path, served: &[std::sync::Arc<AgentService>]) -> Option<String> {
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    for s in served {
        let base = s
            .config_path
            .as_deref()
            .and_then(Path::parent)
            .unwrap_or(Path::new("."));
        for a in &s.config.agents {
            if let Some(w) = &a.workspace {
                let w = Path::new(w);
                let w = if w.is_absolute() {
                    w.to_path_buf()
                } else {
                    base.join(w)
                };
                if same(&w, path) {
                    return Some(a.id.clone());
                }
            }
        }
    }
    let dir = path.parent()?;
    let beside: Vec<_> = served
        .iter()
        .filter(|s| s.workspace.parent() == Some(dir))
        .collect();
    let named_after_ids = !beside.is_empty()
        && beside.iter().all(|s| {
            s.workspace
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .as_deref()
                == Some(s.agent_id.as_str())
        });
    named_after_ids
        .then(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .flatten()
}

/// Why the config at `path` can't be written now: it was changed outside
/// the hub and that change waits on the person, or it changed since `seen`
/// was read.
fn unchanged_since(path: &Path, seen: &str) -> Result<(), String> {
    if config_files::changed_elsewhere(path)
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err("it was changed outside the hub, and that change waits on you".into());
    }
    if config_files::read(path).ok().flatten().as_deref() != Some(seen) {
        return Err("it changed meanwhile".into());
    }
    Ok(())
}

/// The config of `u`, an agent the hub doesn't serve, switched off and
/// naming no group when it names `group`. Checked unchanged (and with no
/// change waiting from outside the hub) before anything is recorded, then
/// recorded as a revision under its agent id when that is known, then
/// written under the workspace's lock, only over what was read.
pub(crate) async fn unname_unserved(
    u: &Unserved,
    store: Option<&WorkflowStore>,
    group: &str,
) -> Result<bool, String> {
    use workforce_common::settings::AgentWorkspaceConfig;
    let path = u.workspace.join(config_files::WORKSPACE_CONFIG);
    let Some(current) = config_files::read(&path).map_err(|e| e.to_string())? else {
        return Ok(false);
    };
    if named_in(&current).as_deref() != Some(group) {
        return Ok(false);
    }
    let off = oversight_off(&current).ok_or_else(|| {
        format!(
            "{}'s settings file ({}) has a mistake in it. Fix its [oversight] section, or take the group out of it, and try again.",
            u.who(),
            path.display()
        )
    })?;
    AgentWorkspaceConfig::locked(&u.workspace, || unchanged_since(&path, &current))?;
    match &u.id {
        Some(id) => {
            record_revision(
                store,
                KIND_AGENT_CONFIG,
                id,
                &path,
                &off,
                EditContext::new("oversight", None),
            )
            .await?;
        }
        None => {
            tracing::warn!(workspace = %u.workspace.display(), "No agent id for this workspace: its change is not kept as a revision")
        }
    }
    AgentWorkspaceConfig::locked(&u.workspace, || {
        unchanged_since(&path, &current)?;
        config_files::replace_over(&path, off.as_bytes(), Over::Ours).map_err(|e| e.to_string())
    })?;
    tracing::info!(workspace = %u.workspace.display(), group, "A group's name taken off an agent the hub doesn't serve");
    Ok(true)
}

async fn save_one(
    service: &AgentService,
    ref_id: &str,
    store: Option<&WorkflowStore>,
    body: &str,
    edit: EditContext<'_>,
    expects: impl Fn(Option<&str>) -> bool,
    over: Over<'_>,
) -> Result<Saved, Refused> {
    use workforce_common::settings::AgentWorkspaceConfig;
    let path = service.workspace.join(config_files::WORKSPACE_CONFIG);
    // `current` is the file as the hub reads it: the saved one, while a
    // change made elsewhere waits on disk.
    let admit = |current: Option<&str>| {
        match (elsewhere(&path)?, over) {
            (None, _) => {}
            (Some(Some(now)), Over::Chosen(seen)) if now == seen => {}
            (Some(_), _) => return Err(Refused::ChangedElsewhere),
        }
        if !expects(current) {
            return Err(Refused::Stale {
                current_hash: content_hash(current.unwrap_or_default()),
            });
        }
        crate::workspace_admission::admit_workspace(service, body).map_err(Refused::Unloadable)
    };
    let current = read(&path)?;
    admit(current.as_deref())?;
    keep_memory_to_scope(service, current.as_deref(), body).await?;

    tokio::fs::create_dir_all(&service.workspace)
        .await
        .map_err(|e| Refused::Io(format!("failed to create the workspace directory: {e}")))?;
    record_revision(store, KIND_AGENT_CONFIG, ref_id, &path, body, edit)
        .await
        .map_err(Refused::Revision)?;

    let findings = AgentWorkspaceConfig::locked(&service.workspace, || {
        let findings = admit(read(&path)?.as_deref())?;
        config_files::replace_over(&path, body.as_bytes(), over).map_err(|e| {
            if config_files::is_changed_elsewhere(&e) {
                Refused::ChangedElsewhere
            } else {
                Refused::Io(format!(
                    "Failed to write workspace config '{}': {e}",
                    path.display()
                ))
            }
        })?;
        Ok(findings)
    })?;
    tracing::info!(agent_id = %service.agent_id, via = edit.edited_via, "Agent config.toml saved");

    let restart_required = service.reload_config().await.unwrap_or_else(|e| {
        tracing::warn!(agent_id = %service.agent_id, error = %e, "Config hot-reload failed after save");
        Vec::new()
    });
    Ok(Saved {
        findings,
        restart_required,
        hash: content_hash(body),
    })
}

/// The change made elsewhere the file at `path` holds, if one waits.
fn elsewhere(path: &Path) -> Result<Option<Option<String>>, Refused> {
    config_files::changed_elsewhere(path)
        .map_err(|e| Refused::Io(format!("failed to read {}: {e}", path.display())))
}

/// A change made to an agent's config.toml outside the hub, waiting on the
/// person.
#[derive(Debug, Clone, PartialEq)]
pub struct Elsewhere {
    /// The content hash of what the file holds now (of nothing, when it was
    /// removed): an answer names it, so it decides the change it was shown.
    pub hash: String,
    /// The file was removed rather than changed.
    pub removed: bool,
    /// Whether the agent would load the file as it is now.
    pub loads: bool,
    /// The top-level sections that differ from the saved file.
    pub sections: Vec<String>,
    /// For the sections that decide who the agent serves and whether it
    /// oversees the crew (`[access]`, `[oversight]`), each field that
    /// differs, as `access.mode: allowlist → open`. Credentials are left out.
    pub details: Vec<String>,
}

/// The sections whose changed fields a person is shown one by one.
const DETAILED: [&str; 3] = ["access", "oversight", "skills"];

/// What differs between the `saved` file and the file as it is `now`.
fn what_changed(saved: Option<&str>, now: Option<&str>) -> (Vec<String>, Vec<String>) {
    let table = |body: Option<&str>| {
        body.and_then(|b| toml::from_str::<toml::Table>(b).ok())
            .unwrap_or_default()
    };
    let (saved, now) = (table(saved), table(now));
    let mut sections: Vec<String> = saved
        .keys()
        .chain(now.keys())
        .filter(|key| saved.get(*key) != now.get(*key))
        .cloned()
        .collect();
    sections.sort();
    sections.dedup();
    let mut details = Vec::new();
    for section in DETAILED.iter().filter(|s| sections.iter().any(|c| c == *s)) {
        let fields = |t: &toml::Table| -> toml::Table {
            t.get(*section)
                .and_then(toml::Value::as_table)
                .cloned()
                .unwrap_or_default()
        };
        let (was, is) = (fields(&saved), fields(&now));
        let mut keys: Vec<&String> = was.keys().chain(is.keys()).collect();
        keys.sort();
        keys.dedup();
        for key in keys {
            let secret = crate::config_tool::is_secret_name(key)
                && !crate::config_tool::is_binding(&[section, key.as_str()]);
            if secret || was.get(key) == is.get(key) {
                continue;
            }
            details.push(format!(
                "{section}.{key}: {} → {}",
                shown(was.get(key)),
                shown(is.get(key))
            ));
        }
    }
    (sections, details)
}

/// A setting's value as a person reads it, kept short.
fn shown(value: Option<&toml::Value>) -> String {
    const MAX: usize = 60;
    let text = match value {
        None => return "unset".to_string(),
        Some(toml::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    match text.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// The change made to `service`'s config.toml outside the hub, if one waits.
pub fn changed_elsewhere(service: &AgentService) -> Option<Elsewhere> {
    let path = service.workspace.join(config_files::WORKSPACE_CONFIG);
    let now = match elsewhere(&path) {
        Ok(now) => now?,
        Err(e) => {
            tracing::warn!(agent_id = %service.agent_id, error = %e, "Could not read config.toml");
            return None;
        }
    };
    let saved = config_files::read(&path).ok().flatten();
    let (sections, details) = what_changed(saved.as_deref(), now.as_deref());
    Some(Elsewhere {
        hash: content_hash(now.as_deref().unwrap_or_default()),
        removed: now.is_none(),
        loads: now
            .as_deref()
            .is_none_or(|body| crate::workspace_admission::admit_workspace(service, body).is_ok()),
        sections,
        details,
    })
}

/// The agent Desk Helper keeps for itself, whose `config.toml` it edits
/// before it starts the hub (`DESK_AGENT` in desk-hubd).
const DESK_HELPER_AGENT: &str = "desk";

/// What Desk Helper adds to its agent's file before it starts the hub, each
/// only when the file says nothing of it (`ensure_rich`, `ensure_oversight`
/// in desk-hubd): every rich block on, and overseeing the crew, watching.
fn desk_helper_additions() -> [(&'static str, toml::Value); 2] {
    let rich = ["tables", "charts", "forms", "maps", "diagrams", "images"]
        .into_iter()
        .map(|kind| (kind.to_string(), toml::Value::Boolean(true)))
        .collect::<toml::Table>();
    let oversight = toml::Table::from_iter([
        ("enabled".to_string(), toml::Value::Boolean(true)),
        ("level".to_string(), toml::Value::String("watch".into())),
    ]);
    [
        ("rich", toml::Value::Table(rich)),
        ("oversight", toml::Value::Table(oversight)),
    ]
}

/// Whether `now` is the `saved` file of `agent_id` with nothing changed but
/// what Desk Helper adds to its own agent's file before boot.
fn desk_helper_made(agent_id: &str, saved: &str, now: Option<&str>) -> bool {
    if agent_id != DESK_HELPER_AGENT {
        return false;
    }
    let (Ok(saved), Some(Ok(mut now))) = (
        toml::from_str::<toml::Table>(saved),
        now.map(toml::from_str::<toml::Table>),
    ) else {
        return false;
    };
    for (section, added) in desk_helper_additions() {
        if !saved.contains_key(section) && now.get(section) == Some(&added) {
            now.remove(section);
        }
    }
    now == saved
}

/// The body of the last revision history holds of `service`'s config.toml.
async fn last_saved(service: &AgentService, store: &WorkflowStore) -> eyre::Result<Option<String>> {
    let id = service.agent_id.as_str();
    let Some(hash) = store
        .latest_revision_hash(KIND_AGENT_CONFIG, id, None)
        .await?
    else {
        return Ok(None);
    };
    store.revision_content(KIND_AGENT_CONFIG, &hash).await
}

/// Guard `service`'s config.toml as the hub boots: the file is the hub's own
/// when it is what history last saved (Desk Helper's additions before boot
/// aside), or when there is no history yet. Anything else was written while
/// the hub was not watching, and is held as a change made elsewhere, so a
/// command that rewrites the file and then stops the hub gains nothing.
/// Whether a change is held.
pub async fn guard_at_boot(service: &AgentService, store: Option<&WorkflowStore>) -> bool {
    let path = service.workspace.join(config_files::WORKSPACE_CONFIG);
    let saved = match store {
        Some(store) => last_saved(service, store).await.unwrap_or_else(|e| {
            tracing::warn!(agent_id = %service.agent_id, error = %e, "Could not read config.toml's history; adopting the file as it stands");
            None
        }),
        None => None,
    };
    let guarded = match saved {
        Some(saved) => match config_files::read(&path) {
            Ok(now) if now.as_deref() == Some(saved.as_str()) => {
                config_files::guard(&path).map(|()| false)
            }
            Ok(now) if desk_helper_made(service.agent_id.as_str(), &saved, now.as_deref()) => {
                record_own(service, store).await;
                config_files::guard(&path).map(|()| false)
            }
            Ok(_) => config_files::guard_as(&path, Some(saved)).map(|()| true),
            Err(e) => Err(e),
        },
        None => config_files::guard(&path).map(|()| false),
    };
    match guarded {
        Ok(held) => {
            if held {
                tracing::warn!(agent_id = %service.agent_id, "config.toml changed while the hub was stopped; keeping the saved settings until the person decides");
            }
            held
        }
        Err(e) => {
            tracing::warn!(agent_id = %service.agent_id, error = %e, "Could not guard config.toml");
            false
        }
    }
}

/// Record `service`'s config.toml in history when the hub wrote it without
/// a revision (`system.llm`, compaction, a report theme) and nothing made
/// elsewhere waits: the next boot then knows the file as the hub's own.
pub async fn record_own(service: &AgentService, store: Option<&WorkflowStore>) {
    let Some(store) = store else {
        return;
    };
    let path = service.workspace.join(config_files::WORKSPACE_CONFIG);
    if !matches!(elsewhere(&path), Ok(None)) {
        return;
    }
    let Ok(Some(body)) = config_files::read(&path) else {
        return;
    };
    let id = service.agent_id.as_str();
    let recorded = async {
        let latest = store
            .latest_revision_hash(KIND_AGENT_CONFIG, id, None)
            .await?;
        if latest.as_deref() != Some(content_hash(&body).as_str()) {
            store
                .record_revision(
                    KIND_AGENT_CONFIG,
                    id,
                    None,
                    &body,
                    false,
                    EditContext::new("hub", None),
                    None,
                )
                .await?;
        }
        eyre::Ok(())
    };
    if let Err(e) = recorded.await {
        tracing::warn!(agent_id = %id, error = %e, "Could not record config.toml in its history");
    }
}

/// The change made elsewhere that `hash` names, as the file holds it now:
/// [`Refused::Stale`] when none waits or the file has changed again.
fn seen(path: &Path, hash: &str) -> Result<Option<String>, Refused> {
    match elsewhere(path)? {
        Some(now) if content_hash(now.as_deref().unwrap_or_default()) == hash => Ok(now),
        _ => Err(stale(path)?),
    }
}

/// The refusal of an answer to a change no longer waiting as it was shown.
fn stale(path: &Path) -> Result<Refused, Refused> {
    Ok(Refused::Stale {
        current_hash: content_hash(read(path)?.as_deref().unwrap_or_default()),
    })
}

/// The person uses the change made to `service`'s config.toml outside the
/// hub that `hash` names: admitted, recorded and applied as any save (a
/// removed file is taken as removed, and the running config kept, as a
/// reload that finds none does).
pub async fn use_changed(
    service: &AgentService,
    store: Option<&WorkflowStore>,
    hash: &str,
    actor: Option<&str>,
) -> Result<(), Refused> {
    let path = service.workspace.join(config_files::WORKSPACE_CONFIG);
    let edit = EditContext::new("desk", actor);
    match seen(&path, hash)? {
        Some(body) => {
            let id = service.agent_id.as_str().to_string();
            save_over(
                service,
                &id,
                store,
                &body,
                edit,
                // The hub reads the saved file until the change is used:
                // `Over::Chosen` holds the write to the change as it was seen.
                |_| true,
                Over::Chosen(&body),
            )
            .await?;
        }
        None => {
            if !config_files::adopt(&path, None).map_err(|e| Refused::Io(e.to_string()))? {
                return Err(stale(&path)?);
            }
        }
    }
    tracing::info!(agent_id = %service.agent_id, "The person used a change made to config.toml outside the hub");
    Ok(())
}

/// The person keeps the saved config.toml over the change made outside the
/// hub that `hash` names: the file is put back as the hub last wrote it, and
/// the running config, which never took the change, stays.
pub fn keep_saved(service: &AgentService, hash: &str) -> Result<(), Refused> {
    let path = service.workspace.join(config_files::WORKSPACE_CONFIG);
    let now = seen(&path, hash)?;
    let restored =
        workforce_common::settings::AgentWorkspaceConfig::locked(&service.workspace, || {
            config_files::restore(&path, now.as_deref())
        })
        .map_err(|e| Refused::Io(e.to_string()))?;
    if !restored {
        return Err(stale(&path)?);
    }
    tracing::info!(agent_id = %service.agent_id, "The person kept the saved config.toml over a change made outside the hub");
    Ok(())
}

/// The file at `path` as it stands, `None` when there is none.
fn read(path: &Path) -> Result<Option<String>, Refused> {
    workforce_common::config_files::read(path)
        .map_err(|e| Refused::Io(format!("failed to read {}: {e}", path.display())))
}

/// Record `new_content` as the next revision of the file at `path`, first
/// capturing what is on disk when history has not seen it: the baseline on a
/// first capture, an out-of-band edit after that. No store: nothing to record.
pub async fn record_revision(
    store: Option<&WorkflowStore>,
    kind: &str,
    ref_id: &str,
    path: &Path,
    new_content: &str,
    edit: EditContext<'_>,
) -> Result<(), String> {
    let Some(store) = store else {
        return Ok(());
    };
    if let Ok(Some(current)) = workforce_common::config_files::read(path) {
        let latest = store
            .latest_revision_hash(kind, ref_id, None)
            .await
            .map_err(|e| e.to_string())?;
        if latest.as_deref() != Some(content_hash(&current).as_str()) {
            let via = if latest.is_none() { "seed" } else { "oob" };
            store
                .record_revision(
                    kind,
                    ref_id,
                    None,
                    &current,
                    false,
                    EditContext::new(via, None),
                    None,
                )
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    store
        .record_revision(kind, ref_id, None, new_content, false, edit, None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn with_store<F, Fut>(test: F)
    where
        F: FnOnce(std::sync::Arc<WorkflowStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move { test(WorkflowStore::new(pool).await.expect("store")).await },
        )
        .await;
    }

    fn unserved(dir: &Path, id: Option<&str>, body: &str) -> Unserved {
        let workspace = dir.join("ghost");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join(config_files::WORKSPACE_CONFIG), body).unwrap();
        Unserved {
            workspace,
            id: id.map(str::to_string),
        }
    }

    const NAMES_ACME: &str = "[oversight]\nenabled = false\ngroup = \"g-acme\"\nlevel = \"act\"\n";

    #[tokio::test]
    async fn an_unserved_agents_group_is_taken_off_as_a_revision_under_its_id() {
        with_store(|store| async move {
            let dir = tempfile::tempdir().unwrap();
            let u = unserved(dir.path(), Some("ghost"), NAMES_ACME);
            assert_eq!(
                unname_unserved(&u, Some(&store), "g-other").await,
                Ok(false)
            );
            assert_eq!(unname_unserved(&u, Some(&store), "g-acme").await, Ok(true));
            let now =
                std::fs::read_to_string(u.workspace.join(config_files::WORKSPACE_CONFIG)).unwrap();
            assert_eq!(named_in(&now), None);
            assert!(now.contains("level = \"act\""));
            let latest = store
                .latest_revision_hash(KIND_AGENT_CONFIG, "ghost", None)
                .await
                .unwrap();
            assert_eq!(latest, Some(content_hash(&now)));
        })
        .await;
    }

    #[tokio::test]
    async fn a_refused_write_to_an_unserved_agent_leaves_no_revision() {
        with_store(|store| async move {
            let dir = tempfile::tempdir().unwrap();
            let u = unserved(dir.path(), Some("ghost"), NAMES_ACME);
            let path = u.workspace.join(config_files::WORKSPACE_CONFIG);
            // The hub holds the file as it read it; a change made outside
            // it waits on the person.
            config_files::guard(&path).unwrap();
            std::fs::write(&path, format!("{NAMES_ACME}# edited by hand\n")).unwrap();
            let refused = unname_unserved(&u, Some(&store), "g-acme")
                .await
                .unwrap_err();
            assert!(refused.contains("changed outside the hub"), "{refused}");
            assert!(store
                .list_revisions(KIND_AGENT_CONFIG, "ghost", None)
                .await
                .unwrap()
                .is_empty());
            assert!(std::fs::read_to_string(&path)
                .unwrap()
                .contains("edited by hand"));
        })
        .await;
    }

    #[tokio::test]
    async fn an_unserved_agent_whose_id_isnt_known_is_written_without_a_revision() {
        with_store(|store| async move {
            let dir = tempfile::tempdir().unwrap();
            let u = unserved(dir.path(), None, NAMES_ACME);
            assert_eq!(u.who(), "ghost");
            assert_eq!(unname_unserved(&u, Some(&store), "g-acme").await, Ok(true));
            assert!(store
                .list_revisions(KIND_AGENT_CONFIG, "ghost", None)
                .await
                .unwrap()
                .is_empty());
        })
        .await;
    }

    #[test]
    fn a_change_to_the_agents_connectors_is_in_the_revision_details() {
        let (sections, details) = what_changed(
            Some("[skills]\nenabled = []\n"),
            Some("[skills]\nenabled = [\"linear\"]\nown_credential = [\"linear\"]\n"),
        );
        assert_eq!(sections, ["skills"]);
        assert!(
            details.iter().any(|d| d.starts_with("skills.enabled:")),
            "{details:?}"
        );
        assert!(
            details
                .iter()
                .any(|d| d.starts_with("skills.own_credential:")),
            "{details:?}"
        );
    }

    /// Desk Helper's own additions before boot are the hub's own, for its own
    /// agent and as it makes them; nothing else is.
    #[test]
    fn only_desk_helpers_own_additions_count_as_the_hubs() {
        let saved = "[llm]\nmax_rounds = 1\n";
        let helped =
            "[llm]\nmax_rounds = 1\n\n[rich]\ntables = true\ncharts = true\nforms = true\n\
                      maps = true\ndiagrams = true\nimages = true\n\n[oversight]\nenabled = true\n\
                      level = \"watch\"\n";
        assert!(desk_helper_made("desk", saved, Some(helped)));
        assert!(desk_helper_made(
            "desk",
            "",
            Some(&helped.replace("[llm]\nmax_rounds = 1\n\n", ""))
        ));
        assert!(
            !desk_helper_made("scout", saved, Some(helped)),
            "only Desk's own agent"
        );
        assert!(!desk_helper_made(
            "desk",
            saved,
            Some(&helped.replace("\"watch\"", "\"act\""))
        ));
        assert!(!desk_helper_made(
            "desk",
            saved,
            Some(&helped.replace("max_rounds = 1", "max_rounds = 9"))
        ));
        let had = "[oversight]\nenabled = false\n";
        assert!(
            !desk_helper_made(
                "desk",
                had,
                Some("[oversight]\nenabled = true\nlevel = \"watch\"\n")
            ),
            "Desk Helper never changes an [oversight] the file has"
        );
    }

    #[test]
    fn a_change_is_shown_by_section_and_by_field_where_it_decides_access() {
        let (sections, details) = what_changed(
            Some("[llm]\nmax_rounds = 1\n[access]\nmode = \"allowlist\"\nallow = [\"a\"]\n"),
            Some("[llm]\nmax_rounds = 2\n[access]\nmode = \"open\"\nallow = [\"a\"]\napi_key = \"x\"\n\
                  [oversight]\nenabled = true\n"),
        );
        assert_eq!(sections, ["access", "llm", "oversight"]);
        assert_eq!(
            details,
            [
                "access.mode: allowlist → open",
                "oversight.enabled: unset → true"
            ]
        );
    }

    #[test]
    fn oversight_is_switched_off_in_place_in_either_form() {
        let tables = "# mine\n[llm]\nprovider = \"chatgpt\"\n\n[oversight]\nenabled = true\nlevel = \"act\"\n\n[oversight.wake]\nquiet_minutes = 0\n";
        let off = oversight_off(tables).unwrap();
        assert!(off.starts_with("# mine\n[llm]\nprovider = \"chatgpt\"\n"));
        assert!(!oversees(&off));
        assert!(off.contains("level = \"act\""));
        assert!(off.contains("[oversight.wake]\nquiet_minutes = 0"));

        let boss = "[oversight]\nenabled = true\ngroup = \"acme\"\nlevel = \"act\"\n";
        let off = oversight_off(boss).unwrap();
        assert!(!oversees(&off));
        assert!(
            !off.contains("group"),
            "a boss switched off names no group: {off}"
        );
        assert!(off.contains("level = \"act\""));
        let off = oversight_off("oversight = { enabled = true, group = \"acme\" }\n").unwrap();
        assert!(!off.contains("group"), "{off}");

        let inline = "oversight = { enabled = true, level = \"watch\" }\n";
        assert!(oversees(inline));
        let off = oversight_off(inline).unwrap();
        assert!(!oversees(&off));
        assert!(off.contains("level = \"watch\""));

        assert_eq!(oversight_off("[llm]\n"), None);
    }
}
