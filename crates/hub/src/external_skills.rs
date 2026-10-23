//! The external (SKILL.md) skills every agent in the process sees — one set per skills directory,
//! shared by every agent service and management surface, so an install, edit or removal through
//! any of them reaches every agent on its next message. Keyed by directory rather than a single
//! global so tests that build services over different temp data directories stay isolated.
//!
//! Each skill's owner, visibility and grantees live in an access record inside its own directory
//! ([`installer::ACCESS_FILE`]), which only the hub writes and which lives and dies with the
//! directory. A directory without one holds a skill for everyone.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use eyre::eyre;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio::task::JoinError;
use tracing::{debug, error, info, warn};
use workforce_core::access::{AccessLevel, AccessWrite, Grant, Visibility};
use workforce_skills::catalog::SkillKind;
use workforce_skills::installer::{self, Installed, Placement, SkillSource};
use workforce_skills::scanner::{self, DiscoveredSkill};
use workforce_wasm_runtime::{OwnedSkill, SkillOwnership, WasmSkillStore};

use crate::skill_access::{refusal, SkillCaller, SkillRegistry};
use crate::team_reach::TeamReach;

const BUNDLED_SKILL_ROOTS: [&str; 2] = ["/app/bundled-skills/skills", "skills"];

static SETS: LazyLock<Mutex<HashMap<PathBuf, Arc<ExternalSkills>>>> =
    LazyLock::new(Default::default);

/// Where external skills are read from, highest precedence first: the skills directory every hub
/// surface installs into, the operator's `~/.workforce/skills`, then skills baked into the image
/// and the working directory's `skills/`.
pub fn skill_roots(skills_dir: &Path) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    std::iter::once(skills_dir.to_path_buf())
        .chain(scanner::user_skills_dir())
        .chain(BUNDLED_SKILL_ROOTS.iter().map(PathBuf::from))
        .filter(|root| seen.insert(root.clone()))
        .collect()
}

/// Who owns an external skill and who may use it, as its directory records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessRecord {
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub shares: Vec<Grant>,
}

impl AccessRecord {
    /// The standing of a directory without a record: nobody's, and everyone's to use.
    pub fn org_wide() -> Self {
        Self {
            owner: None,
            visibility: Visibility::Global,
            shares: Vec::new(),
        }
    }
}

impl OwnedSkill for AccessRecord {
    fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    fn visibility(&self) -> Visibility {
        self.visibility
    }

    fn shares(&self) -> &[Grant] {
        &self.shares
    }
}

/// A live external skill and who may use it.
#[derive(Debug, Clone)]
pub struct ExternalSkill {
    pub discovered: DiscoveredSkill,
    pub access: AccessRecord,
}

impl ExternalSkill {
    pub fn name(&self) -> &str {
        &self.discovered.skill.meta.name
    }
}

impl OwnedSkill for ExternalSkill {
    fn owner(&self) -> Option<&str> {
        self.access.owner()
    }

    fn visibility(&self) -> Visibility {
        self.access.visibility()
    }

    fn shares(&self) -> &[Grant] {
        self.access.shares()
    }
}

/// Why an install or uninstall did not happen: a refusal the caller can act on, or a failure.
#[derive(Debug)]
pub enum ExternalSkillError {
    Refused(String),
    Failed(eyre::Report),
}

impl std::fmt::Display for ExternalSkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(why) => f.write_str(why),
            Self::Failed(report) => write!(f, "{report}"),
        }
    }
}

impl std::error::Error for ExternalSkillError {}

impl From<eyre::Report> for ExternalSkillError {
    fn from(report: eyre::Report) -> Self {
        Self::Failed(report)
    }
}

impl From<JoinError> for ExternalSkillError {
    fn from(error: JoinError) -> Self {
        Self::Failed(eyre::Report::new(error))
    }
}

/// How often [`read_access`] looks again at a directory that changed while it looked.
const ACCESS_READS: usize = 3;

/// What tells the directory at `dir` from any other put at that path since: a rename moves a
/// directory's `ctime` on, so one moved away and back is told apart too. `None` when there is
/// no directory there.
fn incarnation(dir: &Path) -> Option<(u64, u64, i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(dir)
        .ok()
        .filter(std::fs::Metadata::is_dir)
        .map(|meta| (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()))
}

/// The access record in `dir`: the org-wide standing when the directory holds none, an error
/// naming the file when it cannot be read or parsed. Only a directory that stayed the same one
/// while it was read holds none: a skill being replaced or removed is gone for a moment, and
/// must not pass for one without a record.
pub fn read_access(dir: &Path) -> eyre::Result<AccessRecord> {
    let path = dir.join(installer::ACCESS_FILE);
    let stored = || match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| eyre!("{} does not parse: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(eyre!("{} cannot be read: {e}", path.display())),
    };
    (0..ACCESS_READS)
        .find_map(|_| {
            let before = incarnation(dir);
            match stored() {
                Ok(Some(record)) => Some(Ok(record)),
                Ok(None) => (before.is_some() && before == incarnation(dir))
                    .then(|| Ok(AccessRecord::org_wide())),
                Err(e) => Some(Err(e)),
            }
        })
        .unwrap_or_else(|| Err(eyre!("{} is missing or being replaced", dir.display())))
}

/// Replace the access record in `dir` with one rename, so a reader sees the old record or the
/// new one.
fn write_access(dir: &Path, record: &AccessRecord) -> eyre::Result<()> {
    let staged = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer_pretty(staged.as_file(), record)?;
    staged
        .persist(dir.join(installer::ACCESS_FILE))
        .map_err(|e| eyre::Report::new(e.error))?;
    Ok(())
}

/// The record of the installed directory `located` would replace, or `None` when there is none.
fn directory_record(
    skills_dir: &Path,
    located: &SkillSource,
) -> eyre::Result<Option<AccessRecord>> {
    let dir = skills_dir.join(located.dir_name());
    match dir.is_dir() {
        false => Ok(None),
        true => read_access(&dir).map(Some),
    }
}

/// [`directory_record`] for a caller about to replace the directory. A skills manager replaces a
/// record that cannot be read, which is how it is repaired; anyone else is refused.
fn replaced_record(
    skills_dir: &Path,
    located: &SkillSource,
    manages: bool,
) -> Result<Option<AccessRecord>, String> {
    directory_record(skills_dir, located).or_else(|e| match manages {
        true => {
            warn!(
                dir = located.dir_name(),
                error = %e,
                "Replacing an external skill whose access record cannot be read"
            );
            Ok(None)
        }
        false => Err(format!(
            "The access record in `{}` cannot be read ({e}), so the skill there cannot be \
             replaced. An admin can remove that directory on the Skills page.",
            located.dir_name()
        )),
    })
}

/// The record an install writes: a record that names an owner is kept with its visibility and
/// grantees; otherwise the supplied ownership applies, with no grantees, and an ownerless
/// ownership writes none.
pub(crate) fn kept_or(
    existing: Option<AccessRecord>,
    ownership: SkillOwnership,
) -> Option<AccessRecord> {
    existing
        .filter(|record| record.owner.is_some())
        .or_else(|| {
            ownership.owner.map(|owner| AccessRecord {
                owner: Some(owner),
                visibility: ownership.visibility,
                shares: Vec::new(),
            })
        })
}

fn place(
    skills_dir: &Path,
    located: &SkillSource,
    record: Option<&AccessRecord>,
) -> eyre::Result<Installed> {
    let record = record.map(serde_json::to_string_pretty).transpose()?;
    installer::place_skill(located, skills_dir, Placement::Replace, record.as_deref())
}

/// Swap a located skill into `skills_dir` for the whole org, from outside the hub: a record that
/// names an owner is kept, and anything else is installed for everyone.
pub fn place_for_org(skills_dir: &Path, located: &SkillSource) -> eyre::Result<Installed> {
    let existing = replaced_record(skills_dir, located, true).map_err(eyre::Report::msg)?;
    place(
        skills_dir,
        located,
        kept_or(existing, SkillOwnership::operator()).as_ref(),
    )
}

/// Whether `caller` may read the instructions of the skill directory `dir`.
pub fn may_read_instructions(caller: &SkillCaller, dir: &Path) -> bool {
    reaches(caller, dir, |_| AccessLevel::Run)
}

/// Whether `caller` may read the `config.toml` of the skill directory `dir`: an owned skill's
/// settings are for whoever manages it.
pub fn may_read_config(caller: &SkillCaller, dir: &Path) -> bool {
    reaches(caller, dir, |record| match record.owner {
        Some(_) => AccessLevel::Manage,
        None => AccessLevel::Run,
    })
}

fn reaches(
    caller: &SkillCaller,
    dir: &Path,
    needed: impl FnOnce(&AccessRecord) -> AccessLevel,
) -> bool {
    read_access(dir).map_or(caller.is_admin, |record| {
        caller.access(&record).allows(needed(&record))
    })
}

/// [`reaches`] for a *write* to the skill directory `dir`, held as `name`: the
/// same answer, with the bypass recorded when it reaches past an owner.
fn takes(caller: &SkillCaller, name: &str, dir: &Path, needed: AccessLevel) -> bool {
    read_access(dir).map_or(caller.is_admin, |record| {
        caller.may_take(&record, name, needed, SkillKind::External)
    })
}

/// What an install stamps. A skills manager installs for everyone unless they ask for a private
/// skill; everyone else installs one of their own.
fn ownership_for(
    caller: &SkillCaller,
    requested: Option<Visibility>,
    previous: Option<&AccessRecord>,
) -> SkillOwnership {
    match caller.is_admin && requested != Some(Visibility::Private) {
        true => SkillOwnership::operator(),
        false => caller.ownership(requested, previous),
    }
}

/// The built-in skill whose name or directory `name` or `dir` would take, in any case, since a
/// case-insensitive filesystem gives both spellings one directory.
fn builtin_holding(name: &str, dir: &str) -> Option<&'static str> {
    crate::skill_loader::builtin_skill_catalog()
        .into_iter()
        .find(|builtin| {
            [builtin.name, builtin.dir_name()]
                .iter()
                .any(|taken| [name, dir].iter().any(|n| n.eq_ignore_ascii_case(taken)))
        })
        .map(|builtin| builtin.name)
}

/// A live skill with its record, or `None` — with a warning — when the record cannot be read, so
/// a skill whose owner is unknown reaches nobody.
fn with_access(discovered: DiscoveredSkill) -> Option<ExternalSkill> {
    read_access(&discovered.path)
        .inspect_err(|e| {
            warn!(
                path = %discovered.path.display(),
                error = %e,
                "Leaving out an external skill whose access record cannot be read"
            )
        })
        .ok()
        .map(|access| ExternalSkill { discovered, access })
}

fn scan(roots: &[PathBuf]) -> std::io::Result<Arc<[ExternalSkill]>> {
    scanner::scan_skill_roots(roots)
        .map(|found| found.into_iter().filter_map(with_access).collect())
}

pub struct ExternalSkills {
    skills_dir: PathBuf,
    roots: Vec<PathBuf>,
    current: RwLock<Arc<[ExternalSkill]>>,
    /// Serialises everything that reads or changes the skills directory through this set: each
    /// `write`, sharing change and rescan. A read-authorise-write sequence sees no other, and a
    /// scan never pairs one install's `SKILL.md` with another's record. [`Self::write`] takes
    /// it, so its closures never do.
    claims: Mutex<()>,
}

/// The set of external skills `service` reads, the one its skills directory names.
pub fn of(service: &crate::serve::AgentService) -> Arc<ExternalSkills> {
    ExternalSkills::shared(&service.skills_dir)
}

impl ExternalSkills {
    /// The process-wide set for `skills_dir`. The directory is made absolute lexically first, so
    /// two spellings of one directory share a set without touching the filesystem.
    pub fn shared(skills_dir: &Path) -> Arc<Self> {
        let key = std::path::absolute(skills_dir).unwrap_or_else(|_| skills_dir.to_path_buf());
        Arc::clone(
            SETS.lock()
                .entry(key)
                .or_insert_with_key(|dir| Arc::new(Self::new(dir.clone(), skill_roots(dir)))),
        )
    }

    pub(crate) fn new(skills_dir: PathBuf, roots: Vec<PathBuf>) -> Self {
        let found = scan(&roots).unwrap_or_else(|e| {
            warn!(error = %e, "Could not read the skills directories; starting with no external skills");
            Arc::new([])
        });
        info!(count = found.len(), "SKILL.md skills discovered");
        Self {
            skills_dir,
            roots,
            current: RwLock::new(found),
            claims: Mutex::new(()),
        }
    }

    /// The directory every in-hub surface installs external skills into.
    pub fn skills_dir(&self) -> &Path {
        &self.skills_dir
    }

    pub fn snapshot(&self) -> Arc<[ExternalSkill]> {
        Arc::clone(&self.current.read())
    }

    /// The live skills `viewer` may run, in scan order. There is no admin bypass: this is what
    /// an agent working for that identity is told about, and one user's private instructions
    /// stay out of everyone else's prompt.
    pub fn reachable(&self, viewer: &TeamReach) -> Vec<ExternalSkill> {
        let caller = SkillCaller::new(viewer.identity().map(str::to_string), false, viewer.clone());
        self.snapshot()
            .iter()
            .filter(|skill| caller.access(*skill).allows(AccessLevel::Run))
            .cloned()
            .collect()
    }

    /// Run `write` against the skills directories on a blocking thread, one write at a time, then
    /// re-read them. Both run on a task of their own, so the live set catches up with the disk
    /// whatever the write returns, even when it panics or the caller stops waiting.
    pub async fn write<T, E>(
        self: &Arc<Self>,
        write: impl FnOnce() -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<JoinError> + Send + 'static,
    {
        let skills = Arc::clone(self);
        tokio::spawn(async move {
            let writer = Arc::clone(&skills);
            let written = tokio::task::spawn_blocking(move || {
                let _claims = writer.claims.lock();
                write()
            })
            .await;
            skills.refresh().await;
            written.map_err(E::from).flatten()
        })
        .await
        .map_err(E::from)
        .flatten()
    }

    /// Install a located skill for `caller`, replacing an installed copy they manage and keeping
    /// its `config.toml`, then refresh the live set. A skills manager installs for everyone unless
    /// they ask for `Private`; anyone else installs a skill of their own. A record that names an
    /// owner survives the reinstall, and `requested` changes its visibility only for whoever
    /// controls who reaches it. `wasm` is the store whose names an owned install may not take.
    pub async fn install(
        self: &Arc<Self>,
        caller: SkillCaller,
        located: SkillSource,
        requested: Option<Visibility>,
        wasm: Option<Arc<WasmSkillStore>>,
    ) -> Result<Installed, ExternalSkillError> {
        let skills = Arc::clone(self);
        self.write(move || {
            let (name, dir) = (located.name(), located.dir_name());
            if let (true, Some(builtin)) = (caller.is_member(), builtin_holding(name, dir)) {
                return Err(ExternalSkillError::Refused(format!(
                    "`{name}` shares its name or directory with the built-in `{builtin}` skill, so \
                     only someone with the skills_manage capability can install it. Rename the \
                     skill, and the directory you install it from."
                )));
            }
            caller
                .require_install(skills.as_ref(), name)
                .map_err(ExternalSkillError::Refused)?;
            let existing = replaced_record(&skills.skills_dir, &located, !caller.is_member())
                .map_err(ExternalSkillError::Refused)?;
            if let Some(held) = existing
                .as_ref()
                .filter(|record| !caller.access(*record).allows(AccessLevel::Manage))
            {
                return Err(ExternalSkillError::Refused(refusal(
                    dir,
                    AccessLevel::Manage,
                    held.owner.is_none(),
                    SkillKind::External,
                )));
            }
            let ownership = ownership_for(&caller, requested, existing.as_ref());
            let record = kept_or(existing, ownership).map(|record| match requested {
                Some(visibility)
                    if record.visibility != visibility && caller.controls_access(&record) =>
                {
                    AccessRecord {
                        visibility,
                        ..record
                    }
                }
                _ => record,
            });
            if record.is_some()
                && wasm
                    .as_deref()
                    .is_some_and(|store| store.get_installed(name).is_some())
            {
                return Err(ExternalSkillError::Refused(format!(
                    "The name `{name}` belongs to a WASM skill. Pick a different name for your \
                     external skill."
                )));
            }
            place(&skills.skills_dir, &located, record.as_ref()).map_err(ExternalSkillError::Failed)
        })
        .await
    }

    /// Whether `skill` lives in the skills directory, the only one the hub writes to. A skill
    /// read from any other root is for everyone, and nothing changes it here.
    pub fn installed_here(&self, skill: &ExternalSkill) -> bool {
        skill.discovered.path.parent() == Some(self.skills_dir.as_path())
    }

    /// Remove the installed skill `name` for a caller who manages it, then refresh the live set.
    /// Standing is judged on the directory the removal takes: its live skill, or its record when
    /// the skill there is not live. A skill read from another root is never removed.
    pub async fn uninstall(
        self: &Arc<Self>,
        caller: SkillCaller,
        name: String,
    ) -> Result<(), ExternalSkillError> {
        let skills = Arc::clone(self);
        self.write(move || {
            let dir = installer::find_skill_dir_by_name(&name, &skills.skills_dir).map_err(
                |missing| match caller.require(skills.as_ref(), &name, AccessLevel::Manage) {
                    Ok(skill) if !skills.installed_here(&skill) => ExternalSkillError::Refused(
                        read_elsewhere(&skill, "it cannot be uninstalled here"),
                    ),
                    Ok(_) => ExternalSkillError::Failed(missing),
                    Err(refused) => ExternalSkillError::Refused(refused),
                },
            )?;
            let live = skills
                .snapshot()
                .iter()
                .find(|skill| skill.discovered.path == dir)
                .cloned();
            match live {
                Some(skill) => {
                    match caller.may_take(&skill, &name, AccessLevel::Manage, SkillKind::External) {
                        true => Ok(()),
                        false => Err(refusal(
                            &name,
                            AccessLevel::Manage,
                            skill.owner().is_none(),
                            SkillKind::External,
                        )),
                    }
                }
                None => match takes(&caller, &name, &dir, AccessLevel::Manage) {
                    true => Ok(()),
                    false => Err(refusal(
                        &name,
                        AccessLevel::Manage,
                        read_access(&dir).map_or(true, |record| record.owner.is_none()),
                        SkillKind::External,
                    )),
                },
            }
            .map_err(ExternalSkillError::Refused)?;
            installer::remove_skill_dir(&name, &dir).map_err(ExternalSkillError::Failed)
        })
        .await
    }

    async fn refresh(self: &Arc<Self>) {
        let skills = Arc::clone(self);
        if let Err(e) = tokio::task::spawn_blocking(move || skills.rescan()).await {
            error!(error = %e, "External skill refresh failed");
        }
    }

    /// Rescans run one at a time, never during a write, and each starts after its caller's
    /// write, so the last swap always reflects the latest state on disk. An unreadable skills
    /// directory keeps the previous set.
    fn rescan(&self) {
        let _claims = self.claims.lock();
        // A write just landed with the hub's umask; close it before the agent
        // uid can change what the hub is about to read.
        workforce_common::config_files::close_tree(&self.skills_dir);
        match scan(&self.roots) {
            Ok(found) => {
                debug!(count = found.len(), "External skills refreshed");
                *self.current.write() = found;
            }
            Err(e) => {
                warn!(error = %e, "Could not re-read the skills directories; agents keep the previous skill list");
            }
        }
    }
}

impl SkillRegistry for ExternalSkills {
    type Skill = ExternalSkill;
    const KIND: SkillKind = SkillKind::External;

    fn installed(&self, name: &str) -> Option<ExternalSkill> {
        self.snapshot()
            .iter()
            .find(|skill| skill.name() == name)
            .cloned()
    }

    fn all_installed(&self) -> Vec<ExternalSkill> {
        self.snapshot().to_vec()
    }

    fn skill_name(skill: &ExternalSkill) -> &str {
        skill.name()
    }

    /// Writes the record in the skill's directory, then swaps that skill in the live set. A
    /// rescan running meanwhile finishes first or reads the new record.
    fn set_access(&self, name: &str, write: AccessWrite) -> eyre::Result<ExternalSkill> {
        let _claims = self.claims.lock();
        let skill = self
            .installed(name)
            .ok_or_else(|| eyre!("Skill '{name}' is not installed"))?;
        if let Some(fixed) = self.fixed_reach(&skill, write.visibility) {
            return Err(eyre::Report::msg(fixed));
        }
        let stored = read_access(&skill.discovered.path)?;
        let access = AccessRecord {
            owner: write.owner_after(stored.owner.as_deref()),
            visibility: write.visibility,
            shares: write.grants,
        };
        write_access(&skill.discovered.path, &access)?;
        let updated = ExternalSkill { access, ..skill };
        let next: Arc<[ExternalSkill]> = self
            .snapshot()
            .iter()
            .map(
                |live| match live.discovered.path == updated.discovered.path {
                    true => updated.clone(),
                    false => live.clone(),
                },
            )
            .collect();
        *self.current.write() = next;
        Ok(updated)
    }

    /// A skill read from another root keeps its reach, and a skill nobody owns is for everyone,
    /// so its visibility would only mislead.
    fn reach_fixed(&self, skill: &ExternalSkill) -> Option<String> {
        match (self.installed_here(skill), skill.owner()) {
            (false, _) => Some(read_elsewhere(
                skill,
                "who can use it cannot be changed here",
            )),
            (true, None) => Some(ownerless_note(skill)),
            (true, Some(_)) => None,
        }
    }

    /// Leaving an ownerless skill's reach exactly where it is asks nothing of
    /// it, and the team a skill nobody owns does not have is refused in the one
    /// sentence both kinds of skill use for it.
    fn fixed_reach(&self, skill: &ExternalSkill, visibility: Visibility) -> Option<String> {
        match (self.installed_here(skill), skill.owner()) {
            (false, _) => self.reach_fixed(skill),
            (true, Some(_)) => None,
            (true, None) => crate::skill_access::ownerless_team(skill.name(), skill, visibility)
                .or_else(|| (visibility != skill.visibility()).then(|| ownerless_note(skill))),
        }
    }
}

/// Why a skill nobody owns has the reach it has, whatever its record says.
fn ownerless_note(skill: &ExternalSkill) -> String {
    format!(
        "Skill `{}` has no owner, so everyone can use it whatever its visibility. Install it \
         again with `visibility = \"private\"` to make it a private skill of your own.",
        skill.name()
    )
}

/// Why `skill`, read from a root other than the skills directory, is left as it is.
fn read_elsewhere(skill: &ExternalSkill, so: &str) -> String {
    format!(
        "Skill `{}` is read from `{}`, outside the skills directory the hub installs into, so {so}.",
        skill.name(),
        skill.discovered.path.display()
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use workforce_core::access::GrantLevel;
    use workforce_skills::installer::InstallOptions;

    pub(crate) fn write_skill(skills_dir: &Path, dir: &str, name: &str) -> std::io::Result<()> {
        write_versioned(&skills_dir.join(dir), name, "1.0.0")
    }

    fn write_versioned(dir: &Path, name: &str, version: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\nversion: {version}\ndescription: A test skill\n---\nBody\n"
            ),
        )
    }

    fn names(skills: &ExternalSkills) -> Vec<String> {
        let mut names: Vec<String> = skills
            .snapshot()
            .iter()
            .map(|s| s.name().to_string())
            .collect();
        names.sort();
        names
    }

    fn names_of(skills: Vec<ExternalSkill>) -> Vec<String> {
        skills.iter().map(|s| s.name().to_string()).collect()
    }

    fn isolated(skills_dir: &Path) -> Arc<ExternalSkills> {
        Arc::new(ExternalSkills::new(
            skills_dir.to_path_buf(),
            vec![skills_dir.to_path_buf()],
        ))
    }

    fn member(uid: &str) -> SkillCaller {
        SkillCaller::local(Some(uid.to_string()), false)
    }

    fn manager() -> SkillCaller {
        SkillCaller::local(Some("root".to_string()), true)
    }

    fn identity_less() -> SkillCaller {
        SkillCaller::local(None, false)
    }

    /// Someone reading the skill list, standing in no team.
    fn viewer(user: Option<&str>) -> TeamReach {
        TeamReach::new(None, user.map(str::to_string))
    }

    pub(crate) fn owned(
        owner: Option<&str>,
        visibility: Visibility,
        shares: Vec<Grant>,
    ) -> AccessRecord {
        AccessRecord {
            owner: owner.map(str::to_string),
            visibility,
            shares,
        }
    }

    pub(crate) fn share(user: &str, permission: GrantLevel) -> Grant {
        Grant::user(user, permission)
    }

    pub(crate) fn record_in(dir: &Path, record: &AccessRecord) {
        write_access(dir, record).unwrap();
    }

    /// A skill directory outside the skills directory, located as an install would locate it.
    fn source(root: &Path, dir: &str, name: &str, version: &str) -> SkillSource {
        let path = root.join("sources").join(version).join(dir);
        write_versioned(&path, name, version).unwrap();
        installer::locate_skill(path.to_str().unwrap(), &InstallOptions::default()).unwrap()
    }

    fn refusal_text(outcome: Result<Installed, ExternalSkillError>) -> String {
        match outcome {
            Err(ExternalSkillError::Refused(why)) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn skill_roots_put_the_skills_dir_first_and_drop_duplicates() {
        let roots = skill_roots(Path::new("skills"));
        assert_eq!(roots.first(), Some(&PathBuf::from("skills")));
        assert_eq!(
            roots.iter().filter(|r| *r == Path::new("skills")).count(),
            1
        );
        assert!(roots.contains(&PathBuf::from("/app/bundled-skills/skills")));
    }

    #[tokio::test]
    async fn write_picks_up_installs_and_removals() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);
        assert!(skills.snapshot().is_empty());

        let dir = skills_dir.clone();
        skills
            .write(move || write_skill(&dir, "hello-world", "hello-world"))
            .await
            .unwrap();
        assert_eq!(names(&skills), ["hello-world"]);

        let dir = skills_dir.join("hello-world");
        skills
            .write(move || std::fs::remove_dir_all(dir))
            .await
            .unwrap();
        assert!(skills.snapshot().is_empty());
    }

    #[tokio::test]
    async fn write_refreshes_whatever_the_write_does() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);

        let dir = skills_dir.clone();
        let failed = skills
            .write(move || {
                write_skill(&dir, "hello-world", "hello-world")?;
                Err::<(), _>(std::io::Error::other("failed after writing"))
            })
            .await;
        assert!(failed.is_err());
        assert_eq!(names(&skills), ["hello-world"]);

        let dir = skills_dir.clone();
        let panicked = skills
            .write(move || -> std::io::Result<()> {
                write_skill(&dir, "catalog-echo", "catalog-echo").unwrap();
                panic!("panicked after writing");
            })
            .await;
        assert!(panicked.is_err());
        assert_eq!(names(&skills), ["catalog-echo", "hello-world"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_refreshes_after_the_caller_stops_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let (writer, dir) = (Arc::clone(&skills), skills_dir.clone());
        let caller = tokio::spawn(async move {
            writer
                .write(move || {
                    let _ = started_tx.send(());
                    let _ = release_rx.recv();
                    write_skill(&dir, "hello-world", "hello-world")
                })
                .await
        });
        started_rx.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        release_tx.send(()).unwrap();

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while names(&skills).is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(names(&skills), ["hello-world"]);
    }

    #[tokio::test]
    async fn write_keeps_the_previous_set_when_the_skills_dir_cannot_be_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("a");
        let skills_dir = parent.join("skills");
        write_skill(&skills_dir, "hello-world", "hello-world").unwrap();
        let skills = isolated(&skills_dir);
        assert_eq!(names(&skills), ["hello-world"]);

        skills
            .write(move || {
                std::fs::remove_dir_all(&parent)?;
                std::fs::write(&parent, "not a directory")
            })
            .await
            .unwrap();
        assert_eq!(names(&skills), ["hello-world"]);
    }

    #[tokio::test]
    async fn write_drops_only_a_skill_whose_skill_md_breaks() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        write_skill(&skills_dir, "hello-world", "hello-world").unwrap();
        write_skill(&skills_dir, "catalog-echo", "catalog-echo").unwrap();
        let skills = isolated(&skills_dir);
        assert_eq!(names(&skills), ["catalog-echo", "hello-world"]);

        let skill_md = skills_dir.join("hello-world").join("SKILL.md");
        skills
            .write(move || std::fs::write(skill_md, "broken"))
            .await
            .unwrap();
        assert_eq!(names(&skills), ["catalog-echo"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_converge_on_the_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);

        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let (skills, skills_dir) = (Arc::clone(&skills), skills_dir.clone());
                tokio::spawn(async move {
                    let name = format!("skill-{i}");
                    skills
                        .write(move || write_skill(&skills_dir, &name, &name))
                        .await
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        let expected: Vec<String> = (0..8).map(|i| format!("skill-{i}")).collect();
        assert_eq!(names(&skills), expected);
    }

    #[test]
    fn shared_returns_one_set_per_skills_dir() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let first = ExternalSkills::shared(a.path());
        assert!(Arc::ptr_eq(&first, &ExternalSkills::shared(a.path())));
        assert!(!Arc::ptr_eq(&first, &ExternalSkills::shared(b.path())));
        assert_eq!(first.skills_dir(), a.path());

        let relative = format!("target/external-skills-test-{}", std::process::id());
        let plain = ExternalSkills::shared(Path::new(&relative));
        let dotted = ExternalSkills::shared(Path::new(&format!("./{relative}")));
        assert!(Arc::ptr_eq(&plain, &dotted));
        assert!(plain.skills_dir().is_absolute());
    }

    #[test]
    fn a_directory_without_a_record_is_for_everyone() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        write_skill(&skills_dir, "hello-world", "hello-world").unwrap();
        let skills = isolated(&skills_dir);
        let dir = skills_dir.join("hello-world");

        assert_eq!(
            skills.installed("hello-world").unwrap().access,
            AccessRecord::org_wide()
        );
        assert!(!dir.join(installer::ACCESS_FILE).exists());
        for user in [Some("alice"), Some("root"), None] {
            assert_eq!(
                names_of(skills.reachable(&viewer(user))),
                ["hello-world"],
                "{user:?}"
            );
        }
        for caller in [member("alice"), manager(), identity_less()] {
            assert!(may_read_instructions(&caller, &dir), "{caller:?}");
            assert!(may_read_config(&caller, &dir), "{caller:?}");
        }
        assert_eq!(
            member("alice").access(&skills.installed("hello-world").unwrap()),
            AccessLevel::Run
        );
    }

    #[test]
    fn a_missing_directory_has_no_standing() {
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("nowhere");

        let error = read_access(&gone).unwrap_err().to_string();
        assert!(error.contains("nowhere"), "{error}");
        assert!(!may_read_instructions(&member("alice"), &gone));
        assert!(!may_read_config(&identity_less(), &gone));
    }

    #[tokio::test]
    async fn a_write_holds_off_rescans_and_sharing_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = isolated(&tmp.path().join("skills"));

        let writer = Arc::clone(&skills);
        let free = skills
            .write(move || Ok::<_, std::io::Error>(writer.claims.try_lock().is_some()))
            .await
            .unwrap();
        assert!(!free, "the write runs with the claim taken");
        assert!(skills.claims.try_lock().is_some(), "and gives it back");
    }

    #[tokio::test]
    async fn an_unreadable_record_leaves_the_skill_out() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        write_skill(&skills_dir, "hello-world", "hello-world").unwrap();
        write_skill(&skills_dir, "catalog-echo", "catalog-echo").unwrap();
        let dir = skills_dir.join("hello-world");
        std::fs::write(dir.join(installer::ACCESS_FILE), "not json").unwrap();
        let skills = isolated(&skills_dir);

        assert_eq!(names(&skills), ["catalog-echo"]);
        let error = read_access(&dir).unwrap_err().to_string();
        assert!(error.contains(installer::ACCESS_FILE), "{error}");
        assert!(!may_read_instructions(&member("alice"), &dir));
        assert!(!may_read_config(&member("alice"), &dir));
        assert!(may_read_instructions(&manager(), &dir));

        skills.write(|| Ok::<_, std::io::Error>(())).await.unwrap();
        assert_eq!(names(&skills), ["catalog-echo"]);
    }

    #[test]
    fn reachable_filters_by_identity_without_a_bypass_and_keeps_scan_order() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        for name in ["a-private", "b-global", "c-shared", "d-org"] {
            write_skill(&skills_dir, name, name).unwrap();
        }
        record_in(
            &skills_dir.join("a-private"),
            &owned(Some("alice"), Visibility::Private, vec![]),
        );
        record_in(
            &skills_dir.join("b-global"),
            &owned(Some("alice"), Visibility::Global, vec![]),
        );
        record_in(
            &skills_dir.join("c-shared"),
            &owned(
                Some("alice"),
                Visibility::Private,
                vec![share("bob", GrantLevel::Run)],
            ),
        );
        let skills = isolated(&skills_dir);

        assert_eq!(
            names_of(skills.reachable(&viewer(Some("alice")))),
            ["a-private", "b-global", "c-shared", "d-org"]
        );
        assert_eq!(
            names_of(skills.reachable(&viewer(Some("bob")))),
            ["b-global", "c-shared", "d-org"]
        );
        for user in [Some("root"), Some("carol"), None] {
            assert_eq!(
                names_of(skills.reachable(&viewer(user))),
                ["b-global", "d-org"],
                "{user:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_members_install_is_private_and_theirs() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);

        let installed = skills
            .install(
                member("alice"),
                source(tmp.path(), "hello-world", "hello-world", "1.0.0"),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(installed.path, skills_dir.join("hello-world"));
        let expected = owned(Some("alice"), Visibility::Private, vec![]);
        assert_eq!(skills.installed("hello-world").unwrap().access, expected);
        assert_eq!(read_access(&installed.path).unwrap(), expected);
        assert_eq!(
            names_of(skills.reachable(&viewer(Some("alice")))),
            ["hello-world"]
        );
        assert!(skills.reachable(&viewer(Some("bob"))).is_empty());
        assert!(skills.reachable(&viewer(None)).is_empty());
    }

    #[tokio::test]
    async fn a_skills_manager_installs_for_everyone_unless_they_ask_for_private() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);

        for (dir, requested) in [
            ("hello-world", None),
            ("catalog-echo", Some(Visibility::Global)),
        ] {
            let installed = skills
                .install(
                    manager(),
                    source(tmp.path(), dir, dir, "1.0.0"),
                    requested,
                    None,
                )
                .await
                .unwrap();
            assert!(!installed.path.join(installer::ACCESS_FILE).exists());
            assert_eq!(
                skills.installed(dir).unwrap().access,
                AccessRecord::org_wide()
            );
        }

        skills
            .install(
                manager(),
                source(tmp.path(), "notes", "notes", "1.0.0"),
                Some(Visibility::Private),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            skills.installed("notes").unwrap().access,
            owned(Some("root"), Visibility::Private, vec![])
        );
    }

    #[tokio::test]
    async fn an_identity_less_install_is_for_everyone() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);

        for (dir, requested) in [("hello-world", None), ("notes", Some(Visibility::Private))] {
            let installed = skills
                .install(
                    identity_less(),
                    source(tmp.path(), dir, dir, "1.0.0"),
                    requested,
                    None,
                )
                .await
                .unwrap();
            assert!(!installed.path.join(installer::ACCESS_FILE).exists());
            assert_eq!(
                skills.installed(dir).unwrap().access,
                AccessRecord::org_wide()
            );
        }
    }

    #[tokio::test]
    async fn reinstalling_keeps_an_owned_record_config_and_sharing() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);
        let first = skills
            .install(
                member("alice"),
                source(tmp.path(), "hello-world", "hello-world", "1.0.0"),
                None,
                None,
            )
            .await
            .unwrap();
        let shares = vec![share("bob", GrantLevel::Run)];
        skills
            .set_access(
                "hello-world",
                AccessWrite::kept(Visibility::Private, shares.clone()),
            )
            .unwrap();
        std::fs::write(first.path.join("config.toml"), "greeting = \"hi\"\n").unwrap();

        let second = skills
            .install(
                member("alice"),
                source(tmp.path(), "hello-world", "hello-world", "2.0.0"),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(second.replaced);
        assert_eq!(second.previous_version.as_deref(), Some("1.0.0"));
        let live = skills.installed("hello-world").unwrap();
        assert_eq!(live.discovered.skill.meta.version, "2.0.0");
        assert_eq!(
            live.access,
            owned(Some("alice"), Visibility::Private, shares)
        );
        assert_eq!(
            std::fs::read_to_string(second.path.join("config.toml")).unwrap(),
            "greeting = \"hi\"\n"
        );
    }

    #[tokio::test]
    async fn reinstalling_an_ownerless_skill_takes_the_supplied_ownership_and_drops_shares() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        write_skill(&skills_dir, "hello-world", "hello-world").unwrap();
        record_in(
            &skills_dir.join("hello-world"),
            &owned(
                None,
                Visibility::Private,
                vec![share("bob", GrantLevel::Manage)],
            ),
        );
        let skills = isolated(&skills_dir);
        assert_eq!(
            skills.installed("hello-world").unwrap().access,
            owned(
                None,
                Visibility::Private,
                vec![share("bob", GrantLevel::Manage)]
            )
        );

        skills
            .install(
                manager(),
                source(tmp.path(), "hello-world", "hello-world", "2.0.0"),
                Some(Visibility::Private),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            skills.installed("hello-world").unwrap().access,
            owned(Some("root"), Visibility::Private, vec![])
        );
    }

    #[tokio::test]
    async fn a_requested_visibility_applies_only_for_whoever_controls_access() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);
        let reinstall = |caller: SkillCaller, version: &str, requested| {
            let located = source(tmp.path(), "hello-world", "hello-world", version);
            let skills = Arc::clone(&skills);
            async move { skills.install(caller, located, requested, None).await }
        };
        reinstall(member("alice"), "1.0.0", None).await.unwrap();
        skills
            .set_access(
                "hello-world",
                AccessWrite::kept(
                    Visibility::Private,
                    vec![share("carol", GrantLevel::Manage)],
                ),
            )
            .unwrap();

        reinstall(member("alice"), "1.1.0", Some(Visibility::Global))
            .await
            .unwrap();
        assert_eq!(
            skills.installed("hello-world").unwrap().access.visibility,
            Visibility::Global
        );

        reinstall(member("carol"), "1.2.0", Some(Visibility::Private))
            .await
            .unwrap();
        let live = skills.installed("hello-world").unwrap();
        assert_eq!(live.discovered.skill.meta.version, "1.2.0");
        assert_eq!(live.access.visibility, Visibility::Global);
        assert_eq!(live.access.owner.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn a_member_cannot_take_another_users_name() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);
        skills
            .install(
                member("alice"),
                source(tmp.path(), "hello-world", "hello-world", "1.0.0"),
                None,
                None,
            )
            .await
            .unwrap();

        let refused = refusal_text(
            skills
                .install(
                    member("bob"),
                    source(tmp.path(), "my-dir", "hello-world", "2.0.0"),
                    None,
                    None,
                )
                .await,
        );
        assert!(
            refused.contains("belongs to another user's skill"),
            "{refused}"
        );
        assert!(!skills_dir.join("my-dir").exists());
    }

    #[tokio::test]
    async fn a_member_cannot_replace_an_org_wide_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        write_skill(&skills_dir, "hello-world", "hello-world").unwrap();
        let skills = isolated(&skills_dir);

        let refused = refusal_text(
            skills
                .install(
                    member("bob"),
                    source(tmp.path(), "hello-world", "hello-world", "2.0.0"),
                    None,
                    None,
                )
                .await,
        );
        assert!(refused.contains("org-wide skill"), "{refused}");
        assert_eq!(
            installer::installed_version(&skills_dir.join("hello-world")).as_deref(),
            Some("1.0.0")
        );
    }

    #[tokio::test]
    async fn a_member_cannot_shadow_a_lower_root_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let lower = tmp.path().join("bundled");
        write_skill(&lower, "hello-world", "hello-world").unwrap();
        let skills = Arc::new(ExternalSkills::new(
            skills_dir.clone(),
            vec![skills_dir.clone(), lower],
        ));

        let refused = refusal_text(
            skills
                .install(
                    member("bob"),
                    source(tmp.path(), "hello-world", "hello-world", "2.0.0"),
                    None,
                    None,
                )
                .await,
        );
        assert!(refused.contains("org-wide skill"), "{refused}");
        assert!(!skills_dir.join("hello-world").exists());
    }

    #[tokio::test]
    async fn a_member_cannot_occupy_a_directory_they_do_not_manage() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(skills_dir.join("team-notes")).unwrap();
        std::fs::write(skills_dir.join("team-notes/config.toml"), "token = \"x\"\n").unwrap();
        let draft = skills_dir.join("draft");
        std::fs::create_dir_all(&draft).unwrap();
        std::fs::write(draft.join("SKILL.md"), "not a skill yet").unwrap();
        record_in(&draft, &owned(Some("alice"), Visibility::Private, vec![]));
        let skills = isolated(&skills_dir);

        let config_only = refusal_text(
            skills
                .install(
                    member("bob"),
                    source(tmp.path(), "team-notes", "team-notes", "1.0.0"),
                    None,
                    None,
                )
                .await,
        );
        assert!(config_only.contains("has no owner"), "{config_only}");
        assert_eq!(
            std::fs::read_to_string(skills_dir.join("team-notes/config.toml")).unwrap(),
            "token = \"x\"\n"
        );
        assert!(!skills_dir.join("team-notes/SKILL.md").exists());

        let broken = refusal_text(
            skills
                .install(
                    member("bob"),
                    source(tmp.path(), "draft", "draft", "1.0.0"),
                    None,
                    None,
                )
                .await,
        );
        assert!(broken.contains("belongs to another user"), "{broken}");
        assert_eq!(
            std::fs::read_to_string(draft.join("SKILL.md")).unwrap(),
            "not a skill yet"
        );

        skills
            .install(
                member("alice"),
                source(tmp.path(), "draft", "draft", "1.0.0"),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            skills.installed("draft").unwrap().access.owner.as_deref(),
            Some("alice")
        );
    }

    #[tokio::test]
    async fn an_unreadable_record_is_replaced_only_by_a_skills_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        write_skill(&skills_dir, "hello-world", "hello-world").unwrap();
        std::fs::write(
            skills_dir.join("hello-world").join(installer::ACCESS_FILE),
            "{",
        )
        .unwrap();
        let skills = isolated(&skills_dir);

        let refused = refusal_text(
            skills
                .install(
                    member("alice"),
                    source(tmp.path(), "hello-world", "hello-world", "2.0.0"),
                    None,
                    None,
                )
                .await,
        );
        assert!(
            refused.starts_with("The access record in `hello-world` cannot be read ("),
            "{refused}"
        );
        assert!(
            refused.ends_with(
                "so the skill there cannot be replaced. An admin can remove that directory on \
                 the Skills page."
            ),
            "{refused}"
        );

        skills
            .install(
                manager(),
                source(tmp.path(), "hello-world", "hello-world", "2.0.0"),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            skills.installed("hello-world").unwrap().access,
            AccessRecord::org_wide()
        );
    }

    #[tokio::test]
    async fn a_member_cannot_claim_a_built_in_name_or_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);

        for (dir, name) in [
            ("github-helper", "github"),
            ("slack-integration", "my-slack"),
        ] {
            let refused = refusal_text(
                skills
                    .install(
                        member("alice"),
                        source(tmp.path(), dir, name, "1.0.0"),
                        None,
                        None,
                    )
                    .await,
            );
            let builtin = name.strip_prefix("my-").unwrap_or(name);
            assert_eq!(
                refused,
                format!(
                    "`{name}` shares its name or directory with the built-in `{builtin}` skill, \
                     so only someone with the skills_manage capability can install it. Rename \
                     the skill, and the directory you install it from."
                )
            );
        }
        let refused = refusal_text(
            skills
                .install(
                    member("alice"),
                    source(tmp.path(), "GitHub", "helper", "1.0.0"),
                    None,
                    None,
                )
                .await,
        );
        assert!(refused.contains("the built-in `github` skill"), "{refused}");
        assert!(!skills_dir.exists() || std::fs::read_dir(&skills_dir).unwrap().count() == 0);

        skills
            .install(
                manager(),
                source(tmp.path(), "github-helper", "github", "1.0.0"),
                None,
                None,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_owned_install_cannot_take_a_wasm_skills_name() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);
        let store = Arc::new(WasmSkillStore::open(&tmp.path().join("wasm-skills")).unwrap());
        store
            .install_prebuilt(
                workforce_wasm_runtime::SkillManifest::from_toml(
                    "[skill]\nname = \"hello-world\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\
                     [[tools]]\nname = \"ping\"\ndescription = \"p\"\n",
                )
                .unwrap(),
                b"wasm",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::operator(),
            )
            .unwrap();

        let refused = refusal_text(
            skills
                .install(
                    member("alice"),
                    source(tmp.path(), "hello-world", "hello-world", "1.0.0"),
                    None,
                    Some(Arc::clone(&store)),
                )
                .await,
        );
        assert_eq!(
            refused,
            "The name `hello-world` belongs to a WASM skill. Pick a different name for your \
             external skill."
        );
        assert!(!skills_dir.join("hello-world").exists());

        skills
            .install(
                manager(),
                source(tmp.path(), "hello-world", "hello-world", "1.0.0"),
                None,
                Some(store),
            )
            .await
            .unwrap();
        assert_eq!(
            skills.installed("hello-world").unwrap().access,
            AccessRecord::org_wide()
        );
    }

    #[tokio::test]
    async fn a_manage_grantee_updates_without_changing_the_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);
        skills
            .install(
                member("alice"),
                source(tmp.path(), "hello-world", "hello-world", "1.0.0"),
                None,
                None,
            )
            .await
            .unwrap();
        let shares = vec![share("carol", GrantLevel::Manage)];
        skills
            .set_access(
                "hello-world",
                AccessWrite::kept(Visibility::Private, shares.clone()),
            )
            .unwrap();

        skills
            .install(
                member("carol"),
                source(tmp.path(), "hello-world", "hello-world", "2.0.0"),
                None,
                None,
            )
            .await
            .unwrap();
        let live = skills.installed("hello-world").unwrap();
        assert_eq!(live.discovered.skill.meta.version, "2.0.0");
        assert_eq!(
            live.access,
            owned(Some("alice"), Visibility::Private, shares)
        );
    }

    #[tokio::test]
    async fn uninstall_needs_manage() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        write_skill(&skills_dir, "mine", "mine").unwrap();
        record_in(
            &skills_dir.join("mine"),
            &owned(
                Some("alice"),
                Visibility::Private,
                vec![share("bob", GrantLevel::Run)],
            ),
        );
        write_skill(&skills_dir, "org", "org").unwrap();
        let draft = skills_dir.join("draft");
        std::fs::create_dir_all(&draft).unwrap();
        std::fs::write(draft.join("SKILL.md"), "not a skill yet").unwrap();
        record_in(&draft, &owned(Some("alice"), Visibility::Private, vec![]));
        let skills = isolated(&skills_dir);
        let uninstall = |caller: SkillCaller, name: &str| {
            let (skills, name) = (Arc::clone(&skills), name.to_string());
            async move {
                match skills.uninstall(caller, name).await {
                    Ok(()) => Ok(()),
                    Err(ExternalSkillError::Refused(why)) => Err(why),
                    Err(ExternalSkillError::Failed(report)) => panic!("failed: {report}"),
                }
            }
        };

        assert_eq!(
            uninstall(member("bob"), "mine").await.unwrap_err(),
            "Skill `mine` belongs to another user. Ask them to share it with you \
             (`cmd: skill share mine <you> manage`)."
        );
        let stranger = uninstall(member("dave"), "mine").await.unwrap_err();
        assert!(stranger.contains("belongs to another user"), "{stranger}");
        let missing = uninstall(member("dave"), "nope").await.unwrap_err();
        assert_eq!(
            missing,
            "No external skill named `nope` is installed. Available to you: org"
        );
        assert!(uninstall(member("bob"), "org")
            .await
            .unwrap_err()
            .contains("has no owner"));
        assert!(uninstall(member("bob"), "draft")
            .await
            .unwrap_err()
            .contains("belongs to another user"));

        uninstall(member("alice"), "mine").await.unwrap();
        uninstall(manager(), "org").await.unwrap();
        uninstall(member("alice"), "draft").await.unwrap();
        assert!(names(&skills).is_empty());
        assert_eq!(std::fs::read_dir(&skills_dir).unwrap().count(), 0);
    }

    /// Removing somebody else's external skill reaches past its owner, so the
    /// bypass that allows it is recorded — on a live skill and on the record of
    /// one that never went live alike. A skill nobody owns has no owner to
    /// reach past, and a refusal is not an override.
    #[tokio::test]
    async fn an_overridden_uninstall_is_recorded() {
        use workforce_memory::access_audit::{self, AuditOperation};

        let buffer = access_audit::install_ring_buffer(256);
        let _ = access_audit::set_global_sink(buffer.clone());
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let unique = |stem: &str| format!("{stem}-{}", uuid::Uuid::new_v4());
        let (live, draft, org) = (unique("live"), unique("draft"), unique("org"));
        let hers = owned(Some("alice"), Visibility::Private, vec![]);
        write_skill(&skills_dir, &live, &live).unwrap();
        record_in(&skills_dir.join(&live), &hers);
        let draft_dir = skills_dir.join(&draft);
        std::fs::create_dir_all(&draft_dir).unwrap();
        std::fs::write(draft_dir.join("SKILL.md"), "not a skill yet").unwrap();
        record_in(&draft_dir, &hers);
        write_skill(&skills_dir, &org, &org).unwrap();
        let skills = isolated(&skills_dir);
        let events = |name: &str| {
            let resource = format!("external:{name}");
            buffer
                .snapshot()
                .into_iter()
                .filter(|event| event.resource_id.as_deref() == Some(resource.as_str()))
                .collect::<Vec<_>>()
        };

        assert!(skills.uninstall(member("bob"), live.clone()).await.is_err());
        skills.uninstall(manager(), org.clone()).await.unwrap();
        assert!(events(&live).is_empty(), "a refusal is not an override");
        assert!(events(&org).is_empty(), "nobody owns it");

        skills.uninstall(manager(), live.clone()).await.unwrap();
        let recorded = events(&live);
        assert_eq!(recorded.len(), 1, "exactly one event per override");
        assert_eq!(
            recorded[0].operation,
            AuditOperation::ResourceAccessOverride
        );
        assert_eq!(recorded[0].requester_identity_id.as_deref(), Some("root"));
        assert_eq!(recorded[0].owner_identity_id.as_deref(), Some("alice"));

        skills.uninstall(manager(), draft.clone()).await.unwrap();
        assert_eq!(events(&draft).len(), 1, "and one for a record-only skill");
    }

    #[tokio::test]
    async fn uninstall_is_judged_on_the_directory_it_removes() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let lower = tmp.path().join("bundled");
        write_skill(&lower, "hello-world", "hello-world").unwrap();
        record_in(
            &lower.join("hello-world"),
            &owned(
                None,
                Visibility::Global,
                vec![share("bob", GrantLevel::Manage)],
            ),
        );
        let mine = skills_dir.join("hello-world");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::write(mine.join("SKILL.md"), "not a skill yet").unwrap();
        record_in(&mine, &owned(Some("alice"), Visibility::Private, vec![]));
        let skills = Arc::new(ExternalSkills::new(
            skills_dir.clone(),
            vec![skills_dir.clone(), lower],
        ));
        assert_eq!(
            SkillCaller::local(Some("bob".into()), false)
                .access(&skills.installed("hello-world").unwrap()),
            AccessLevel::Manage
        );

        let refused = skills
            .uninstall(member("bob"), "hello-world".into())
            .await
            .unwrap_err()
            .to_string();
        assert!(refused.contains("belongs to another user"), "{refused}");
        assert!(mine.is_dir());
    }

    #[tokio::test]
    async fn uninstall_never_reaches_outside_the_skills_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        write_skill(&skills_dir, "org", "org").unwrap();
        let workspace = tmp.path().join("workspace");
        write_skill(&workspace, "mine", "mine").unwrap();
        record_in(
            &workspace.join("mine"),
            &owned(Some("alice"), Visibility::Private, vec![]),
        );
        let skills = isolated(&skills_dir);

        let refused = skills
            .uninstall(member("alice"), "../workspace/mine".into())
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            refused,
            "No external skill named `../workspace/mine` is installed. Available to you: org"
        );
        assert!(workspace.join("mine").join("SKILL.md").is_file());
    }

    #[tokio::test]
    async fn a_skill_from_another_root_is_for_everyone_and_never_changed_here() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        let bundled = tmp.path().join("bundled");
        write_skill(&bundled, "hello-world", "hello-world").unwrap();
        let skills = Arc::new(ExternalSkills::new(
            skills_dir.clone(),
            vec![skills_dir.clone(), bundled.clone()],
        ));
        let elsewhere = |so: &str| {
            format!(
                "Skill `hello-world` is read from `{}`, outside the skills directory the hub \
                 installs into, so {so}.",
                bundled.join("hello-world").display()
            )
        };

        assert_eq!(
            names_of(skills.reachable(&viewer(Some("bob")))),
            ["hello-world"]
        );
        assert_eq!(
            skills
                .set_access(
                    "hello-world",
                    AccessWrite::kept(Visibility::Private, Vec::new()),
                )
                .unwrap_err()
                .to_string(),
            elsewhere("who can use it cannot be changed here")
        );
        assert_eq!(
            skills
                .uninstall(manager(), "hello-world".into())
                .await
                .unwrap_err()
                .to_string(),
            elsewhere("it cannot be uninstalled here")
        );
        assert!(!bundled
            .join("hello-world")
            .join(installer::ACCESS_FILE)
            .exists());
        assert!(bundled.join("hello-world").join("SKILL.md").is_file());
    }

    #[test]
    fn an_ownerless_skill_keeps_its_visibility_but_takes_grantees() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        write_skill(&skills_dir, "org", "org").unwrap();
        let skills = isolated(&skills_dir);

        assert_eq!(
            skills
                .set_access("org", AccessWrite::kept(Visibility::Private, Vec::new()))
                .unwrap_err()
                .to_string(),
            "Skill `org` has no owner, so everyone can use it whatever its visibility. Install it \
             again with `visibility = \"private\"` to make it a private skill of your own."
        );
        assert!(!skills_dir.join("org").join(installer::ACCESS_FILE).exists());

        // A team it cannot have is refused in the sentence both kinds of skill
        // use, rather than in the one about visibility being beside the point.
        assert_eq!(
            skills
                .set_access("org", AccessWrite::kept(Visibility::Team, Vec::new()))
                .unwrap_err()
                .to_string(),
            "Skill `org` has no owner, so it has no team; install it again with \
             `visibility = \"private\"` to make it a skill of your own."
        );

        let shared = skills
            .set_access(
                "org",
                AccessWrite::kept(Visibility::Global, vec![share("bob", GrantLevel::Manage)]),
            )
            .unwrap();
        assert_eq!(member("bob").access(&shared), AccessLevel::Manage);
        assert_eq!(
            member("carol")
                .require(skills.as_ref(), "org", AccessLevel::Manage)
                .unwrap_err(),
            "Skill `org` has no owner — it was installed for the whole org, so only someone with \
             the skills_manage capability can change it."
        );
    }

    #[tokio::test]
    async fn set_access_writes_the_record_and_updates_the_live_set() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        write_skill(&skills_dir, "catalog-echo", "catalog-echo").unwrap();
        let skills = isolated(&skills_dir);
        skills
            .install(
                member("alice"),
                source(tmp.path(), "hello-world", "hello-world", "1.0.0"),
                None,
                None,
            )
            .await
            .unwrap();
        let shares = vec![share("bob", GrantLevel::Manage)];

        let updated = skills
            .set_access(
                "hello-world",
                AccessWrite::kept(Visibility::Global, shares.clone()),
            )
            .unwrap();
        let expected = owned(Some("alice"), Visibility::Global, shares);
        assert_eq!(updated.access, expected);
        assert_eq!(skills.installed("hello-world").unwrap().access, expected);
        assert_eq!(
            read_access(&skills_dir.join("hello-world")).unwrap(),
            expected
        );
        assert_eq!(names(&skills), ["catalog-echo", "hello-world"]);
        assert_eq!(
            ExternalSkills::new(skills_dir.clone(), vec![skills_dir.clone()])
                .installed("hello-world")
                .unwrap()
                .access,
            expected
        );
        assert!(skills
            .set_access("nope", AccessWrite::kept(Visibility::Global, Vec::new()))
            .is_err());
    }

    #[tokio::test]
    async fn place_for_org_keeps_an_owned_record() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);
        skills
            .install(
                member("alice"),
                source(tmp.path(), "hello-world", "hello-world", "1.0.0"),
                None,
                None,
            )
            .await
            .unwrap();
        let record = read_access(&skills_dir.join("hello-world")).unwrap();

        let updated = place_for_org(
            &skills_dir,
            &source(tmp.path(), "hello-world", "hello-world", "2.0.0"),
        )
        .unwrap();
        assert!(updated.replaced);
        assert_eq!(read_access(&updated.path).unwrap(), record);

        let fresh = place_for_org(
            &skills_dir,
            &source(tmp.path(), "catalog-echo", "catalog-echo", "1.0.0"),
        )
        .unwrap();
        assert!(!fresh.path.join(installer::ACCESS_FILE).exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_fresh_installs_of_one_name_leave_one_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);

        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let uid = ["alice", "bob"][i % 2];
                let located = source(
                    tmp.path(),
                    "hello-world",
                    "hello-world",
                    &format!("1.0.{i}"),
                );
                let skills = Arc::clone(&skills);
                tokio::spawn(async move {
                    (uid, skills.install(member(uid), located, None, None).await)
                })
            })
            .collect();
        let mut outcomes = Vec::new();
        for task in tasks {
            outcomes.push(task.await.unwrap());
        }

        let owner = read_access(&skills_dir.join("hello-world"))
            .unwrap()
            .owner
            .expect("an owner");
        for (uid, outcome) in outcomes {
            match uid == owner {
                true => assert!(outcome.is_ok(), "{uid}: {outcome:?}"),
                false => assert!(
                    matches!(outcome, Err(ExternalSkillError::Refused(_))),
                    "{uid}: {outcome:?}"
                ),
            }
        }
        assert_eq!(std::fs::read_dir(&skills_dir).unwrap().count(), 1);
        assert_eq!(
            skills.installed("hello-world").unwrap().access.owner,
            Some(owner)
        );
    }

    /// Replicas share the skills directory and no lock, and one may start at any moment: each
    /// scan here is a replica booting while another reinstalls and removes a private skill.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_replica_starting_mid_write_never_shows_a_private_skill_as_everyones() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = isolated(&skills_dir);
        let install = |version: String| {
            let located = source(tmp.path(), "hello-world", "hello-world", &version);
            skills.install(member("alice"), located, None, None)
        };
        install("1.0.0".into()).await.unwrap();
        let writing = Arc::new(AtomicBool::new(true));
        let scanner = {
            let writing = Arc::clone(&writing);
            std::thread::spawn(move || {
                std::iter::from_fn(|| {
                    writing.load(Ordering::Relaxed).then(|| {
                        isolated(&skills_dir)
                            .installed("hello-world")
                            .map(|skill| skill.access)
                    })
                })
                .flatten()
                .filter(|access| access.owner.is_none())
                .count()
            })
        };
        for i in 1..=100 {
            install(format!("1.0.{i}")).await.unwrap();
            if i % 2 == 0 {
                skills
                    .uninstall(member("alice"), "hello-world".into())
                    .await
                    .unwrap();
            }
        }
        writing.store(false, Ordering::Relaxed);
        assert_eq!(
            scanner.join().unwrap(),
            0,
            "no scan saw the skill ownerless"
        );
    }

    #[test]
    fn may_read_config_needs_manage_for_an_owned_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        write_skill(&skills_dir, "mine", "mine").unwrap();
        write_skill(&skills_dir, "org", "org").unwrap();
        let mine = skills_dir.join("mine");
        record_in(
            &mine,
            &owned(
                Some("alice"),
                Visibility::Private,
                vec![
                    share("bob", GrantLevel::Run),
                    share("carol", GrantLevel::Manage),
                ],
            ),
        );

        for (caller, instructions, config) in [
            (member("alice"), true, true),
            (member("bob"), true, false),
            (member("carol"), true, true),
            (member("dave"), false, false),
            (manager(), true, true),
            (identity_less(), false, false),
        ] {
            assert_eq!(
                may_read_instructions(&caller, &mine),
                instructions,
                "{caller:?}"
            );
            assert_eq!(may_read_config(&caller, &mine), config, "{caller:?}");
        }
        assert!(may_read_config(&member("dave"), &skills_dir.join("org")));
    }
}
