//! The engines Desk runs sessions on: what each can do in each view, the
//! program it is and how its version reads, and who it is signed in as.
//! Everything that differs by engine is decided here, so the rest of Desk
//! (and the person's Desk app) renders only what [`caps`] says.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::model::text_enum;
use crate::usage::{LocalAccount, LocalPlan};

text_enum!(Engine {
    Claude => "claude",
    Codex => "codex",
    Gemini => "gemini",
    Copilot => "copilot",
    OpenCode => "opencode",
    Cursor => "cursor",
    Devin => "devin",
    Custom => "custom",
});

impl Engine {
    pub const ALL: [Engine; 8] = [
        Engine::Claude,
        Engine::Codex,
        Engine::Gemini,
        Engine::Copilot,
        Engine::OpenCode,
        Engine::Cursor,
        Engine::Devin,
        Engine::Custom,
    ];

    /// What the person calls it.
    pub fn title(self) -> &'static str {
        match self {
            Engine::Claude => "Claude Code",
            Engine::Codex => "Codex",
            Engine::Gemini => "Gemini CLI",
            Engine::Copilot => "Copilot CLI",
            Engine::OpenCode => "OpenCode",
            Engine::Cursor => "Cursor CLI",
            Engine::Devin => "Devin",
            Engine::Custom => "Custom command",
        }
    }

    /// The program it is when the hub's config names none: looked up on
    /// `PATH`. A custom command names its own.
    pub fn default_program(self) -> Option<&'static str> {
        match self {
            Engine::Claude => Some("claude"),
            Engine::Codex => Some("codex"),
            Engine::Gemini => Some("gemini"),
            Engine::Copilot => Some("copilot"),
            Engine::OpenCode => Some("opencode"),
            Engine::Cursor => Some("agent"),
            Engine::Devin => Some("devin"),
            Engine::Custom => None,
        }
    }

    /// The arguments its program prints its version for.
    pub fn version_args(self) -> Option<&'static [&'static str]> {
        match self {
            Engine::Claude | Engine::Codex | Engine::Cursor | Engine::Devin => Some(&["--version"]),
            Engine::Gemini | Engine::Copilot | Engine::OpenCode => Some(&["-v"]),
            Engine::Custom => None,
        }
    }
}

text_enum!(View {
    Terminal => "terminal",
    Desk => "desk",
});

/// How Desk knows a session is working, waiting or done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StateSource {
    /// The engine's own hooks post each step to Desk.
    Hooks,
    /// Desk joins the engine's server and reads its notifications.
    Server,
    None,
}

/// Who answers a session's requests to run a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Answerer {
    /// Desk can answer them, or hand them to the terminal.
    Desk,
    /// Only its terminal asks.
    Terminal,
    None,
}

/// Whether a stopped session can carry on with its conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Resume {
    ById,
    None,
}

/// How Desk ends the turn a session is working on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Interrupt {
    /// Keys written to its terminal ([`interrupt_keys`]).
    Keys,
    None,
}

/// What an engine can do in a view, as Desk drives it today. The person's
/// Desk shows only what this says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Caps {
    pub state: StateSource,
    pub approvals: Answerer,
    pub resume: Resume,
    pub interrupt: Interrupt,
    /// Whose plan's windows its sessions report, for the Floor's plan cards.
    pub plan: Option<LocalPlan>,
    /// What the person reads beside it, in the new session dialog and on
    /// its card.
    pub notes: Vec<&'static str>,
}

impl Caps {
    /// Nothing: an engine in a view Desk doesn't run it in.
    fn none() -> Self {
        Self {
            state: StateSource::None,
            approvals: Answerer::None,
            resume: Resume::None,
            interrupt: Interrupt::None,
            plan: None,
            notes: Vec::new(),
        }
    }
}

/// The views Desk can run `engine` in, the default first; none while Desk
/// has no way to run it yet.
pub fn views(engine: Engine) -> &'static [View] {
    match engine {
        Engine::Claude | Engine::Codex => &[View::Terminal],
        Engine::Gemini
        | Engine::Copilot
        | Engine::OpenCode
        | Engine::Cursor
        | Engine::Devin
        | Engine::Custom => &[],
    }
}

/// The engines Desk can start sessions of, by name: what a tool that
/// starts one offers.
pub fn startable() -> Vec<&'static str> {
    Engine::ALL
        .into_iter()
        .filter(|&e| !views(e).is_empty())
        .map(Engine::as_str)
        .collect()
}

/// Whether Desk can start `engine` in `view`.
pub fn runs(engine: Engine, view: View) -> bool {
    views(engine).contains(&view)
}

/// What `engine` can do in `view`: nothing where Desk doesn't run it there.
pub fn caps(engine: Engine, view: View) -> Caps {
    if !runs(engine, view) {
        return Caps::none();
    }
    match engine {
        Engine::Claude => Caps {
            state: StateSource::Hooks,
            approvals: Answerer::Desk,
            resume: Resume::ById,
            interrupt: Interrupt::Keys,
            plan: Some(LocalPlan::Claude),
            notes: Vec::new(),
        },
        // Codex's app-server sends its approvals to the TUI, which asks.
        Engine::Codex => Caps {
            state: StateSource::Server,
            approvals: Answerer::Terminal,
            resume: Resume::ById,
            interrupt: Interrupt::Keys,
            plan: Some(LocalPlan::Codex),
            notes: vec!["Codex asks its approvals in its own terminal."],
        },
        Engine::Gemini
        | Engine::Copilot
        | Engine::OpenCode
        | Engine::Cursor
        | Engine::Devin
        | Engine::Custom => Caps::none(),
    }
}

/// The bytes that end a working turn in `engine`'s terminal: Esc for both
/// Claude Code and Codex.
pub fn interrupt_keys(engine: Engine) -> Option<&'static [u8]> {
    match caps(engine, View::Terminal).interrupt {
        Interrupt::Keys => Some(b"\x1b"),
        Interrupt::None => None,
    }
}

/// A version an engine reported, as `major.minor.patch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl Version {
    /// The first `n.n[.n]` in what a program printed for its version, as
    /// `claude --version` (`2.1.300 (Claude Code)`) or `codex --version`
    /// (`codex-cli 0.164.0`) print it.
    pub fn read(printed: &str) -> Option<Self> {
        printed
            .split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')')
            .map(|word| word.trim_start_matches('v'))
            .find_map(|word| {
                let mut parts = word.split('.');
                let major = parts.next()?.parse().ok()?;
                let minor = parts.next()?.parse().ok()?;
                let patch = match parts.next() {
                    // `1.2.3-beta.1`: the patch is what comes before the tag.
                    Some(p) => p
                        .split(|c: char| !c.is_ascii_digit())
                        .next()?
                        .parse()
                        .ok()?,
                    None => 0,
                };
                Some(Self {
                    major,
                    minor,
                    patch,
                })
            })
    }
}

text_enum!(Via {
    Plan => "plan",
    Key => "key",
    Vendor => "vendor",
});

/// Who an engine on this computer is signed in as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum SignIn {
    /// Signed in: to a plan, with an API key, or to the vendor's own
    /// account (Devin, Copilot).
    In {
        via: Via,
        /// The account's name as the engine says it (an email).
        who: Option<String>,
        /// The plan's name, when it says (`max`, `pro`).
        plan: Option<String>,
        /// The ids its account is known by, raw: plan readings are keyed
        /// by them in the hub, never shown.
        #[serde(skip)]
        ids: Vec<String>,
    },
    Out,
    /// It says nothing Desk can read, or it hasn't been asked yet.
    Unknown,
}

impl SignIn {
    /// The plan account it is signed in to, for the plan readings its
    /// sessions report.
    pub fn plan_account(&self) -> Option<LocalAccount> {
        match self {
            SignIn::In {
                via: Via::Plan,
                ids,
                plan,
                ..
            } => Some(LocalAccount {
                ids: ids.clone(),
                plan: plan.clone(),
            }),
            SignIn::In { .. } | SignIn::Out | SignIn::Unknown => None,
        }
    }

    /// Whether it is signed in to a plan.
    pub fn on_plan(&self) -> bool {
        matches!(self, SignIn::In { via: Via::Plan, .. })
    }
}

/// What every engine on this computer says of its sign-in, as last read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignIns {
    pub by_engine: BTreeMap<Engine, SignIn>,
}

impl SignIns {
    pub fn of(&self, engine: Engine) -> &SignIn {
        self.by_engine.get(&engine).unwrap_or(&SignIn::Unknown)
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeStatus {
    #[serde(default)]
    logged_in: bool,
    auth_method: Option<String>,
    org_id: Option<String>,
    email: Option<String>,
    subscription_type: Option<String>,
}

/// The sign-ins that are a plan's: the claude.ai login and a plan's token.
const PLAN_AUTH: [&str; 2] = ["claude.ai", "oauth_token"];

/// Claude Code's `auth status --json`.
pub fn claude_sign_in(status: &str) -> SignIn {
    let Ok(s) = serde_json::from_str::<ClaudeStatus>(status) else {
        return SignIn::Unknown;
    };
    if !s.logged_in {
        return SignIn::Out;
    }
    let via = match s.auth_method.as_deref() {
        Some(m) if PLAN_AUTH.contains(&m) => Via::Plan,
        _ => Via::Key,
    };
    SignIn::In {
        via,
        who: s.email.clone().filter(|e| !e.trim().is_empty()),
        plan: s
            .subscription_type
            .filter(|p| !p.trim().is_empty() && via == Via::Plan),
        ids: [s.org_id, s.email].into_iter().flatten().collect(),
    }
}

/// `codex login status`, which Codex prints on stdout or stderr.
pub fn codex_sign_in(status: &str) -> SignIn {
    let said = |words: &str| {
        status
            .lines()
            .any(|l| l.trim().to_lowercase().starts_with(words))
    };
    let signed_in = |via| SignIn::In {
        via,
        who: None,
        plan: None,
        ids: Vec::new(),
    };
    if said("logged in using chatgpt") {
        signed_in(Via::Plan)
    } else if said("logged in using an api key") {
        signed_in(Via::Key)
    } else if said("not logged in") {
        SignIn::Out
    } else {
        SignIn::Unknown
    }
}

/// Devin keeps its login in `credentials.toml` under its data folder
/// (`$XDG_DATA_HOME/devin`, else `~/.local/share/devin`).
pub fn devin_sign_in(env: &BTreeMap<String, String>, exists: impl Fn(&Path) -> bool) -> SignIn {
    let data = env
        .get("XDG_DATA_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| env.get("HOME").map(|h| Path::new(h).join(".local/share")));
    match data {
        Some(data) if exists(&data.join("devin/credentials.toml")) => SignIn::In {
            via: Via::Vendor,
            who: None,
            plan: None,
            ids: Vec::new(),
        },
        Some(_) => SignIn::Out,
        None => SignIn::Unknown,
    }
}

/// Copilot CLI signs in with a GitHub token from its environment; with
/// none set, only it can say.
pub fn copilot_sign_in(env: &BTreeMap<String, String>) -> SignIn {
    let token_set = ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .any(|k| env.get(*k).is_some_and(|v| !v.is_empty()));
    match token_set {
        true => SignIn::In {
            via: Via::Vendor,
            who: None,
            plan: None,
            ids: Vec::new(),
        },
        false => SignIn::Unknown,
    }
}

/// What Desk read of one engine on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// Its version; `None` when its program didn't run or said none.
    pub version: Option<Version>,
    pub sign_in: SignIn,
}

/// One engine as Settings → Engines and the new session dialog show it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineView {
    pub engine: Engine,
    pub title: &'static str,
    /// Its program, as the hub runs it; none for a custom command.
    pub program: Option<String>,
    /// Whether its program runs here; `None` until Desk has looked.
    pub installed: Option<bool>,
    pub version: Option<Version>,
    pub sign_in: SignIn,
    /// The views Desk starts it in, the default first: none while Desk
    /// can't start it.
    pub views: Vec<View>,
    /// What it can do in each of those views.
    pub caps: BTreeMap<View, Caps>,
}

/// `engine` as Desk shows it, from what was last read of it.
pub fn view_of(engine: Engine, program: Option<&Path>, read: Option<&Installed>) -> EngineView {
    let views = views(engine).to_vec();
    EngineView {
        engine,
        title: engine.title(),
        program: program.map(|p| p.display().to_string()),
        installed: read.map(|r| r.version.is_some()),
        version: read.and_then(|r| r.version),
        sign_in: read.map_or(SignIn::Unknown, |r| r.sign_in.clone()),
        caps: views.iter().map(|&v| (v, caps(engine, v))).collect(),
        views,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_claude_code_and_codex_run_and_each_in_a_terminal() {
        for engine in Engine::ALL {
            let expected: &[View] = match engine {
                Engine::Claude | Engine::Codex => &[View::Terminal],
                _ => &[],
            };
            assert_eq!(views(engine), expected, "{engine:?}");
            assert!(!runs(engine, View::Desk));
        }
        assert_eq!(startable(), ["claude", "codex"]);
        assert_eq!(caps(Engine::Gemini, View::Terminal), Caps::none());
        assert_eq!(caps(Engine::Claude, View::Desk), Caps::none());
    }

    #[test]
    fn claude_code_answers_in_desk_and_codex_in_its_terminal() {
        let claude = caps(Engine::Claude, View::Terminal);
        assert_eq!(
            (claude.state, claude.approvals, claude.resume, claude.plan),
            (
                StateSource::Hooks,
                Answerer::Desk,
                Resume::ById,
                Some(LocalPlan::Claude)
            )
        );
        let codex = caps(Engine::Codex, View::Terminal);
        assert_eq!(
            (codex.state, codex.approvals, codex.plan),
            (
                StateSource::Server,
                Answerer::Terminal,
                Some(LocalPlan::Codex)
            )
        );
        assert!(!codex.notes.is_empty(), "the card says why");
        assert_eq!(interrupt_keys(Engine::Claude), Some(&b"\x1b"[..]));
        assert_eq!(interrupt_keys(Engine::Devin), None);
    }

    #[test]
    fn every_engine_round_trips_and_has_a_title() {
        for engine in Engine::ALL {
            assert_eq!(Engine::parse(engine.as_str()).unwrap(), engine);
            assert!(!engine.title().is_empty());
            assert_eq!(
                serde_json::to_value(engine).unwrap(),
                serde_json::json!(engine.as_str())
            );
            assert_eq!(
                engine.default_program().is_some(),
                engine.version_args().is_some()
            );
        }
        assert!(Engine::parse("grok").is_err());
    }

    #[test]
    fn versions_are_read_as_each_cli_prints_them() {
        let v = |s| Version::read(s).map(|v| v.to_string());
        assert_eq!(v("2.1.300 (Claude Code)").as_deref(), Some("2.1.300"));
        assert_eq!(v("codex-cli 0.164.0\n").as_deref(), Some("0.164.0"));
        assert_eq!(v("v1.4.2-beta.3").as_deref(), Some("1.4.2"));
        assert_eq!(v("opencode 2.0").as_deref(), Some("2.0.0"));
        assert_eq!(v("command not found"), None);
        assert!(Version::read("2.1.287").unwrap() < Version::read("2.1.300").unwrap());
    }

    #[test]
    fn claude_codes_sign_in_is_a_plan_or_a_key() {
        let plan = claude_sign_in(
            r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty",
            "email":"person@example.com","orgId":"org-1","subscriptionType":"max"}"#,
        );
        assert_eq!(
            plan,
            SignIn::In {
                via: Via::Plan,
                who: Some("person@example.com".into()),
                plan: Some("max".into()),
                ids: vec!["org-1".into(), "person@example.com".into()],
            }
        );
        assert_eq!(
            plan.plan_account(),
            Some(LocalAccount {
                ids: vec!["org-1".into(), "person@example.com".into()],
                plan: Some("max".into())
            })
        );
        let token = claude_sign_in(r#"{"loggedIn":true,"authMethod":"oauth_token"}"#);
        assert!(token.on_plan());
        assert_eq!(token.plan_account(), Some(LocalAccount::default()));
        let key = claude_sign_in(r#"{"loggedIn":true,"authMethod":"api_key"}"#);
        assert!(matches!(key, SignIn::In { via: Via::Key, .. }));
        assert_eq!(key.plan_account(), None, "an API key is no plan");
        assert_eq!(claude_sign_in(r#"{"loggedIn":false}"#), SignIn::Out);
        assert_eq!(claude_sign_in("not json"), SignIn::Unknown);
    }

    #[test]
    fn codex_says_its_sign_in_in_words() {
        assert!(codex_sign_in("Logged in using ChatGPT\n").on_plan());
        assert!(matches!(
            codex_sign_in("Logged in using an API key - sk-…\n"),
            SignIn::In { via: Via::Key, .. }
        ));
        assert_eq!(codex_sign_in("Not logged in\n"), SignIn::Out);
        assert_eq!(codex_sign_in(""), SignIn::Unknown);
    }

    #[test]
    fn devin_and_copilot_are_read_from_their_files_and_environment() {
        let env = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let home = env(&[("HOME", "/home/allan")]);
        let at = |want: &'static str| move |p: &Path| p == Path::new(want);
        assert!(matches!(
            devin_sign_in(&home, at("/home/allan/.local/share/devin/credentials.toml")),
            SignIn::In {
                via: Via::Vendor,
                ..
            }
        ));
        assert_eq!(devin_sign_in(&home, |_| false), SignIn::Out);
        let xdg = env(&[("HOME", "/home/allan"), ("XDG_DATA_HOME", "/data")]);
        assert!(matches!(
            devin_sign_in(&xdg, at("/data/devin/credentials.toml")),
            SignIn::In { .. }
        ));
        assert_eq!(devin_sign_in(&env(&[]), |_| true), SignIn::Unknown);
        assert!(matches!(
            copilot_sign_in(&env(&[("GH_TOKEN", "x")])),
            SignIn::In { .. }
        ));
        assert_eq!(copilot_sign_in(&env(&[("GH_TOKEN", "")])), SignIn::Unknown);
    }

    #[test]
    fn an_engines_view_says_what_was_read_and_never_its_ids() {
        let read = Installed {
            version: Version::read("2.1.300"),
            sign_in: claude_sign_in(
                r#"{"loggedIn":true,"authMethod":"claude.ai","email":"a@example.com","orgId":"org-9"}"#,
            ),
        };
        let v = view_of(Engine::Claude, Some(Path::new("claude")), Some(&read));
        assert_eq!(v.installed, Some(true));
        assert_eq!(v.views, vec![View::Terminal]);
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["version"], "2.1.300");
        assert_eq!(json["signIn"]["state"], "in");
        assert_eq!(json["signIn"]["via"], "plan");
        assert!(!json.to_string().contains("org-9"), "{json}");
        assert_eq!(json["caps"]["terminal"]["approvals"], "desk");
        let unread = view_of(Engine::Gemini, Some(Path::new("gemini")), None);
        assert_eq!(
            (unread.installed, unread.views.len(), unread.caps.len()),
            (None, 0, 0)
        );
        let gone = view_of(
            Engine::Codex,
            Some(Path::new("codex")),
            Some(&Installed {
                version: None,
                sign_in: SignIn::Unknown,
            }),
        );
        assert_eq!(gone.installed, Some(false));
    }
}
