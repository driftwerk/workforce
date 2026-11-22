//! One account's mailbox as its server describes it: what the server can do
//! (CAPABILITY) and its folders with their special uses (LIST, RFC 6154), so
//! nothing depends on which provider hosts it.

use base64::alphabet::IMAP_MUTF7;
use base64::engine::general_purpose::{GeneralPurpose, NO_PAD};
use base64::Engine;
use serde::Serialize;

use super::error::EmailError;
use super::session::{Listed, MailSession};

/// The base64 of IMAP's modified UTF-7 (RFC 3501 §5.1.3).
const MUTF7: GeneralPurpose = GeneralPurpose::new(&IMAP_MUTF7, NO_PAD);

fn printable(c: char) -> bool {
    (' '..='~').contains(&c)
}

/// `name` in modified UTF-7, as a server without `ENABLE UTF8=ACCEPT`
/// (never sent here) takes and lists folder names.
pub fn encoded(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    chars
        .chunk_by(|a, b| printable(*a) == printable(*b))
        .map(|run| match printable(run[0]) {
            true => run.iter().collect::<String>().replace('&', "&-"),
            false => {
                let bytes: Vec<u8> = run
                    .iter()
                    .flat_map(|c| c.encode_utf16(&mut [0u16; 2]).to_vec())
                    .flat_map(u16::to_be_bytes)
                    .collect();
                format!("&{}-", MUTF7.encode(bytes))
            }
        })
        .collect()
}

/// `name` as a person reads it: modified UTF-7 decoded, or as it is where
/// it isn't valid modified UTF-7.
pub fn decoded(name: &str) -> String {
    fn shifted(inside: &str) -> Option<String> {
        if inside.is_empty() {
            return Some("&".to_string());
        }
        let bytes = MUTF7.decode(inside).ok()?;
        let units: Vec<u16> = bytes
            .chunks(2)
            .map(|pair| match pair {
                [hi, lo] => Some(u16::from_be_bytes([*hi, *lo])),
                _ => None,
            })
            .collect::<Option<_>>()?;
        String::from_utf16(&units).ok()
    }
    let mut parts = name.split('&');
    let head = parts.next().unwrap_or_default().to_string();
    parts
        .try_fold(head, |out, part| {
            let (inside, after) = part.split_once('-')?;
            Some(out + &shifted(inside)? + after)
        })
        .unwrap_or_else(|| name.to_string())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Caps {
    /// `UID MOVE` (RFC 6851).
    pub moves: bool,
    /// `UID EXPUNGE` and COPYUID (RFC 4315).
    pub uidplus: bool,
    /// Gmail's extensions (`X-GM-EXT-1`): labels, its own search.
    pub gmail: bool,
}

impl Caps {
    pub fn of<'a>(names: impl IntoIterator<Item = &'a str>) -> Caps {
        names.into_iter().fold(Caps::default(), |caps, name| {
            match name.to_ascii_uppercase().as_str() {
                "MOVE" => Caps {
                    moves: true,
                    ..caps
                },
                "UIDPLUS" => Caps {
                    uidplus: true,
                    ..caps
                },
                "X-GM-EXT-1" => Caps {
                    gmail: true,
                    ..caps
                },
                _ => caps,
            }
        })
    }

    /// Whether this server can take just some messages out of a folder
    /// without touching any other message marked deleted there.
    pub fn moves_safely(&self) -> bool {
        self.moves || self.uidplus
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Inbox,
    All,
    Archive,
    Drafts,
    Flagged,
    Junk,
    Sent,
    Trash,
}

impl Role {
    fn by_attribute(attribute: &str) -> Option<Role> {
        match attribute.to_ascii_lowercase().as_str() {
            "\\all" => Some(Role::All),
            "\\archive" => Some(Role::Archive),
            "\\drafts" => Some(Role::Drafts),
            "\\flagged" => Some(Role::Flagged),
            "\\junk" => Some(Role::Junk),
            "\\sent" => Some(Role::Sent),
            "\\trash" => Some(Role::Trash),
            _ => None,
        }
    }

    /// The role a folder's name suggests, for a server that lists no special
    /// uses.
    pub fn by_name(name: &str) -> Option<Role> {
        if name.eq_ignore_ascii_case("INBOX") {
            return Some(Role::Inbox);
        }
        let leaf = name.rsplit(['/', '.']).next().unwrap_or(name);
        match leaf.to_ascii_lowercase().as_str() {
            "all mail" => Some(Role::All),
            "archive" | "archives" => Some(Role::Archive),
            "drafts" => Some(Role::Drafts),
            "starred" | "flagged" => Some(Role::Flagged),
            "junk" | "junk email" | "junk e-mail" | "spam" | "bulk mail" => Some(Role::Junk),
            "sent" | "sent items" | "sent messages" | "sent mail" => Some(Role::Sent),
            "trash" | "deleted items" | "deleted messages" | "bin" => Some(Role::Trash),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Role::Inbox => "Inbox",
            Role::All => "All Mail",
            Role::Archive => "Archive",
            Role::Drafts => "Drafts",
            Role::Flagged => "Starred",
            Role::Junk => "Spam",
            Role::Sent => "Sent",
            Role::Trash => "Trash",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Folder {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    pub selectable: bool,
    pub has_children: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    pub caps: Caps,
    pub delimiter: Option<String>,
    /// What every folder of the person's but the inbox sits under, where
    /// the server keeps them all inside it (`INBOX.` on Courier and many
    /// Dovecots). Read from LIST: the NAMESPACE response is one async-imap
    /// can't parse.
    pub prefix: Option<String>,
    pub folders: Vec<Folder>,
}

fn starts_with_ignoring_case(name: &str, prefix: &str) -> bool {
    name.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn has(listed: &Listed, attribute: &str) -> bool {
    listed
        .attributes
        .iter()
        .any(|a| a.eq_ignore_ascii_case(attribute))
}

impl Mailbox {
    pub async fn probe(session: &mut (impl MailSession + ?Sized)) -> Result<Mailbox, EmailError> {
        let caps = session.capabilities().await?;
        let listed = session.list().await?;
        Ok(Mailbox::of(caps.iter().map(String::as_str), &listed))
    }

    pub fn of<'a>(caps: impl IntoIterator<Item = &'a str>, listed: &[Listed]) -> Mailbox {
        let delimiter = listed.iter().find_map(|l| l.delimiter.clone());
        let special_use = listed
            .iter()
            .any(|l| l.attributes.iter().any(|a| Role::by_attribute(a).is_some()));
        let child_of = |parent: &str, name: &str| {
            delimiter
                .as_deref()
                .and_then(|d| name.strip_prefix(parent)?.strip_prefix(d))
                .is_some_and(|rest| !rest.is_empty())
        };
        let folders = listed
            .iter()
            .map(|l| {
                let attributed = l.attributes.iter().find_map(|a| Role::by_attribute(a));
                let role = match (l.name.eq_ignore_ascii_case("INBOX"), special_use) {
                    (true, _) => Some(Role::Inbox),
                    (false, true) => attributed,
                    (false, false) => Role::by_name(&l.name),
                };
                Folder {
                    name: l.name.clone(),
                    role,
                    selectable: !has(l, "\\Noselect") && !has(l, "\\NonExistent"),
                    has_children: has(l, "\\HasChildren")
                        || listed.iter().any(|other| child_of(&l.name, &other.name)),
                }
            })
            .collect();
        let prefix = delimiter
            .as_deref()
            .map(|d| format!("INBOX{d}"))
            .filter(|p| {
                let mut others = listed
                    .iter()
                    .filter(|l| !l.name.eq_ignore_ascii_case("INBOX"))
                    .peekable();
                others.peek().is_some() && others.all(|l| starts_with_ignoring_case(&l.name, p))
            });
        Mailbox {
            caps: Caps::of(caps),
            delimiter,
            prefix,
            folders,
        }
    }

    pub fn get(&self, name: &str) -> Option<&Folder> {
        self.folders
            .iter()
            .find(|f| match name.eq_ignore_ascii_case("INBOX") {
                true => f.role == Some(Role::Inbox),
                false => f.name == name,
            })
    }

    /// The folder with `role`, when the account has one.
    pub fn role(&self, role: Role) -> Option<&str> {
        self.folders
            .iter()
            .find(|f| f.role == Some(role) && f.selectable)
            .map(|f| f.name.as_str())
    }

    /// The server's name for `path`: an existing folder's own name as given
    /// or as a person reads it, else `path` with `/` between its levels in
    /// the server's delimiter, in modified UTF-7, under the server's prefix.
    pub fn server_name(&self, path: &str) -> Result<String, String> {
        let path = path.trim().trim_matches('/');
        if path.contains(['\r', '\n', '\0']) {
            return Err("A folder name can't contain a line break.".to_string());
        }
        if let Some(folder) = self.get(path) {
            return Ok(folder.name.clone());
        }
        let levels: Vec<&str> = path.split('/').map(str::trim).collect();
        if path.is_empty() || levels.iter().any(|l| l.is_empty()) {
            return Err(format!("'{path}' is not a folder name."));
        }
        let joined = match self.delimiter.as_deref() {
            None if levels.len() > 1 => Err(format!(
                "This account has no nested folders, so '{path}' can't be made."
            )),
            None => Ok(encoded(path)),
            Some(d) => match levels.iter().find(|l| d != "/" && l.contains(d)) {
                Some(level) => Err(format!(
                    "'{level}' contains '{d}', which this server uses between folder levels."
                )),
                None => Ok(levels
                    .iter()
                    .map(|l| encoded(l))
                    .collect::<Vec<_>>()
                    .join(d)),
            },
        }?;
        let name = match self.prefix.as_deref() {
            Some(p)
                if !joined.eq_ignore_ascii_case("INBOX")
                    && !starts_with_ignoring_case(&joined, p) =>
            {
                format!("{p}{joined}")
            }
            _ => joined,
        };
        Ok(self.get(&name).map_or(name, |f| f.name.clone()))
    }

    /// The folders `name` and its missing parents, outermost first, that a
    /// create makes.
    pub fn to_create(&self, name: &str) -> Vec<String> {
        let ancestors = match self.delimiter.as_deref() {
            Some(d) => name
                .match_indices(d)
                .map(|(i, _)| name[..i].to_string())
                .collect(),
            None => Vec::new(),
        };
        ancestors
            .into_iter()
            .chain(std::iter::once(name.to_string()))
            .filter(|n| self.get(n).is_none())
            .collect()
    }

    /// Whether `name` sits inside a folder that holds no mail of its own,
    /// as Gmail's `[Gmail]/…` system labels do.
    fn under_a_root(&self, name: &str) -> bool {
        self.delimiter
            .as_deref()
            .and_then(|d| name.rsplit_once(d))
            .and_then(|(parent, _)| self.get(parent))
            .is_some_and(|parent| !parent.selectable)
    }

    fn system(&self, folder: &Folder) -> Option<String> {
        let name = decoded(&folder.name);
        match folder.role {
            Some(Role::Inbox) => Some("The inbox can't be renamed or deleted.".to_string()),
            Some(role) => Some(format!(
                "{name} is this account's {} folder, which the provider keeps.",
                role.label()
            )),
            None if self.caps.gmail && (!folder.selectable || self.under_a_root(&folder.name)) => {
                Some(format!("{name} is one of Gmail's own folders."))
            }
            None => None,
        }
    }

    /// Why `from` can't be renamed to `to`, when it can't.
    pub fn refuse_rename(&self, from: &str, to: &str) -> Option<String> {
        let inside = self
            .delimiter
            .as_deref()
            .and_then(|d| to.strip_prefix(from)?.strip_prefix(d))
            .is_some();
        match (self.get(from), self.get(to)) {
            (None, _) => Some(format!("There is no folder {}.", decoded(from))),
            (Some(folder), _) if self.system(folder).is_some() => self.system(folder),
            (_, Some(_)) => Some(format!("A folder {} already exists.", decoded(to))),
            _ if inside => Some(format!("{} can't be moved inside itself.", decoded(from))),
            _ => None,
        }
    }

    /// Why `name`, holding `messages`, can't be deleted, when it can't. Only
    /// an empty folder with no folders inside it is ever deleted.
    pub fn refuse_delete(&self, name: &str, messages: u32) -> Option<String> {
        let folder = match self.get(name) {
            None => return Some(format!("There is no folder {}.", decoded(name))),
            Some(folder) => folder,
        };
        let name = decoded(name);
        match (self.system(folder), folder.has_children, messages) {
            (Some(why), _, _) => Some(why),
            (None, true, _) => Some(format!(
                "{name} has folders inside it; move or delete those first."
            )),
            (None, false, 0) => None,
            (None, false, 1) => Some(format!(
                "{name} still holds 1 message; move it out first, then delete the folder."
            )),
            (None, false, n) => Some(format!(
                "{name} still holds {n} messages; move them out first, then delete the folder."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(name: &str, delimiter: &str, attributes: &[&str]) -> Listed {
        Listed {
            name: name.to_string(),
            delimiter: Some(delimiter.to_string()),
            attributes: attributes.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn gmail() -> Mailbox {
        Mailbox::of(
            ["IMAP4rev1", "UIDPLUS", "MOVE", "X-GM-EXT-1"],
            &[
                listed("INBOX", "/", &["\\HasNoChildren"]),
                listed("[Gmail]", "/", &["\\Noselect", "\\HasChildren"]),
                listed("[Gmail]/All Mail", "/", &["\\All", "\\HasNoChildren"]),
                listed("[Gmail]/Trash", "/", &["\\Trash"]),
                listed("[Gmail]/Important", "/", &["\\Important"]),
                listed("Receipts", "/", &["\\HasChildren"]),
                listed("Receipts/2025", "/", &[]),
                listed("Archive", "/", &[]),
            ],
        )
    }

    fn dovecot() -> Mailbox {
        Mailbox::of(
            ["IMAP4rev1", "UIDPLUS"],
            &[
                listed("INBOX", ".", &[]),
                listed("INBOX.Archive", ".", &[]),
                listed("INBOX.Trash", ".", &[]),
                listed("INBOX.Clients", ".", &[]),
                listed("INBOX.Clients.Acme", ".", &[]),
            ],
        )
    }

    #[test]
    fn gmail_is_known_by_its_capability_not_its_host() {
        assert!(gmail().caps.gmail && gmail().caps.moves);
        assert!(!dovecot().caps.gmail);
        assert!(dovecot().caps.moves_safely());
        assert!(!Caps::of(["IMAP4rev1"]).moves_safely());
    }

    #[test]
    fn special_uses_decide_roles_where_the_server_lists_them() {
        let g = gmail();
        assert_eq!(g.role(Role::All), Some("[Gmail]/All Mail"));
        assert_eq!(g.role(Role::Trash), Some("[Gmail]/Trash"));
        assert_eq!(
            g.role(Role::Archive),
            None,
            "a label named Archive is the person's own"
        );
        assert_eq!(g.role(Role::Inbox), Some("INBOX"));
    }

    #[test]
    fn names_decide_roles_where_the_server_lists_none() {
        let d = dovecot();
        assert_eq!(d.role(Role::Archive), Some("INBOX.Archive"));
        assert_eq!(d.role(Role::Trash), Some("INBOX.Trash"));
        assert_eq!(Role::by_name("Deleted Messages"), Some(Role::Trash));
        assert_eq!(Role::by_name("Sent Items"), Some(Role::Sent));
        assert_eq!(Role::by_name("Clients"), None);
    }

    #[test]
    fn paths_are_written_with_slashes_and_sent_in_the_servers_delimiter() {
        let d = dovecot();
        assert_eq!(
            d.server_name("INBOX/Clients/Beta").unwrap(),
            "INBOX.Clients.Beta"
        );
        assert_eq!(d.server_name("INBOX.Clients").unwrap(), "INBOX.Clients");
        assert!(d.server_name("Mr. Smith").is_err());
        assert!(d.server_name("a//b").is_err());
        assert!(d.server_name(" ").is_err());
        assert_eq!(
            gmail().server_name("Receipts/2026").unwrap(),
            "Receipts/2026"
        );
        assert!(gmail().server_name("a\r\nA1 DELETE INBOX").is_err());
        assert!(gmail().server_name("a\0b").is_err());
    }

    #[test]
    fn a_server_that_keeps_folders_inside_the_inbox_gets_new_ones_there_too() {
        let d = dovecot();
        assert_eq!(d.prefix.as_deref(), Some("INBOX."));
        assert_eq!(
            d.server_name("Projects/2026").unwrap(),
            "INBOX.Projects.2026"
        );
        assert_eq!(d.server_name("inbox/Clients").unwrap(), "INBOX.Clients");
        assert_eq!(d.server_name("INBOX").unwrap(), "INBOX");
        assert_eq!(gmail().prefix, None);
        let bare = Mailbox::of(["IMAP4rev1"], &[listed("INBOX", ".", &[])]);
        assert_eq!(bare.prefix, None, "nothing shows a prefix yet");
    }

    #[test]
    fn names_beyond_ascii_are_sent_in_modified_utf7_and_read_back() {
        assert_eq!(encoded("Reçus"), "Re&AOc-us");
        assert_eq!(encoded("Tom & Jerry"), "Tom &- Jerry");
        assert_eq!(encoded("日本語"), "&ZeVnLIqe-");
        for name in ["Reçus", "Tom & Jerry", "日本語/Ünïcode", "📬 Inbox"] {
            assert_eq!(decoded(&encoded(name)), name);
        }
        assert_eq!(decoded("&broken"), "&broken");
        let g = Mailbox::of(
            ["IMAP4rev1"],
            &[listed("INBOX", "/", &[]), listed("Re&AOc-us", "/", &[])],
        );
        assert_eq!(g.server_name("Reçus").unwrap(), "Re&AOc-us");
        assert_eq!(g.server_name("Re&AOc-us").unwrap(), "Re&AOc-us");
        assert_eq!(g.server_name("Reçus/2026").unwrap(), "Re&AOc-us/2026");
        assert_eq!(
            g.refuse_delete("Re&AOc-us", 2).unwrap(),
            "Reçus still holds 2 messages; move them out first, then delete the folder."
        );
    }

    #[test]
    fn a_nested_create_makes_its_missing_parents_first() {
        let d = dovecot();
        assert_eq!(
            d.to_create("INBOX.Projects.2026.Q1"),
            vec![
                "INBOX.Projects",
                "INBOX.Projects.2026",
                "INBOX.Projects.2026.Q1"
            ]
        );
        assert_eq!(
            d.to_create("INBOX.Clients.Beta"),
            vec!["INBOX.Clients.Beta"]
        );
        assert!(d.to_create("INBOX.Clients").is_empty());
    }

    #[test]
    fn system_folders_and_labels_are_never_renamed_or_deleted() {
        let g = gmail();
        for name in [
            "INBOX",
            "[Gmail]/All Mail",
            "[Gmail]/Trash",
            "[Gmail]",
            "[Gmail]/Important",
        ] {
            assert!(g.refuse_delete(name, 0).is_some(), "{name}");
            assert!(g.refuse_rename(name, "Elsewhere").is_some(), "{name}");
        }
        assert!(g.refuse_rename("Archive", "Old").is_none());
        assert!(g.refuse_rename("Archive", "Receipts").is_some());
        assert!(g.refuse_rename("Nope", "Old").is_some());
        assert!(g
            .refuse_rename("Receipts", "Receipts/2025/Old")
            .unwrap()
            .contains("inside itself"));
        assert!(g.refuse_rename("Receipts/2025", "Receipts2025").is_none());
    }

    #[test]
    fn only_an_empty_folder_with_nothing_inside_is_deleted() {
        let g = gmail();
        assert_eq!(g.refuse_delete("Archive", 0), None);
        assert_eq!(
            g.refuse_delete("Archive", 3).unwrap(),
            "Archive still holds 3 messages; move them out first, then delete the folder."
        );
        assert!(g
            .refuse_delete("Receipts", 0)
            .unwrap()
            .contains("folders inside it"));
        assert!(dovecot()
            .refuse_delete("INBOX.Clients", 0)
            .unwrap()
            .contains("folders inside"));
        assert_eq!(dovecot().refuse_delete("INBOX.Clients.Acme", 0), None);
    }
}
