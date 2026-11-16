//! A multi-instance adapter's "no such instance" error (a Linear workspace,
//! a Jira site), made in one place so a program that reads it back (Desk's
//! board sync, through a hub's remote MCP) finds the names in it without
//! guessing at another crate's wording.

/// The error for an instance `name` of `what` ("Linear workspace") that the
/// caller has none of, naming only the caller's own (`available`, sorted).
pub fn unknown_instance_text(what: &str, name: &str, available: &[&str]) -> String {
    if available.is_empty() {
        format!("{what} not configured for you (asked for '{name}'); none of yours is set up.")
    } else {
        format!("Unknown {what}: '{name}'. Available: {available:?}")
    }
}

/// What an unknown-instance error said: which kind of instance ("Linear
/// workspace"), the name asked for, and the caller's own names (none: they
/// have none set up).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownInstance {
    pub what: String,
    pub asked: String,
    pub available: Vec<String>,
}

/// The kinds of instance whose errors a board's sync reads back.
const KINDS: [&str; 2] = ["Linear workspace", "Jira workspace"];

/// Reads [`unknown_instance_text`]'s words back, wherever they sit in
/// `text` (a hub may put its own words around them). `None` for anything
/// else.
pub fn parse_unknown_instance(text: &str) -> Option<UnknownInstance> {
    let quoted = |s: &str| -> Option<String> {
        let rest = s.strip_prefix('\'')?;
        rest.find('\'').map(|end| rest[..end].to_string())
    };
    for what in KINDS {
        let none = format!("{what} not configured for you (asked for ");
        if let Some(at) = text.find(&none) {
            return Some(UnknownInstance {
                what: what.to_string(),
                asked: quoted(&text[at + none.len()..])?,
                available: vec![],
            });
        }
        // An older hub's Jira said a missing default its own way:
        // `Default Jira workspace '<name>' not configured. Available: [..]`.
        let head = format!("Unknown {what}: ");
        let older = format!("Default {what} ");
        let (at, len) = match (text.find(&head), text.find(&older)) {
            (Some(at), _) => (at, head.len()),
            (None, Some(at)) => (at, older.len()),
            (None, None) => continue,
        };
        let rest = &text[at + len..];
        let asked = quoted(rest)?;
        let list = &rest[rest.find("Available: [")? + "Available: [".len()..];
        let list = &list[..list.find(']')?];
        let available: Vec<String> = list
            .split(',')
            .map(|n| n.trim().trim_matches('"').to_string())
            .filter(|n| !n.is_empty())
            .collect();
        return (!available.is_empty()).then(|| UnknownInstance {
            what: what.to_string(),
            asked,
            available,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn its_own_words_read_back() {
        let said = unknown_instance_text(
            "Linear workspace",
            "default",
            &["driftwerk", "mantiscrm", "petmobile"],
        );
        assert_eq!(
            said,
            "Unknown Linear workspace: 'default'. Available: [\"driftwerk\", \"mantiscrm\", \"petmobile\"]"
        );
        let u = parse_unknown_instance(&format!("linear.teams_list failed: {said}")).unwrap();
        assert_eq!(u.what, "Linear workspace");
        assert_eq!(u.asked, "default");
        assert_eq!(u.available, ["driftwerk", "mantiscrm", "petmobile"]);

        let none = unknown_instance_text("Jira workspace", "default", &[]);
        let u = parse_unknown_instance(&none).unwrap();
        assert_eq!(u.what, "Jira workspace");
        assert_eq!(u.asked, "default");
        assert!(u.available.is_empty());
    }

    #[test]
    fn an_older_hub_s_jira_default_reads_too() {
        let u = parse_unknown_instance(
            "Default Jira workspace 'default' not configured. Available: [\"acme\", \"client\"]",
        )
        .unwrap();
        assert_eq!(u.what, "Jira workspace");
        assert_eq!(u.asked, "default");
        assert_eq!(u.available, ["acme", "client"]);
    }

    #[test]
    fn anything_else_is_not_one() {
        assert!(parse_unknown_instance("Issue ENG-1 not found").is_none());
        assert!(parse_unknown_instance("Unknown field: 'x'. Available: [\"a\"]").is_none());
        assert!(parse_unknown_instance("Unknown Linear workspace: 'x'.").is_none());
    }
}
