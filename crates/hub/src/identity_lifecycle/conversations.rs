//! What the person said and what was learned about them: sessions,
//! memories, their files in each agent's workspace, signals, game tallies,
//! and their billing history.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use workforce_common::session::SessionStore;

use super::{Context, Holding, Owners, Person, Tally};

/// Sessions read per owner per pass; a reassign loops until none are left.
const SESSION_PAGE: usize = 100_000;

/// Where a session's coding rounds kept their Codex home: its driver
/// session ids are in its metadata under this prefix.
const DRIVER_UUID_PREFIX: &str = "driver_claude_session_uuid:";

pub struct Sessions;

impl Sessions {
    /// The sessions the person owns, a page per name, and the names whose
    /// page came back full and so may own more.
    fn owned(
        sessions: &dyn SessionStore,
        who: &Person,
    ) -> Result<(Vec<workforce_common::session::Session>, Vec<String>)> {
        let mut owned = Vec::new();
        let mut more = Vec::new();
        for name in &who.names {
            let page = sessions.list_for_owner(name, SESSION_PAGE)?;
            if page.len() >= SESSION_PAGE {
                more.push(name.clone());
            }
            owned.extend(page);
        }
        Ok((owned, more))
    }
}

/// A suspended person's turns in flight are stopped where they run,
/// through the stop a person or an administrator uses, which reaches every
/// replica on a fleet with a bus. Nothing is restarted on reactivation.
#[async_trait]
impl super::Pausable for Sessions {
    async fn suspend(&self, cx: &Context<'_>, who: &Person, apply: bool) -> Result<Tally> {
        let mut tally = Tally::default();
        for service in cx.services() {
            let sessions = service.sessions.as_ref();
            for session in Self::owned(sessions, who)?
                .0
                .into_iter()
                .filter(|session| !session.is_sealed())
            {
                if sessions.turn_claim_holder(&session.id)?.is_none() {
                    continue;
                }
                tally.item(format!("{}: {}", service.agent_id, session.channel));
                if apply {
                    crate::gateway::steer(
                        service.nats().as_ref(),
                        &service,
                        &session.id,
                        workforce_nats::chat::TurnControl::Cancel,
                    )
                    .await;
                }
            }
        }
        Ok(tally)
    }

    async fn resume(&self, _: &Context<'_>, _: &Person, _: bool) -> Result<Tally> {
        Ok(Tally::default())
    }
}

#[async_trait]
impl Holding for Sessions {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let mut owners = Owners::default();
        for service in cx.services() {
            owners.absorb(Owners::counted(service.sessions.owners()?));
        }
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let mut tally = Tally::default();
        for service in cx.services() {
            for session in Self::owned(service.sessions.as_ref(), who)?.0 {
                tally.item(format!("{}: {}", service.agent_id, session.channel));
            }
        }
        Ok(tally)
    }

    /// Both sides are kept: an open session on a channel where the identity
    /// merged into already has one moves to a channel of its own.
    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let to = into.key();
        let mut tally = Tally::default();
        for service in cx.services() {
            let sessions = service.sessions.as_ref();
            for name in from.names.iter().filter(|name| **name != to) {
                let planned = sessions
                    .list_for_owner(name, SESSION_PAGE)?
                    .into_iter()
                    .map(|session| {
                        let renamed =
                            !session.is_sealed() && sessions.find(&session.channel, &to)?.is_some();
                        Ok(workforce_common::session::Reassigned {
                            channel: match renamed {
                                true => workforce_common::session::reassigned_channel(
                                    &session.channel,
                                    &from.tag(),
                                ),
                                false => session.channel,
                            },
                            session_id: session.id,
                            renamed,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                // The files move first, each with its session, so a pass that
                // fails between them finds the sessions still to move; any
                // past the page, or moved elsewhere than planned, follow.
                let moved = match apply {
                    true => {
                        move_files(&planned, &to).await?;
                        let moved = sessions.reassign_owner(name, &to, &from.tag())?;
                        sessions.move_desk_reads(name, &to)?;
                        let unplanned: Vec<_> = moved
                            .iter()
                            .filter(|session| !planned.contains(session))
                            .cloned()
                            .collect();
                        move_files(&unplanned, &to).await?;
                        moved
                    }
                    false => planned,
                };
                for session in moved {
                    if session.renamed {
                        tally.conflicts.push(format!(
                            "{}: both had a conversation open here; theirs continues on {}",
                            service.agent_id, session.channel
                        ));
                    }
                    tally.item(format!("{}: {}", service.agent_id, session.channel));
                }
            }
        }
        Ok(tally)
    }

    /// A session with a turn in flight is left for the next pass: the turn's
    /// save would write it back. So is everything past a full page.
    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let homes =
            crate::coding_sandbox::absolutise_data_dir(cx.data_dir.clone()).join("codex-homes");
        let mut tally = Tally::default();
        for service in cx.services() {
            let sessions = service.sessions.as_ref();
            let (owned, more) = Self::owned(sessions, who)?;
            tally.blocking.extend(more.into_iter().map(|name| {
                format!(
                    "{}: {name} holds more than {SESSION_PAGE} sessions; run again for the rest",
                    service.agent_id
                )
            }));
            for session in owned {
                tally.item(format!("{}: {}", service.agent_id, session.channel));
                if !apply {
                    continue;
                }
                if sessions.turn_claim_holder(&session.id)?.is_some() {
                    tally.blocking.push(format!(
                        "{}: a turn is running in {}",
                        service.agent_id, session.channel
                    ));
                    continue;
                }
                let drivers: Vec<String> = sessions
                    .get(&session.id)?
                    .map(|full| driver_homes(&full))
                    .unwrap_or_default();
                // Its files go first: a pass that fails between the two
                // finds the session again and finishes.
                crate::desk_attachments::forget_session_files(&session.id).await?;
                sessions.delete(&session.id)?;
                for driver in drivers {
                    remove_path(&homes.join(format!(".codex-{driver}")))?;
                }
            }
            if apply {
                for name in &who.names {
                    sessions.forget_desk_reads(name)?;
                }
            }
        }
        Ok(tally)
    }
}

/// Move the files of each of `sessions` to `owner`, on its channel.
async fn move_files(sessions: &[workforce_common::session::Reassigned], owner: &str) -> Result<()> {
    for session in sessions {
        crate::desk_attachments::move_session_files(&session.session_id, owner, &session.channel)
            .await?;
    }
    Ok(())
}

/// The Codex homes a session's coding rounds kept, which hold the login of
/// the person who drove them.
fn driver_homes(session: &workforce_common::session::Session) -> Vec<String> {
    session
        .metadata
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| key.starts_with(DRIVER_UUID_PREFIX))
        .filter_map(|(_, value)| value.as_str().map(str::to_string))
        .collect()
}

pub struct Memories;

#[async_trait]
impl Holding for Memories {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let mut owners = Owners::default();
        for service in cx.services() {
            if let Some(store) = &service.memory_store {
                owners.absorb(Owners::counted(
                    store.identity_counts(&service.agent_id).await?,
                ));
            }
        }
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let mut tally = Tally::default();
        for service in cx.services() {
            if let Some(store) = &service.memory_store {
                for name in &who.names {
                    let held = store
                        .identity_memories(&service.agent_id, name)
                        .await?
                        .len();
                    if held > 0 {
                        tally.absorb(Tally {
                            count: held,
                            items: vec![format!("{}: {held}", service.agent_id)],
                            ..Tally::default()
                        });
                    }
                }
            }
        }
        Ok(tally)
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, from).await?;
        if apply {
            for service in cx.services() {
                if let Some(store) = &service.memory_store {
                    for name in from.names.iter().filter(|n| **n != into.key()) {
                        store
                            .reassign_identity(&service.agent_id, name, &into.key())
                            .await?;
                    }
                }
            }
        }
        Ok(tally)
    }

    /// Purged, forgotten ones included: forgetting only hides a memory.
    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, who).await?;
        if apply {
            for service in cx.services() {
                if let Some(store) = &service.memory_store {
                    for name in &who.names {
                        store.purge_identity(&service.agent_id, name).await?;
                    }
                }
            }
        }
        Ok(tally)
    }
}

/// The person's files in one agent's workspace: their notes directory and
/// their profile.
fn personal_paths(workspace: &Path, name: &str) -> [PathBuf; 2] {
    [
        workspace.join("memory").join("users").join(name),
        workspace.join("users").join(format!("{name}.md")),
    ]
}

pub struct PersonalFiles;

impl PersonalFiles {
    /// What the person has in `service`'s workspace, under any of their
    /// names or the profile name the agent's `user_map` gives one of them.
    fn held_in(service: &crate::serve::AgentService, who: &Person) -> Vec<PathBuf> {
        who.names
            .iter()
            .chain(
                who.names
                    .iter()
                    .filter_map(|name| service.user_map.get(name)),
            )
            .filter(|name| is_path_safe(name))
            .flat_map(|name| personal_paths(&service.workspace, name))
            .filter(|path| path.exists())
            .collect()
    }
}

/// A name that can only ever name one entry beneath the directory it is
/// joined to.
fn is_path_safe(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

impl PersonalFiles {
    /// Whom each personal file in `service`'s workspace belongs to: a notes
    /// directory or profile under a person's name, or a profile named by
    /// the agent's `user_map`, which belongs to the ids it maps from; and
    /// each consolidation cursor's unit.
    fn owners_in(service: &crate::serve::AgentService) -> Result<Owners> {
        let workspace = &service.workspace;
        let entries = |dir: PathBuf| -> Result<Vec<String>> {
            match std::fs::read_dir(dir) {
                Ok(entries) => entries
                    .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
                    .collect(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(e) => Err(e.into()),
            }
        };
        let notes = entries(workspace.join("memory").join("users"))?;
        let profiles: Vec<String> = entries(workspace.join("users"))?
            .into_iter()
            .filter_map(|file| file.strip_suffix(".md").map(str::to_string))
            .collect();
        let owners_of = |name: String| -> Vec<String> {
            let mapped: Vec<String> = service
                .user_map
                .iter()
                .filter(|(_, profile)| **profile == name)
                .map(|(id, _)| id.clone())
                .collect();
            match mapped.is_empty() {
                true => vec![name],
                false => mapped,
            }
        };
        Ok(Owners::of(
            notes
                .into_iter()
                .chain(profiles)
                .flat_map(owners_of)
                .chain(crate::memory_consolidation::cursor_units(workspace)),
        ))
    }
}

#[async_trait]
impl Holding for PersonalFiles {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let mut owners = Owners::default();
        for service in cx.services() {
            owners.absorb(Self::owners_in(&service)?);
        }
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let mut tally = Tally::default();
        for service in cx.services() {
            for path in Self::held_in(&service, who) {
                tally.item(display(&service.workspace, &path));
            }
            let cursors = crate::memory_consolidation::unit_cursors(&service.workspace, &who.names);
            if cursors > 0 {
                tally.item(format!("{}: consolidation cursor", service.agent_id));
            }
        }
        Ok(tally)
    }

    /// Union: a file only the source has moves; a note both have keeps the
    /// target's and gains the source's beneath it; any other file both have
    /// keeps the target's.
    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let to = into.key();
        let mut tally = Tally::default();
        for service in cx.services() {
            let workspace = &service.workspace;
            for name in from.names.iter().filter(|n| **n != to && is_path_safe(n)) {
                for (source, target) in personal_paths(workspace, name)
                    .into_iter()
                    .zip(personal_paths(workspace, &to))
                    .filter(|(source, _)| source.exists())
                {
                    tally.item(display(workspace, &source));
                    union_into(&source, &target, &from.tag(), apply, &mut tally.conflicts)?;
                }
            }
            if apply {
                crate::memory_consolidation::forget_units(workspace, &from.names);
            }
        }
        Ok(tally)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, who).await?;
        if apply {
            for service in cx.services() {
                for path in Self::held_in(&service, who) {
                    remove_path(&path)?;
                }
                crate::memory_consolidation::forget_units(&service.workspace, &who.names);
            }
        }
        Ok(tally)
    }
}

fn display(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Move `source` (a file or a directory) into `target` by the union rule,
/// recording what kept the target's copy.
fn union_into(
    source: &Path,
    target: &Path,
    tag: &str,
    apply: bool,
    kept: &mut Vec<String>,
) -> Result<()> {
    if !target.exists() {
        if apply {
            if let Some(parent) = target.parent() {
                workforce_common::private_fs::ensure_private_dir(parent)?;
            }
            std::fs::rename(source, target)?;
        }
        return Ok(());
    }
    if source.is_dir() && target.is_dir() {
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            union_into(
                &entry.path(),
                &target.join(entry.file_name()),
                tag,
                apply,
                kept,
            )?;
        }
    } else if source.extension().is_some_and(|e| e == "md") && target.is_file() {
        if apply {
            let theirs = std::fs::read_to_string(source)?;
            let mut file = std::fs::OpenOptions::new().append(true).open(target)?;
            std::io::Write::write_all(
                &mut file,
                format!("\n\n<!-- merged from {tag} -->\n\n{theirs}").as_bytes(),
            )?;
        }
    } else {
        kept.push(format!("{}: kept the existing copy", target.display()));
    }
    if apply {
        remove_path(source)?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    let removed = match path.is_dir() {
        true => std::fs::remove_dir_all(path),
        false => std::fs::remove_file(path),
    };
    match removed {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

pub struct GameTallies;

#[async_trait]
impl Holding for GameTallies {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        Ok(Owners::counted(
            crate::games_driver::rivalry_store(&cx.data_dir).humans(),
        ))
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Ok(Tally::counted(
            crate::games_driver::rivalry_store(&cx.data_dir).held(&who.names),
        ))
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, from).await?;
        if apply {
            crate::games_driver::rivalry_store(&cx.data_dir).merge_into(&from.names, &into.key());
        }
        Ok(tally)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, who).await?;
        if apply {
            crate::games_driver::rivalry_store(&cx.data_dir).forget(&who.names);
        }
        Ok(tally)
    }
}

/// Relationship and lesson events, the prompt excerpts on cost and task
/// rows, and the floor's log of the person's turns with their plans'
/// readings. A move renames them all, an erase removes or unnames the events,
/// clears the excerpts and removes the floor's rows; every tally is the rows
/// read or changed, by the one definition their store counts and changes
/// them by.
pub struct Signals;

impl Signals {
    fn tally(signals: workforce_common::telemetry::IdentitySignals) -> Tally {
        let mut tally = Tally::default();
        for (what, count) in [
            ("relationship events", signals.relationship_events),
            ("lesson events", signals.lesson_events),
            (
                "prompt excerpts on cost and task rows",
                signals.described_rows,
            ),
        ] {
            if count > 0 {
                tally.absorb(Tally {
                    count,
                    items: vec![format!("{what}: {count}")],
                    ..Tally::default()
                });
            }
        }
        tally
    }

    /// What the floor keeps of a person, by every name they go by: the log
    /// of their turns, and their own plans' latest readings.
    async fn floor(who: &Person) -> Result<Tally> {
        let mut tally = Tally::default();
        for name in &who.names {
            let (turns, readings) = crate::floor::held(name).await?;
            for (what, count) in [("turns on the floor", turns), ("plan readings", readings)] {
                if count > 0 {
                    tally.absorb(Tally {
                        count: usize::try_from(count).unwrap_or_default(),
                        items: vec![format!("{what}: {count}")],
                        ..Tally::default()
                    });
                }
            }
        }
        Ok(tally)
    }

    /// `each` for every name the person goes by, tallied.
    fn per_name(
        who: &Person,
        each: impl Fn(&str) -> Result<workforce_common::telemetry::IdentitySignals>,
    ) -> Result<Tally> {
        who.names
            .iter()
            .try_fold(Tally::default(), |mut tally, name| {
                tally.absorb(Self::tally(each(name)?));
                Ok(tally)
            })
    }
}

#[async_trait]
impl Holding for Signals {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let mut owners = Owners::counted(cx.hub()?.telemetry_store.identity_signal_holders()?);
        owners.absorb(Owners::counted(crate::floor::holders().await?));
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let telemetry = &cx.hub()?.telemetry_store;
        let mut tally = Self::per_name(who, |name| telemetry.identity_signals(name))?;
        tally.absorb(Self::floor(who).await?);
        Ok(tally)
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        if !apply {
            return self.held(cx, from).await;
        }
        let telemetry = &cx.hub()?.telemetry_store;
        let into = into.key();
        let mut tally = Self::per_name(from, |name| match name == into {
            true => Ok(Default::default()),
            false => telemetry.reassign_identity_signals(name, &into),
        })?;
        tally.absorb(Self::floor(from).await?);
        for name in from.names.iter().filter(|name| **name != into) {
            crate::floor::reassign(name, &into).await?;
        }
        Ok(tally)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        if !apply {
            return self.held(cx, who).await;
        }
        let telemetry = &cx.hub()?.telemetry_store;
        let mut tally = Self::per_name(who, |name| telemetry.forget_identity_signals(name))?;
        tally.absorb(Self::floor(who).await?);
        for name in &who.names {
            crate::floor::forget(name).await?;
        }
        Ok(tally)
    }
}

/// Billing history: kept whatever happens to the identity, and moved with
/// the person when they are merged into another identity (or a missing
/// one's personal content is handed over), so their spend still counts
/// against their own budget and their usage history stays theirs.
pub struct CostLedger;

#[async_trait]
impl Holding for CostLedger {
    /// Kept by a delete, so nothing here is left over to act on. The ledger
    /// records the identity a turn was charged to, which is the signed-in
    /// identity ([`crate::user_config::current_identity_id`]) or a `system:`
    /// lane, so an id in it proves an identity.
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        Ok(Owners::proving(
            cx.hub()?
                .telemetry_store
                .cost_by_identity(DateTime::<Utc>::UNIX_EPOCH, Utc::now())?
                .into_iter()
                .map(|(identity, _)| identity),
        ))
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let spent: f64 = cx
            .hub()?
            .telemetry_store
            .cost_by_identity(DateTime::<Utc>::UNIX_EPOCH, Utc::now())?
            .into_iter()
            .filter(|(identity, _)| who.names.contains(identity))
            .map(|(_, usd)| usd)
            .sum();
        Ok(match spent > 0.0 {
            true => Tally::of([format!("USD {spent:.2} spent")]),
            false => Tally::default(),
        })
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, from).await?;
        if apply {
            let telemetry = &cx.hub()?.telemetry_store;
            let into = into.key();
            for name in from.names.iter().filter(|name| **name != into) {
                telemetry.reassign_identity_billing(name, &into)?;
            }
        }
        Ok(tally)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        _apply: bool,
    ) -> Result<Tally> {
        self.held(cx, who).await
    }
}
