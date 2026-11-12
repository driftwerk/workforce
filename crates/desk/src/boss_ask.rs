//! A request a session asks the person, sent up a boss link so the person
//! can answer it from anywhere (remote bosses design §7), and how this
//! engine checks the answer that comes back ([`accept_remote`]).
//!
//! The boss hub is trusted to say who answered, and nothing more:
//! - each ask carries a fresh nonce, kept here only as its hash; an answer
//!   must bring it, and it is good once (the row is closed as it is
//!   taken), so an answer is for this ask and is never replayed;
//! - the answer must name the link's own person, as the hub said at
//!   `ready`;
//! - what an answer from elsewhere may do is the control: narrowing
//!   (deny, stop the session) from anywhere; allowing only an ask of the
//!   [`Class::Anywhere`] class. A [`Class::DeskOnly`] ask is answered in
//!   Desk, whatever the hub says.
//!
//! Pure; the store is [`crate::grant_store`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::never::Never;

/// Where an ask may be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// From Slack, Desk or the web: the card says what trusting the boss's
    /// hub means.
    Anywhere,
    /// Only in Desk: an answer from elsewhere may only narrow it.
    DeskOnly,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anywhere => "anywhere",
            Self::DeskOnly => "desk_only",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "anywhere" => Some(Self::Anywhere),
            "desk_only" => Some(Self::DeskOnly),
            _ => None,
        }
    }
}

/// An answer to an ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    /// Let the session do it, this once.
    Allow,
    /// Refuse it; the session goes on.
    Deny,
    /// Refuse it and stop the session.
    Stop,
}

impl Choice {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Stop => "stop",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }

    /// Whether it only stops work: a forged one costs nothing but that.
    pub fn narrows(self) -> bool {
        matches!(self, Self::Deny | Self::Stop)
    }
}

/// The answers an ask of `class` offers from elsewhere.
pub fn options(class: Class) -> Vec<Choice> {
    match class {
        Class::Anywhere => vec![Choice::Allow, Choice::Deny, Choice::Stop],
        Class::DeskOnly => vec![Choice::Deny, Choice::Stop],
    }
}

/// The class of a request, from the hard limit it runs into, if any, and
/// whether it ships (a push, a merge, a release, a publish, a deploy) in a
/// group whose person keeps shipping to Desk. Every hard limit is the
/// person's in Desk: what widens what anyone may do, or can't be taken
/// back, is never allowed on another hub's word.
pub fn class_of(never: Option<Never>, ships: bool, shipping_needs_desk: bool) -> Class {
    if never.is_some() || (ships && shipping_needs_desk) {
        Class::DeskOnly
    } else {
        Class::Anywhere
    }
}

/// The programs whose verbs below ship.
const SHIPPERS: [&str; 20] = [
    "git",
    "gh",
    "glab",
    "npm",
    "pnpm",
    "yarn",
    "bun",
    "cargo",
    "docker",
    "podman",
    "terraform",
    "tofu",
    "helm",
    "kubectl",
    "twine",
    "poetry",
    "flyctl",
    "fly",
    "vercel",
    "netlify",
];

/// What a shipper does when it ships.
const SHIP_VERBS: [&str; 8] = [
    "push", "merge", "publish", "deploy", "release", "apply", "upload", "workflow",
];

/// Whether `command` reads as shipping: a push, a merge, a release, a
/// publish, an apply or a deploy, by any of the shippers, wherever its
/// options put the verb, and inside a `sh -c` too. Loose on purpose: a
/// false catch costs one answer in Desk.
pub fn ships(command: &str) -> bool {
    ships_within(command, 0)
}

fn ships_within(command: &str, depth: u8) -> bool {
    let c = command.to_ascii_lowercase();
    if c.contains("publish") || c.contains("deploy") {
        return true;
    }
    if depth > 3 {
        return true;
    }
    let words: Vec<String> =
        shlex::split(&c).unwrap_or_else(|| c.split_whitespace().map(String::from).collect());
    let mut shipper = false;
    for w in &words {
        // A command inside a command (`sh -c "…"`, `env … git push`).
        if w.contains(char::is_whitespace) && ships_within(w, depth + 1) {
            return true;
        }
        for part in w.split([';', '&', '|', '\n']) {
            if part.is_empty() {
                shipper = false;
                continue;
            }
            let program = part.rsplit('/').next().unwrap_or(part);
            if SHIPPERS.contains(&program) {
                shipper = true;
            } else if shipper && SHIP_VERBS.contains(&part) {
                return true;
            }
        }
    }
    false
}

/// The longest a decision's fields may be.
const FIELD_CHARS: usize = 128;

/// The hash a nonce is kept as.
pub fn nonce_hash(nonce: &str) -> String {
    hex::encode(Sha256::digest(nonce.as_bytes()))
}

/// Whether `a` and `b` are the same, in time that doesn't depend on where
/// they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// An ask as this engine keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub id: Uuid,
    pub link_id: Uuid,
    pub group: String,
    pub session: Uuid,
    /// The request it asks about.
    pub approval: Uuid,
    pub nonce_hash: String,
    pub class: Class,
    pub options: Vec<Choice>,
    /// `open`, or how it closed.
    pub state: String,
    pub expires_at: DateTime<Utc>,
}

/// Who answered, as the boss hub says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decider {
    pub identity_id: String,
    /// `slack`, `desk` or `web`.
    pub via: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack_user_id: Option<String>,
}

/// An answer, as a `decision` frame brings it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub ask_id: Uuid,
    pub nonce: String,
    pub choice: String,
    pub decider: Decider,
    #[serde(default)]
    pub decided_at: Option<DateTime<Utc>>,
}

/// Why an answer isn't taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// It is answered, withdrawn or expired.
    NotOpen,
    /// It came on another link than the ask went up.
    OtherLink,
    Expired,
    /// Not this ask's nonce.
    WrongNonce,
    /// Not the link's person.
    NotThePerson,
    /// Allowing it is the person's in Desk.
    DeskOnly,
    /// Not one of its answers.
    NotAnOption,
    /// Taken, but the session couldn't be answered so: the person answers
    /// it in Desk.
    NotApplied,
}

impl Refusal {
    /// In the words the boss hub's card shows.
    pub fn say(self) -> &'static str {
        match self {
            Self::NotOpen => "Already answered.",
            Self::OtherLink | Self::WrongNonce => "That answer isn't for this request.",
            Self::Expired => "It waited too long: answer it in Desk.",
            Self::NotThePerson => "Only the person this boss works for answers it.",
            Self::DeskOnly => "Decide this in Desk.",
            Self::NotAnOption => "That isn't one of its answers.",
            Self::NotApplied => "It couldn't be done: answer it in Desk.",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotOpen => "not_open",
            Self::OtherLink => "other_link",
            Self::Expired => "expired",
            Self::WrongNonce => "wrong_nonce",
            Self::NotThePerson => "not_the_person",
            Self::DeskOnly => "desk_only",
            Self::NotAnOption => "not_an_option",
            Self::NotApplied => "not_applied",
        }
    }
}

/// Whether `decision`, come on `link` whose person the boss hub said is
/// `person`, answers `ask` now, and with what. The caller takes the ask
/// (closing it) only after this, so its nonce is good once.
pub fn accept_remote(
    ask: &Ask,
    decision: &Decision,
    link: Uuid,
    person: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Choice, Refusal> {
    // Another link learns nothing of this one's asks, not even whether
    // they are open.
    if ask.link_id != link || decision.ask_id != ask.id {
        return Err(Refusal::OtherLink);
    }
    let d = &decision.decider;
    let long = |s: &str| s.len() > FIELD_CHARS;
    if long(&decision.nonce)
        || long(&decision.choice)
        || long(&d.identity_id)
        || long(&d.via)
        || d.slack_user_id.as_deref().is_some_and(long)
    {
        return Err(Refusal::NotAnOption);
    }
    if ask.state != "open" {
        return Err(Refusal::NotOpen);
    }
    if now >= ask.expires_at {
        return Err(Refusal::Expired);
    }
    if !same(
        nonce_hash(&decision.nonce).as_bytes(),
        ask.nonce_hash.as_bytes(),
    ) {
        return Err(Refusal::WrongNonce);
    }
    match person {
        Some(p) if !p.is_empty() && p == decision.decider.identity_id => {}
        _ => return Err(Refusal::NotThePerson),
    }
    let choice = Choice::parse(&decision.choice).ok_or(Refusal::NotAnOption)?;
    if ask.class == Class::DeskOnly && !choice.narrows() {
        return Err(Refusal::DeskOnly);
    }
    if !ask.options.contains(&choice) {
        return Err(Refusal::NotAnOption);
    }
    Ok(choice)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(class: Class) -> Ask {
        Ask {
            id: Uuid::from_u128(1),
            link_id: Uuid::from_u128(2),
            group: "acme".into(),
            session: Uuid::from_u128(3),
            approval: Uuid::from_u128(4),
            nonce_hash: nonce_hash("n0nce"),
            class,
            options: options(class),
            state: "open".into(),
            expires_at: Utc::now() + chrono::Duration::minutes(30),
        }
    }

    fn decision(choice: &str) -> Decision {
        Decision {
            ask_id: Uuid::from_u128(1),
            nonce: "n0nce".into(),
            choice: choice.into(),
            decider: Decider {
                identity_id: "sam".into(),
                via: "slack".into(),
                slack_user_id: Some("U1".into()),
            },
            decided_at: None,
        }
    }

    #[test]
    fn an_answer_is_taken_only_for_its_open_ask_on_its_link_from_its_person() {
        let a = ask(Class::Anywhere);
        let link = a.link_id;
        let now = Utc::now();
        let take = |a: &Ask, d: &Decision, link, person| accept_remote(a, d, link, person, now);
        assert_eq!(
            take(&a, &decision("allow"), link, Some("sam")),
            Ok(Choice::Allow)
        );
        assert_eq!(
            take(&a, &decision("stop"), link, Some("sam")),
            Ok(Choice::Stop)
        );

        let mut closed = a.clone();
        closed.state = "answered_in_desk".into();
        assert_eq!(
            take(&closed, &decision("allow"), link, Some("sam")),
            Err(Refusal::NotOpen)
        );
        assert_eq!(
            take(&a, &decision("allow"), Uuid::from_u128(9), Some("sam")),
            Err(Refusal::OtherLink)
        );
        let mut other = decision("allow");
        other.ask_id = Uuid::from_u128(7);
        assert_eq!(take(&a, &other, link, Some("sam")), Err(Refusal::OtherLink));
        let mut late = a.clone();
        late.expires_at = now;
        assert_eq!(
            take(&late, &decision("allow"), link, Some("sam")),
            Err(Refusal::Expired)
        );
        let mut forged = decision("allow");
        forged.nonce = "n0ncf".into();
        assert_eq!(
            take(&a, &forged, link, Some("sam")),
            Err(Refusal::WrongNonce)
        );
        forged.nonce = String::new();
        assert_eq!(
            take(&a, &forged, link, Some("sam")),
            Err(Refusal::WrongNonce)
        );
        assert_eq!(
            take(&a, &decision("allow"), link, Some("bea")),
            Err(Refusal::NotThePerson)
        );
        assert_eq!(
            take(&a, &decision("allow"), link, None),
            Err(Refusal::NotThePerson)
        );
        let mut nobody = decision("allow");
        nobody.decider.identity_id = String::new();
        assert_eq!(
            take(&a, &nobody, link, Some("")),
            Err(Refusal::NotThePerson)
        );
        assert_eq!(
            take(&a, &decision("always"), link, Some("sam")),
            Err(Refusal::NotAnOption)
        );
    }

    #[test]
    fn what_widens_is_answered_in_desk_and_narrowing_it_is_taken_from_anywhere() {
        let a = ask(Class::DeskOnly);
        let now = Utc::now();
        assert_eq!(
            accept_remote(&a, &decision("allow"), a.link_id, Some("sam"), now),
            Err(Refusal::DeskOnly)
        );
        for narrows in ["deny", "stop"] {
            assert!(accept_remote(&a, &decision(narrows), a.link_id, Some("sam"), now).is_ok());
        }
        // An ask whose options leave allowing out is refused it, whatever
        // its class says.
        let mut no_allow = ask(Class::Anywhere);
        no_allow.options = vec![Choice::Deny];
        assert_eq!(
            accept_remote(
                &no_allow,
                &decision("allow"),
                no_allow.link_id,
                Some("sam"),
                now
            ),
            Err(Refusal::NotAnOption)
        );
    }

    #[test]
    fn a_request_is_desk_only_when_it_widens_or_ships_where_shipping_needs_desk() {
        assert_eq!(class_of(None, false, false), Class::Anywhere);
        assert_eq!(
            class_of(Some(Never::Secrets), false, false),
            Class::DeskOnly
        );
        assert_eq!(class_of(Some(Never::Money), false, false), Class::DeskOnly);
        // Every hard limit, those that can't be taken back too.
        for n in crate::never::ALL {
            assert_eq!(class_of(Some(n), false, false), Class::DeskOnly, "{n:?}");
        }
        assert_eq!(class_of(None, true, false), Class::Anywhere);
        assert_eq!(class_of(None, true, true), Class::DeskOnly);
        assert!(ships("git push origin main"));
        assert!(ships("gh pr merge 42 --squash"));
        assert!(ships("npm publish"));
        assert!(!ships("cargo test"));
        // Wherever the options put the verb, and inside a shell.
        for c in [
            "git -C repo push",
            "git  push",
            "git -c core.x=y push origin",
            "/usr/bin/git push",
            "docker push ghcr.io/acme/api",
            "terraform apply -auto-approve",
            "cd api && git push",
            "bash -c 'git push --tags'",
            "gh workflow run release.yml",
        ] {
            assert!(ships(c), "{c}");
        }
        assert!(!ships("git status && cargo build"));
        assert!(!ships("echo push"));
        assert_eq!(options(Class::DeskOnly), vec![Choice::Deny, Choice::Stop]);
    }
}
