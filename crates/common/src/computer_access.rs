//! `[computer]` in an agent's `config.toml`: how far the agent reaches on
//! this computer, read only on a hub with `[desk]` enabled.
//!
//! ```toml
//! [computer]
//! access = "full"            # own_folder | read_anywhere | full
//! asks = "irreversible"      # at full: irreversible (the default) | never
//! ask_in_own_folder = false  # true: changes in its own folder ask too
//! unattended = "run"         # at full, with nobody to ask: run | refuse
//! works_in = ["/Users/sarah/dev/acme-web"]  # folders it works in too
//! ```
//!
//! `works_in` names folders (a repo cloned into one, say) the agent reads,
//! changes and runs in beside its own, at every level, and starts coding
//! work in. Only the person sets it: the agent's own config file is never
//! changed by its tools. Never `/`, a volume, the home folder or anything
//! above it, nor anything Desk keeps from every agent; deleting there still
//! asks.
//!
//! An agent with no `[computer]` is at Full access and asks only before what
//! can't be undone (the always-asks list): everything else it did before
//! the table existed runs as it did. `asks = "never"` asks before nothing.
//! A server hub never reads it.
//!
//! The table is kept as a raw value and checked here, so what is wrong with
//! it is said in plain words, without key names: Desk shows the refusal to
//! the person as it is. Keys this hub does not know are ignored.

use std::path::{Component, Path, PathBuf};

use toml::Value;
use workforce_core::computer::{Access, Asking, ComputerAccess, Unattended};

type Table = toml::map::Map<String, Value>;

const ASKS: &str =
    "What the agent asks you about first must be irreversible things only, or never.";
const UNATTENDED: &str = "What the agent does when nobody can be asked must be run or refuse.";
const LEVEL: &str = "How far the agent reaches on this computer must be its own folder, read anywhere or full access.";

fn switch(t: &Table, key: &str, default: bool, what: &str) -> Result<bool, String> {
    match t.get(key) {
        None => Ok(default),
        Some(Value::Boolean(b)) => Ok(*b),
        Some(_) => Err(format!(
            "Whether {what} is on or off must be true or false."
        )),
    }
}

/// The `[computer]` table as the hub runs it, or what is wrong with it.
pub fn from_value(value: &Value) -> Result<ComputerAccess, String> {
    let Value::Table(t) = value else {
        return Err("The settings for this computer need to be a group of settings.".into());
    };
    let d = ComputerAccess::default();
    let access = match t.get("access") {
        None => d.access,
        Some(Value::String(s)) => Access::parse(s.trim()).ok_or_else(|| LEVEL.to_string())?,
        Some(_) => return Err(LEVEL.into()),
    };
    let autonomous = switch(t, "autonomous", d.autonomous, "working on its own")?;
    if autonomous && access != Access::Full {
        return Err("An agent can work on its own only with full access to this computer.".into());
    }
    let unattended = match t.get("unattended") {
        None => d.unattended,
        Some(Value::String(s)) => {
            Unattended::parse(s.trim()).ok_or_else(|| UNATTENDED.to_string())?
        }
        Some(_) => return Err(UNATTENDED.into()),
    };
    let asks = match t.get("asks") {
        None => d.asks,
        Some(Value::String(s)) => Asking::parse(s.trim()).ok_or_else(|| ASKS.to_string())?,
        Some(_) => return Err(ASKS.into()),
    };
    Ok(ComputerAccess {
        access,
        autonomous,
        unattended,
        asks,
        ask_in_own_folder: switch(
            t,
            "ask_in_own_folder",
            d.ask_in_own_folder,
            "asking before changes in its own folder",
        )?,
    })
}

/// `[computer]` as a config holds it: absent is Full access, asking first.
pub fn of(value: Option<&Value>) -> Result<ComputerAccess, String> {
    value.map_or_else(|| Ok(ComputerAccess::default()), from_value)
}

/// The table Desk writes for `access`, every key spelled out.
pub fn to_value(access: &ComputerAccess) -> Value {
    let mut t = Table::new();
    t.insert(
        "access".into(),
        Value::String(access.access.as_str().into()),
    );
    t.insert("autonomous".into(), Value::Boolean(access.autonomous));
    t.insert(
        "ask_in_own_folder".into(),
        Value::Boolean(access.ask_in_own_folder),
    );
    t.insert(
        "unattended".into(),
        Value::String(access.unattended.as_str().into()),
    );
    t.insert("asks".into(), Value::String(access.asks.as_str().into()));
    Value::Table(t)
}

const WORKS_IN: &str = "The folders the agent works in must be a list of full folder paths.";

/// The most folders an agent works in.
pub const MAX_WORKS_IN: usize = 16;

/// `[computer] works_in` as written: full paths, none twice, at most
/// [`MAX_WORKS_IN`], or what is wrong with it. Where each may be is
/// [`works_in_refusal`]'s to say: that needs the machine's home and what is
/// withheld on it.
pub fn works_in(value: Option<&Value>) -> Result<Vec<PathBuf>, String> {
    let Some(Value::Table(t)) = value else {
        return Ok(Vec::new());
    };
    let list = match t.get("works_in") {
        None => return Ok(Vec::new()),
        Some(Value::Array(list)) => list,
        Some(_) => return Err(WORKS_IN.into()),
    };
    if list.len() > MAX_WORKS_IN {
        return Err(format!("An agent works in at most {MAX_WORKS_IN} folders."));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in list {
        let Value::String(raw) = entry else {
            return Err(WORKS_IN.into());
        };
        let path = PathBuf::from(raw.trim());
        let plain = path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
        if !path.is_absolute() || !plain {
            return Err(format!(
                "{} isn't a full folder path: start it at / and leave out . and ..",
                raw.trim()
            ));
        }
        if !out.contains(&path) {
            out.push(path);
        }
    }
    Ok(out)
}

/// Under home, what no folder an agent works in may be, sit in or hold,
/// beside the secret and start-file folders a boss never touches: the rest
/// of the person's own configuration, and the engines' sign-ins.
const KEPT_IN_HOME: &[&str] = &[
    ".config",
    ".claude",
    ".claude.json",
    ".local/share/keyrings",
];

/// Why the agent may not work in `folder` on a machine whose home is `home`
/// and that keeps `withheld` (Desk's data, the keychains) from every agent;
/// `None` when it may. It must be inside home, and not home itself; never a
/// secret or start-file folder (`~/.ssh`, `~/.aws`, `~/.config`, shell rc
/// files, `~/Library/LaunchAgents`, …) nor inside or holding one; never
/// inside or holding what is withheld. Checked as written and with its links
/// followed, names compared as a case-insensitive disk (APFS) compares them.
pub fn works_in_refusal(folder: &Path, home: &Path, withheld: &[PathBuf]) -> Option<String> {
    use workforce_core::home_secrets::{fold, SECRET_DIRS, START_FILES};
    let real = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    let home_real = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let shown = folder.display();
    let homes = [fold(home), fold(&home_real)];
    let kept: Vec<PathBuf> = SECRET_DIRS
        .iter()
        .chain(START_FILES)
        .chain(KEPT_IN_HOME)
        .map(|k| fold(Path::new(k)))
        .collect();
    let withheld: Vec<PathBuf> = withheld
        .iter()
        .flat_map(|w| {
            [
                fold(w),
                fold(&std::fs::canonicalize(w).unwrap_or_else(|_| w.clone())),
            ]
        })
        .collect();
    for path in [fold(folder), fold(&real)] {
        // Inside home, below it.
        let Some(rest) = homes.iter().find_map(|h| {
            path.strip_prefix(h)
                .ok()
                .filter(|rest| !rest.as_os_str().is_empty())
        }) else {
            return Some(format!(
                "{shown} isn't inside your home folder: choose a folder inside it, not your home itself or anything outside it."
            ));
        };
        if kept
            .iter()
            .any(|k| rest.starts_with(k) || k.starts_with(rest))
        {
            return Some(format!(
                "{shown} is or holds where your keys, sign-ins or shell settings are kept, which no agent works in."
            ));
        }
        if withheld
            .iter()
            .any(|k| path.starts_with(k) || k.starts_with(&path))
        {
            return Some(format!(
                "{shown} holds Desk's own data or your keychains, which no agent works in."
            ));
        }
    }
    None
}

/// What no agent reaches on a machine whose hub keeps its data in
/// `data_dir`: Desk's data (on Desk the hub's data is `…/Desk/hub/data`, so
/// Desk's folder is two up; elsewhere the hub's data folder alone) and the
/// keychains. Paths as given; callers resolve them as they compare.
pub fn withheld_on(data_dir: &Path, home: &Path) -> Vec<PathBuf> {
    let data = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let desk = data
        .parent()
        .filter(|hub| hub.file_name().is_some_and(|n| n == "hub"))
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    vec![desk.unwrap_or(data), home.join("Library/Keychains")]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Result<ComputerAccess, String> {
        let doc: Table = toml::from_str(source).unwrap();
        of(doc.get("computer"))
    }

    #[test]
    fn absent_is_full_access_asking_first() {
        let none = parse("").unwrap();
        assert_eq!(none.access, Access::Full);
        assert!(!none.autonomous);
        assert!(!none.ask_in_own_folder);
        // Where nobody can be asked, it runs as before.
        assert_eq!(none.unattended, Unattended::Run);
        // At Full access it asks only before what can't be undone.
        assert_eq!(none.asks, Asking::Irreversible);
        assert_eq!(
            parse("[computer]\nasks = \"never\"\n").unwrap().asks,
            Asking::Never
        );
        assert_eq!(parse("[computer]\n").unwrap(), none);
    }

    #[test]
    fn each_level_and_switch_reads() {
        for (level, access) in [
            ("own_folder", Access::OwnFolder),
            ("read_anywhere", Access::ReadAnywhere),
            ("full", Access::Full),
        ] {
            let read = parse(&format!(
                "[computer]\naccess = \"{level}\"\nask_in_own_folder = true\n"
            ))
            .unwrap();
            assert_eq!(read.access, access);
            assert!(read.ask_in_own_folder);
        }
        let own =
            parse("[computer]\naccess = \"full\"\nautonomous = true\nfuture_key = 1\n").unwrap();
        assert!(own.autonomous);
        assert_eq!(
            parse("computer = { access = \"own_folder\" }\n")
                .unwrap()
                .access,
            Access::OwnFolder
        );
    }

    #[test]
    fn every_value_round_trips_through_the_table_desk_writes() {
        for access in [Access::OwnFolder, Access::ReadAnywhere, Access::Full] {
            for autonomous in [false, true] {
                for (ask_in_own_folder, unattended, asks) in [
                    (false, Unattended::Run, Asking::Irreversible),
                    (true, Unattended::Refuse, Asking::Never),
                ] {
                    let want = ComputerAccess {
                        access,
                        autonomous,
                        ask_in_own_folder,
                        unattended,
                        asks,
                    };
                    let written = to_value(&want);
                    let read = from_value(&written);
                    if autonomous && access != Access::Full {
                        assert!(read.is_err());
                        continue;
                    }
                    assert_eq!(read.unwrap(), want);
                    let body =
                        toml::to_string(&Table::from_iter([("computer".into(), written)])).unwrap();
                    assert_eq!(parse(&body).unwrap(), want, "{body}");
                }
            }
        }
    }

    #[test]
    fn what_is_wrong_is_said_in_plain_words() {
        for (source, says) in [
            (
                "[computer]\naccess = \"everything\"\n",
                "own folder, read anywhere or full",
            ),
            (
                "[computer]\naccess = 3\n",
                "own folder, read anywhere or full",
            ),
            ("[computer]\nautonomous = \"yes\"\n", "working on its own"),
            (
                "[computer]\naccess = \"read_anywhere\"\nautonomous = true\n",
                "only with full access",
            ),
            ("[computer]\nask_in_own_folder = 1\n", "true or false"),
            ("[computer]\nunattended = \"ask\"\n", "run or refuse"),
            (
                "[computer]\nasks = \"always\"\n",
                "irreversible things only, or never",
            ),
            ("computer = \"full\"\n", "group of settings"),
        ] {
            let e = parse(source).unwrap_err();
            assert!(e.contains(says), "{source} → {e}");
            for key in [
                "access =",
                "autonomous",
                "ask_in_own_folder",
                "own_folder",
                "read_anywhere",
                "[",
            ] {
                assert!(!e.contains(key), "{source} → {e} names {key}");
            }
        }
    }

    fn folders(source: &str) -> Result<Vec<PathBuf>, String> {
        let doc: Table = toml::from_str(source).unwrap();
        works_in(doc.get("computer"))
    }

    #[test]
    fn works_in_reads_full_paths_once_each() {
        assert!(folders("").unwrap().is_empty());
        assert!(folders("[computer]\naccess = \"full\"\n")
            .unwrap()
            .is_empty());
        assert_eq!(
            folders(
                "[computer]\nworks_in = [\"/Users/sarah/dev/acme\", \" /Users/sarah/dev/acme \", \"/srv/repo\"]\n"
            )
            .unwrap(),
            [PathBuf::from("/Users/sarah/dev/acme"), PathBuf::from("/srv/repo")]
        );
        for (source, says) in [
            (
                "[computer]\nworks_in = \"/srv\"\n",
                "list of full folder paths",
            ),
            ("[computer]\nworks_in = [3]\n", "list of full folder paths"),
            (
                "[computer]\nworks_in = [\"dev/acme\"]\n",
                "isn't a full folder path",
            ),
            (
                "[computer]\nworks_in = [\"~/dev/acme\"]\n",
                "isn't a full folder path",
            ),
            (
                "[computer]\nworks_in = [\"/Users/sarah/dev/../..\"]\n",
                "isn't a full folder path",
            ),
        ] {
            let e = folders(source).unwrap_err();
            assert!(e.contains(says), "{source} → {e}");
            assert!(!e.contains("works_in"), "{e} names the key");
        }
        let many: Vec<String> = (0..=MAX_WORKS_IN)
            .map(|i| format!("\"/srv/r{i}\""))
            .collect();
        assert!(
            folders(&format!("[computer]\nworks_in = [{}]\n", many.join(",")))
                .unwrap_err()
                .contains("at most")
        );
    }

    #[test]
    fn works_in_is_inside_home_and_never_what_desk_or_the_person_keeps() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("sarah");
        let data = home.join("Library/Application Support/Desk/hub/data");
        let repo = home.join("dev/acme");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        let withheld = withheld_on(&data, &home);
        assert!(works_in_refusal(&repo, &home, &withheld).is_none());
        assert!(works_in_refusal(&home.join("Dev/Acme"), &home, &withheld).is_none());
        // Home itself, above it, and anywhere outside it.
        for wide in [
            PathBuf::from("/"),
            home.clone(),
            tmp.path().to_path_buf(),
            PathBuf::from("/Volumes/Backup"),
            PathBuf::from("/srv/repo"),
            PathBuf::from("/tmp/acme"),
        ] {
            let e = works_in_refusal(&wide, &home, &withheld).expect("refused");
            assert!(
                e.contains("isn't inside your home"),
                "{} → {e}",
                wide.display()
            );
        }
        // The secret and start-file folders, inside them, holding them, in
        // any case.
        for kept in [
            ".ssh",
            ".ssh/keys",
            ".SSH",
            ".aws",
            ".Aws/sso/cache",
            ".config",
            ".config/gh",
            ".Config/anything",
            ".gnupg",
            ".kube",
            ".docker",
            ".zshrc",
            ".bashrc",
            ".gitconfig",
            ".claude",
            ".cargo",
            "Library",
            "Library/LaunchAgents",
        ] {
            let e = works_in_refusal(&home.join(kept), &home, &withheld)
                .unwrap_or_else(|| panic!("{kept} admitted"));
            assert!(
                e.contains("keys, sign-ins or shell settings") || e.contains("Desk's own data"),
                "{kept} → {e}"
            );
        }
        // What Desk withholds, and what holds it.
        for kept in [
            home.join("Library/Application Support/Desk/hub/data/agents/ben"),
            home.join("Library/Application Support"),
            home.join("library/application support/desk"),
        ] {
            let e = works_in_refusal(&kept, &home, &withheld).expect("refused");
            assert!(e.contains("Desk's own data"), "{} → {e}", kept.display());
        }
        // A link into what is kept is followed.
        #[cfg(unix)]
        {
            let link = home.join("dev/sneaky");
            std::os::unix::fs::symlink(&data, &link).unwrap();
            assert!(works_in_refusal(&link, &home, &withheld).is_some());
            let ssh = home.join(".ssh");
            std::fs::create_dir_all(&ssh).unwrap();
            let to_ssh = home.join("dev/keys");
            std::os::unix::fs::symlink(&ssh, &to_ssh).unwrap();
            assert!(works_in_refusal(&to_ssh, &home, &withheld).is_some());
        }
    }
}
