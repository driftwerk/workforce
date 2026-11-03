//! `[computer]` in an agent's `config.toml`: how far the agent reaches on
//! this computer, read only on a hub with `[desk]` enabled.
//!
//! ```toml
//! [computer]
//! access = "full"            # own_folder | read_anywhere | full
//! asks = "irreversible"      # at full: irreversible (the default) | never
//! ask_in_own_folder = false  # true: changes in its own folder ask too
//! unattended = "run"         # at full, with nobody to ask: run | refuse
//! ```
//!
//! An agent with no `[computer]` is at Full access and asks only before what
//! can't be undone (the always-asks list): everything else it did before
//! the table existed runs as it did. `asks = "never"` asks before nothing.
//! A server hub never reads it.
//!
//! The table is kept as a raw value and checked here, so what is wrong with
//! it is said in plain words, without key names: Desk shows the refusal to
//! the person as it is. Keys this hub does not know are ignored.

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
}
