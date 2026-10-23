//! Owner-only modes for the state the hub keeps for itself.
//!
//! Under privilege separation one `agent` uid runs every dispatched command,
//! for every agent and every user, in the hub's own group, and the hub runs
//! with umask 002 so the trees both of them write stay group-writable. Anything
//! the hub creates is therefore readable from any member's `exec.run` unless it
//! is closed here. A closed directory hides everything beneath it whatever the
//! files' own modes, so a store that owns a tree is closed at its root and the
//! tree is never walked; re-closing on every open is what repairs an older,
//! wider directory on a live install.
//!
//! The agent can also write the trees these positions sit in, so it can plant
//! something there first: a link to a directory it reads, or a directory of its
//! own. What it plants is owned by a uid that is neither root, the hub, nor the
//! owner of the directory it sits in, since the agent only ever writes a
//! hub-made tree through the group. Only such an entry is moved aside and
//! replaced. Anything else is used as it stands: an operator's link to another
//! volume, and every install where ownership does not follow the process uid (a
//! network share mapping or squashing owners, a volume an older image or the
//! host populated), keeps its data where it is.
//!
//! Modes keep out the agent uid, not the agent's own tools: those run as the
//! hub. The positions holding what the hub keeps apart for each person are
//! therefore also withheld from every path a tool is handed
//! ([`withhold_from_tools`]).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};

const PRIVATE_DIR: u32 = 0o700;
const PRIVATE_FILE: u32 = 0o600;
const SQLITE_SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Create `dir` and its parents, then close `dir` to its owner.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    ensure_private_dir_for(dir, effective_uid())
}

fn ensure_private_dir_for(dir: &Path, hub: Option<u32>) -> io::Result<()> {
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    if !claim(dir, hub, Kind::Dir)? {
        created(std::fs::create_dir(dir), dir, hub)?;
    }
    if !std::fs::metadata(dir)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", dir.display()),
        ));
    }
    close(dir, hub, Kind::Dir);
    Ok(())
}

/// Close `dir` to its owner if it exists. For state a configured backend no
/// longer writes but an older install still holds; nothing is created or moved.
pub fn close_private_dir(dir: &Path) {
    close(dir, effective_uid(), Kind::Dir);
}

/// Make the SQLite database at `path` owner-only before it is opened, creating
/// it empty when absent (SQLite treats an empty file as a new database).
/// SQLite gives the journal, WAL and shared-memory files it creates later the
/// database's own mode, so only the ones already on disk are closed here. An
/// in-memory database or a `file:` URI names no file to close.
pub fn ensure_private_sqlite(path: &Path) -> io::Result<()> {
    let names_a_file = path
        .to_str()
        .is_none_or(|p| !p.is_empty() && p != ":memory:" && !p.starts_with("file:"));
    if !names_a_file {
        return Ok(());
    }
    ensure_private_sqlite_for(path, effective_uid())
}

fn ensure_private_sqlite_for(path: &Path, hub: Option<u32>) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    if !claim(path, hub, Kind::File)? {
        let file = private_file_options()
            .write(true)
            .create_new(true)
            .open(path)
            .map(drop);
        created(file, path, hub)?;
    }
    close(path, hub, Kind::File);
    sqlite_sidecars(path)
        .filter(|sidecar| is_real_file(sidecar))
        .for_each(|sidecar| close(&sidecar, hub, Kind::File));
    Ok(())
}

/// Close every SQLite database (and its sidecars) directly inside `dir`, not
/// below it: the stores at the data root that the configured backends no longer
/// open still hold everything they ever recorded.
pub fn close_private_sqlite_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let hub = effective_uid();
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| entry.file_name().to_str().is_some_and(is_sqlite_name))
        .for_each(|entry| close(&entry.path(), hub, Kind::File));
}

/// Open options that create a file owner-only. The mode applies only when the
/// file is created; an existing file keeps its own.
pub fn private_file_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(PRIVATE_FILE);
    }
    options
}

/// What a tool is told when a path leads into a withheld position.
pub const TOOL_REFUSED: &str = "this holds what the hub keeps for each person (conversations, \
     notes, profiles, artifacts, reminders and background jobs), which no tool reads or writes \
     by path beyond a person's own notes and profile in their own turn; use the sessions, notes, \
     artifact, scheduler or jobs tools, which show each person their own";

/// Real locations of the positions withheld from the agent's tools.
static WITHHELD: RwLock<BTreeSet<PathBuf>> = RwLock::new(BTreeSet::new());

/// Real locations of the withheld positions whose entries are each named by
/// the person they hold ([`withhold_per_person`]).
static PER_PERSON: RwLock<BTreeSet<PathBuf>> = RwLock::new(BTreeSet::new());

/// Real locations of the shared notes directories whose dated notes mix
/// whoever spoke that day ([`withhold_dated_notes`]).
static SHARED_NOTES: RwLock<BTreeSet<PathBuf>> = RwLock::new(BTreeSet::new());

/// Withhold `dir` and everything below it from the paths an agent's tools are
/// handed. It is recorded by its real location, so a link leading into it is
/// caught too; one not created yet is recorded where it will be.
pub fn withhold_from_tools(dir: &Path) {
    record(&WITHHELD, dir);
}

/// Withhold `dir` as [`withhold_from_tools`] does, except from the person
/// each entry in it is named for: `dir/<person>/…` or `dir/<person>.md`.
pub fn withhold_per_person(dir: &Path) {
    record(&WITHHELD, dir);
    record(&PER_PERSON, dir);
}

/// Whether `real`, a resolved location, is `person`'s own entry in a
/// position withheld per person.
pub fn is_own_entry(real: &Path, person: &str) -> bool {
    let own = format!("{person}.md");
    PER_PERSON
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter_map(|dir| real.strip_prefix(dir).ok())
        .filter_map(|below| below.components().next())
        .any(|entry| entry.as_os_str() == person || entry.as_os_str() == own.as_str())
}

/// Withhold the dated notes directly in `dir`, a shared notes directory, from
/// the tools of a turn run for a person. They were written before notes were
/// kept per person, and a turn bound to nobody still writes there, so each
/// mixes whoever spoke that day; the shared notes beside them stay open.
pub fn withhold_dated_notes(dir: &Path) {
    record(&SHARED_NOTES, dir);
}

/// Whether `real`, a resolved location, is a dated note in a shared notes
/// directory.
pub fn is_shared_dated_note(real: &Path) -> bool {
    let (Some(dir), Some(name)) = (real.parent(), real.file_name().and_then(|n| n.to_str())) else {
        return false;
    };
    crate::identity::is_daily_note_name(name)
        && SHARED_NOTES
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(dir)
}

/// Record `dir` in `set` by its real location: the deepest part of it that
/// exists, resolved, with the rest where it will be.
fn record(set: &RwLock<BTreeSet<PathBuf>>, dir: &Path) {
    let real = dir
        .ancestors()
        .find_map(|existing| {
            let real = existing.canonicalize().ok()?;
            Some(real.join(dir.strip_prefix(existing).ok()?))
        })
        .unwrap_or_else(|| dir.to_path_buf());
    set.write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(real);
}

/// Close each dated note directly in `dir` to its owner: they hold whoever
/// spoke that day, which no agent command reads.
pub fn close_dated_notes(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let hub = effective_uid();
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(crate::identity::is_daily_note_name)
        })
        .for_each(|entry| close(&entry.path(), hub, Kind::File));
}

/// Whether `real`, a resolved location, is or lies below a withheld position.
pub fn is_withheld_from_tools(real: &Path) -> bool {
    WITHHELD
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .any(|dir| real.starts_with(dir))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Dir,
    File,
}

/// What to do with an entry at a private position, from who owns it, who owns
/// the directory it sits in, and who the process is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    /// Make it owner-only.
    Close,
    /// Use it as it stands: closing it would either fail (the process does not
    /// own it) or, as root, lock its real owner out.
    Keep,
    /// Planted through the group: move it aside and replace it.
    Displace,
}

fn verdict(entry: u32, parent: u32, process: u32) -> Verdict {
    let as_root = process == 0;
    match (trusted(entry, parent, process), as_root) {
        (false, false) => Verdict::Displace,
        (false, true) => Verdict::Keep,
        (true, false) if entry == process => Verdict::Close,
        (true, true) if entry == parent => Verdict::Close,
        (true, _) => Verdict::Keep,
    }
}

/// Whether an owner could have put an entry where it is without the group:
/// root, the process, or whoever owns the directory.
fn trusted(owner: u32, parent: u32, process: u32) -> bool {
    owner == 0 || owner == process || owner == parent
}

/// Whether to make the entry owner-only. A link is followed only when its own
/// owner is trusted: the agent can swap a position for a link at any time, and
/// following its link would chmod whatever it points at (the data root, a
/// shared tree) shut. The target is then closed only where [`verdict`] would
/// close it.
fn closes(link: Option<u32>, target: u32, parent: u32, process: u32) -> bool {
    link.is_none_or(|link| trusted(link, parent, process))
        && verdict(target, parent, process) == Verdict::Close
}

/// Whether an entry is present at `path` (and so is not to be created). One
/// that was planted is moved aside, leaving the position free; if it cannot be
/// moved it is used as it stands, as it was before any of this existed.
fn claim(path: &Path, hub: Option<u32>, kind: Kind) -> io::Result<bool> {
    let entry = match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
        Ok(entry) => entry,
    };
    if ownership(path, &entry, hub).map(|(owner, parent, process)| verdict(owner, parent, process))
        != Some(Verdict::Displace)
    {
        return Ok(true);
    }
    match displace(path) {
        Ok(()) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                kind = ?kind,
                error = %e,
                "Could not move aside an entry the hub does not own at a private position; using it as it is"
            );
            Ok(true)
        }
    }
}

/// The result of creating the entry at `path`. Losing a race to another replica
/// or thread creating the same position leaves it present, which is the goal.
/// Root (a CLI command run through `docker exec`) hands what it creates to the
/// owner of the directory it sits in, as SQLite does with its journals, so the
/// hub is never locked out of a store root made for it.
fn created(result: io::Result<()>, path: &Path, hub: Option<u32>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
        Ok(()) => {
            if hub == Some(0) {
                adopt_parent_owner(path);
            }
            Ok(())
        }
    }
}

/// Make the entry at `path` owner-only when [`verdict`] says to. Best-effort: a
/// position on a host mount the hub does not own cannot be changed, and
/// refusing to start over it would take the install down for a mode
/// `privsep-verify.sh` reports anyway.
fn close(path: &Path, hub: Option<u32>, kind: Kind) {
    let Ok(entry) = std::fs::symlink_metadata(path) else {
        return;
    };
    let Ok(target) = std::fs::metadata(path) else {
        return;
    };
    let of_kind = match kind {
        Kind::Dir => target.is_dir(),
        Kind::File => target.is_file(),
    };
    if !of_kind {
        return;
    }
    let link = entry.file_type().is_symlink();
    match ownership(path, &entry, hub).zip(owner(&target)) {
        Some(((entry_owner, parent, process), target_owner))
            if !closes(link.then_some(entry_owner), target_owner, parent, process) =>
        {
            tracing::warn!(
                path = %path.display(),
                owner = entry_owner,
                target_owner,
                "Hub-private state is owned by another uid and stays as it is"
            );
        }
        _ => restrict(path, mode_of(kind)),
    }
}

/// The owners [`verdict`] weighs for the entry at `path`: its own (the link's,
/// for a link), its parent directory's, and the process's. `None` where
/// ownership is unknown, which is never grounds to move anything.
fn ownership(path: &Path, entry: &std::fs::Metadata, hub: Option<u32>) -> Option<(u32, u32, u32)> {
    let parent = path
        .parent()
        .map(|p| {
            if p.as_os_str().is_empty() {
                Path::new(".")
            } else {
                p
            }
        })
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|meta| owner(&meta))?;
    Some((owner(entry)?, parent, hub?))
}

fn mode_of(kind: Kind) -> u32 {
    match kind {
        Kind::Dir => PRIVATE_DIR,
        Kind::File => PRIVATE_FILE,
    }
}

fn displace(path: &Path) -> io::Result<()> {
    let mut aside = path.as_os_str().to_owned();
    aside.push(format!(".displaced-{}", uuid::Uuid::new_v4().simple()));
    std::fs::rename(path, &aside)?;
    tracing::warn!(
        path = %path.display(),
        moved_to = %Path::new(&aside).display(),
        "Moved aside an entry planted where the hub's private state belongs"
    );
    Ok(())
}

#[cfg(unix)]
fn adopt_parent_owner(path: &Path) {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = path.parent().and_then(|p| std::fs::metadata(p).ok()) else {
        return;
    };
    if let Err(e) = std::os::unix::fs::lchown(path, Some(parent.uid()), Some(parent.gid())) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "Could not hand hub-private state created as root to its directory's owner"
        );
    }
}

#[cfg(not(unix))]
fn adopt_parent_owner(_path: &Path) {}

#[cfg(unix)]
fn owner(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.uid())
}

#[cfg(not(unix))]
fn owner(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn effective_uid() -> Option<u32> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    Some(unsafe { libc::geteuid() })
}

#[cfg(not(unix))]
fn effective_uid() -> Option<u32> {
    None
}

fn sqlite_sidecars(path: &Path) -> impl Iterator<Item = std::path::PathBuf> + '_ {
    SQLITE_SIDECARS.iter().map(move |suffix| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        name.into()
    })
}

fn is_sqlite_name(name: &str) -> bool {
    [".db", ".db-wal", ".db-shm", ".db-journal"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

fn restrict(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Could not make hub-private state owner-only"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

#[cfg(test)]
mod withheld_tests {
    use super::*;

    #[test]
    fn a_withheld_position_covers_what_lies_below_it_and_where_it_will_be() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        std::fs::create_dir(workspace.join("sessions")).unwrap();
        withhold_from_tools(&workspace.join("sessions"));
        withhold_from_tools(&workspace.join("not-yet/deeper/still"));
        withhold_from_tools(&workspace.join("not-yet"));

        assert!(is_withheld_from_tools(&workspace.join("sessions")));
        assert!(is_withheld_from_tools(&workspace.join("sessions/a.jsonl")));
        assert!(is_withheld_from_tools(&workspace.join("not-yet/b")));
        assert!(!is_withheld_from_tools(
            &workspace.join("sessions-notes.md")
        ));
        assert!(!is_withheld_from_tools(&workspace.join("repos/sessions")));
    }

    #[test]
    fn a_per_person_position_gives_each_person_their_own_entry_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        withhold_per_person(&workspace.join("memory/users"));
        withhold_per_person(&workspace.join("users"));

        assert!(is_withheld_from_tools(&workspace.join("users/ada.md")));
        assert!(is_own_entry(
            &workspace.join("memory/users/ada/notes.md"),
            "ada"
        ));
        assert!(is_own_entry(&workspace.join("users/ada.md"), "ada"));
        for other in [
            "memory/users/bob/notes.md",
            "users/bob.md",
            "users",
            "users/ada.md.bak",
        ] {
            assert!(!is_own_entry(&workspace.join(other), "ada"), "{other}");
        }
        assert!(!is_own_entry(&workspace.join("memory/users/ada"), "bob"));
    }

    #[test]
    fn only_the_dated_notes_of_a_shared_notes_directory_are_withheld() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = tmp.path().canonicalize().unwrap().join("memory");
        std::fs::create_dir(&memory).unwrap();
        withhold_dated_notes(&memory);

        assert!(is_shared_dated_note(&memory.join("2026-03-14.md")));
        for open in ["active-task.md", "2026-03-14-compact.md", "2026-13-01.md"] {
            assert!(!is_shared_dated_note(&memory.join(open)), "{open}");
        }
        assert!(!is_shared_dated_note(&memory.join("users/a/2026-03-14.md")));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    }

    fn widen(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    const HUB: u32 = 1000;
    const AGENT: u32 = 1001;

    /// Rows are (entry owner, parent owner, process, verdict). Only an entry
    /// the process, root and the parent's owner all disown is planted, and
    /// root never moves anything or closes what it would lock its owner out of.
    #[test]
    fn only_an_entry_planted_through_the_group_is_moved_aside() {
        let rows = [
            (HUB, HUB, HUB, Verdict::Close),
            (AGENT, HUB, HUB, Verdict::Displace),
            (0, HUB, HUB, Verdict::Keep),
            // A volume an older image, the host or a share mapping populated:
            // everything is one other uid's, entry and directory alike.
            (500, 500, HUB, Verdict::Keep),
            (65534, 65534, HUB, Verdict::Keep),
            // The process is root over a hub-owned tree (`docker exec`).
            (HUB, HUB, 0, Verdict::Close),
            (0, HUB, 0, Verdict::Keep),
            (AGENT, HUB, 0, Verdict::Keep),
            // A hub that runs as root throughout.
            (0, 0, 0, Verdict::Close),
            // The hub sits in a root-owned parent.
            (HUB, 0, HUB, Verdict::Close),
            (AGENT, 0, HUB, Verdict::Displace),
        ];
        for (entry, parent, process, want) in rows {
            assert_eq!(
                verdict(entry, parent, process),
                want,
                "entry {entry}, parent {parent}, process {process}"
            );
        }
    }

    /// Rows are (link owner, target owner, parent owner, process, closes).
    #[test]
    fn a_link_is_followed_only_when_its_own_owner_is_trusted() {
        let rows = [
            (None, HUB, HUB, HUB, true),
            (Some(HUB), HUB, HUB, HUB, true),
            // An operator's relocation.
            (Some(0), HUB, HUB, HUB, true),
            // The agent swapped the position for a link to the data root or a
            // shared tree: following it would shut that tree.
            (Some(AGENT), HUB, HUB, HUB, false),
            (Some(AGENT), HUB, HUB, 0, false),
            (Some(0), 0, HUB, HUB, false),
            (None, AGENT, HUB, HUB, false),
        ];
        for (link, target, parent, process, want) in rows {
            assert_eq!(
                closes(link, target, parent, process),
                want,
                "link {link:?}, target {target}, parent {parent}, process {process}"
            );
        }
    }

    #[test]
    fn ensure_private_dir_closes_a_new_and_an_existing_wide_directory() {
        let root = tempfile::tempdir().unwrap();
        let fresh = root.path().join("a/b");
        ensure_private_dir(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o700);

        let legacy = root.path().join("legacy");
        std::fs::create_dir(&legacy).unwrap();
        widen(&legacy, 0o2775);
        ensure_private_dir(&legacy).unwrap();
        assert_eq!(mode(&legacy), 0o700);
    }

    #[test]
    fn a_link_the_hub_owns_is_followed_like_an_operator_relocation() {
        let root = tempfile::tempdir().unwrap();
        let volume = root.path().join("volume");
        std::fs::create_dir(&volume).unwrap();
        widen(&volume, 0o2775);
        let traces = root.path().join("traces");
        std::os::unix::fs::symlink(&volume, &traces).unwrap();

        ensure_private_dir(&traces).unwrap();

        assert!(std::fs::symlink_metadata(&traces)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(mode(&volume), 0o700);
    }

    /// Seen from a process of another uid, entries owned by whoever owns their
    /// directory (a volume populated by an older image, or by the host) are
    /// neither moved nor emptied, and their modes are left to their owner.
    #[test]
    fn a_tree_owned_by_another_uid_throughout_keeps_its_data_in_place() {
        let root = tempfile::tempdir().unwrap();
        let mine = std::fs::metadata(root.path()).unwrap().uid();
        if mine == 0 {
            return;
        }
        let elsewhere = Some(mine + 1);
        let sessions = root.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        widen(&sessions, 0o2775);
        std::fs::write(sessions.join("s1.jsonl"), b"transcript").unwrap();
        let db = root.path().join("identities.db");
        std::fs::write(&db, b"rows").unwrap();
        widen(&db, 0o664);

        ensure_private_dir_for(&sessions, elsewhere).unwrap();
        ensure_private_sqlite_for(&db, elsewhere).unwrap();

        assert_eq!(
            std::fs::read(sessions.join("s1.jsonl")).unwrap(),
            b"transcript"
        );
        assert_eq!(std::fs::read(&db).unwrap(), b"rows");
        assert_eq!(mode(&sessions), 0o2775);
        assert_eq!(mode(&db), 0o664);
        let displaced = std::fs::read_dir(root.path())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".displaced-"));
        assert!(!displaced);
    }

    /// Moving a planted link aside moves the link, never what it points at.
    #[test]
    fn displacing_a_link_leaves_its_target_untouched() {
        let root = tempfile::tempdir().unwrap();
        let readable = root.path().join("readable");
        std::fs::create_dir(&readable).unwrap();
        std::fs::write(readable.join("note"), b"x").unwrap();
        widen(&readable, 0o2775);
        let users = root.path().join("users");
        std::os::unix::fs::symlink(&readable, &users).unwrap();

        displace(&users).unwrap();
        ensure_private_dir(&users).unwrap();

        let entry = std::fs::symlink_metadata(&users).unwrap();
        assert!(entry.is_dir() && !entry.file_type().is_symlink());
        assert_eq!(mode(&users), 0o700);
        assert_eq!(mode(&readable), 0o2775);
        assert_eq!(std::fs::read(readable.join("note")).unwrap(), b"x");
    }

    /// Two replicas share one data dir and boot together, and every agent
    /// opens the traces root at once: losing the race to create a position
    /// must not fail the store.
    #[test]
    fn concurrent_creators_of_one_position_all_succeed() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("traces");
        let db = root.path().join("approvals.db");
        let barrier = std::sync::Barrier::new(16);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        (ensure_private_dir(&dir), ensure_private_sqlite(&db))
                    })
                })
                .collect();
            for handle in handles {
                let (dir_result, db_result) = handle.join().unwrap();
                dir_result.unwrap();
                db_result.unwrap();
            }
        });
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&db), 0o600);
    }

    #[test]
    fn a_creation_lost_to_another_creator_is_success() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("cc-briefs");
        std::fs::create_dir(&dir).unwrap();
        created(std::fs::create_dir(&dir), &dir, Some(HUB)).unwrap();
        assert!(created(
            std::fs::create_dir(root.path().join("absent/child")),
            &dir,
            Some(HUB)
        )
        .is_err());
    }

    #[test]
    fn a_position_of_the_wrong_kind_is_left_for_its_store_to_report() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        std::fs::write(&sessions, b"not a directory").unwrap();
        widen(&sessions, 0o644);

        assert!(ensure_private_dir(&sessions).is_err());

        assert_eq!(std::fs::read(&sessions).unwrap(), b"not a directory");
        assert_eq!(mode(&sessions), 0o644);
    }

    #[test]
    fn close_private_dir_leaves_an_absent_directory_absent() {
        let root = tempfile::tempdir().unwrap();
        let absent = root.path().join("sessions");
        close_private_dir(&absent);
        assert!(!absent.exists());
    }

    #[test]
    fn ensure_private_sqlite_creates_owner_only_and_closes_existing_sidecars() {
        let root = tempfile::tempdir().unwrap();
        let fresh = root.path().join("fresh.db");
        ensure_private_sqlite(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o600);
        assert_eq!(std::fs::metadata(&fresh).unwrap().len(), 0);

        let legacy = root.path().join("legacy.db");
        std::fs::write(&legacy, b"rows").unwrap();
        let wal = root.path().join("legacy.db-wal");
        std::fs::write(&wal, b"frames").unwrap();
        widen(&legacy, 0o664);
        widen(&wal, 0o664);
        ensure_private_sqlite(&legacy).unwrap();
        assert_eq!(mode(&legacy), 0o600);
        assert_eq!(mode(&wal), 0o600);
        assert_eq!(std::fs::read(&legacy).unwrap(), b"rows");
    }

    #[test]
    fn an_in_memory_database_creates_no_file() {
        let cwd = std::env::current_dir().unwrap();
        ensure_private_sqlite(Path::new(":memory:")).unwrap();
        ensure_private_sqlite(Path::new("file::memory:?cache=shared")).unwrap();
        ensure_private_sqlite(Path::new("")).unwrap();
        assert!(!cwd.join(":memory:").exists());
        assert!(!cwd.join("file::memory:?cache=shared").exists());
    }

    #[test]
    fn a_database_opened_after_ensure_private_sqlite_keeps_its_wal_owner_only() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("store.db");
        ensure_private_sqlite(&db).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);")
            .unwrap();
        let wal = root.path().join("store.db-wal");
        let shm = root.path().join("store.db-shm");
        assert!(wal.exists() && shm.exists());
        assert_eq!(mode(&wal), 0o600);
        assert_eq!(mode(&shm), 0o600);
        assert_eq!(mode(&db), 0o600);
    }

    #[test]
    fn close_private_sqlite_files_closes_databases_at_the_top_level_only() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("approvals.db");
        let shm = root.path().join("approvals.db-shm");
        let other = root.path().join("workforce.overlay.toml");
        let nested = root.path().join("worktrees");
        std::fs::create_dir(&nested).unwrap();
        let nested_db = nested.join("repo.db");
        for file in [&db, &shm, &other, &nested_db] {
            std::fs::write(file, b"x").unwrap();
            widen(file, 0o664);
        }

        close_private_sqlite_files(root.path());

        assert_eq!(mode(&db), 0o600);
        assert_eq!(mode(&shm), 0o600);
        assert_eq!(mode(&other), 0o664);
        assert_eq!(mode(&nested_db), 0o664);
    }
}
