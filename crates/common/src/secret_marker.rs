//! Session-scoped secret tokenization.
//!
//! Bridges three boundaries:
//!  1. **Ingress** (Slack, web, voice, NATS chat, tool-result content):
//!     when a credential pattern is detected in inbound text, the literal
//!     value is replaced with an opaque [`Marker`] (`[[secret:<id> ...]]`)
//!     and the cleartext is stashed in a per-session [`SecretCache`].
//!  2. **LLM context** sees only markers — never the cleartext. Markers
//!     persist into the session transcript and provider logs.
//!  3. **Tool dispatch** for an allowlist of secret-sink tools (vault.set,
//!     system.add_account, slack.connect) substitutes markers back
//!     to cleartext per declared field, just before invoking the tool.
//!
//! The cache lives in `workforce-hub` and is wired up at process boot via
//! [`set_global_cache`] so the agent-runtime substituter can reach it
//! without depending on hub.
//!
//! Threat model is the same as [`crate::secret::Secret`] — operator-grade
//! hygiene, not cryptographic memory protection. Cleartext lives in
//! process memory while a marker is alive; the cache TTL bounds the
//! window. Markers themselves are safe to log (they carry only a partial
//! preview) and safe to serialize into transcripts.

use std::sync::OnceLock;
use std::sync::{Arc, RwLock};

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::secret::Secret;

/// Identifier for a cached secret.
///
/// Two disjoint forms share the type:
///  * **auto** — a 128-bit value rendered as 32 lowercase hex chars,
///    minted by [`MarkerId::derived`] so equal values in one session share
///    an id (which reveals equality, never content) and equal values in
///    different sessions do not; [`MarkerId::fresh`] exists for callers
///    that need an id unrelated to any value;
///  * **named** — a user-chosen handle from `/secret <name> <value>`,
///    built via [`MarkerId::named`]. Validation forbids the 32-hex shape
///    so the two keyspaces never collide.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MarkerId(String);

impl MarkerId {
    /// Generate a fresh 128-bit id. Uses uuid v4 for entropy and emits
    /// the simple lowercase-hex form (no dashes) so it slots cleanly into
    /// the marker grammar.
    pub fn fresh() -> Self {
        let bytes = uuid::Uuid::new_v4().into_bytes();
        Self(hex::encode(bytes))
    }

    /// The auto id for `cleartext` in the session keyed by `session_key`: a
    /// domain-separated BLAKE3 hash truncated to 128 bits. The same value
    /// tokenised again in the same session maps to the marker — and cache
    /// entry — it already has, so a replayed message renders the same
    /// bytes; the same value in another session is unrelated.
    pub fn derived(session_key: &str, cleartext: &str) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key("workforce secret marker v1");
        hasher.update(session_key.as_bytes());
        hasher.update(&[0]);
        hasher.update(cleartext.as_bytes());
        Self(hex::encode(&hasher.finalize().as_bytes()[..16]))
    }

    /// Build a named id from a user-supplied handle. Accepts
    /// `^[a-z][a-z0-9_]{0,63}$` and rejects the 32-hex auto form so a
    /// name can never shadow (or be mistaken for) an auto id.
    pub fn named(name: &str) -> Result<Self, NamedSecretError> {
        let mut bytes = name.bytes();
        let valid = name.len() <= 64
            && bytes.next().is_some_and(|b| b.is_ascii_lowercase())
            && bytes.all(|b| b == b'_' || b.is_ascii_lowercase() || b.is_ascii_digit());
        if !valid {
            return Err(NamedSecretError::InvalidName);
        }
        if is_auto_form(name) {
            return Err(NamedSecretError::ReservedName);
        }
        Ok(Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this id is a `/secret` name rather than an auto id. Used
    /// by the cache to scope named lookups to the channel while keeping
    /// auto ids strictly session-scoped.
    pub fn is_named(&self) -> bool {
        !is_auto_form(&self.0)
    }
}

/// True when `s` is exactly 32 lowercase hex characters — the shape
/// [`MarkerId::fresh`] emits.
fn is_auto_form(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Why a `/secret` name was rejected. Surfaced verbatim to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NamedSecretError {
    /// Outside `^[a-z][a-z0-9_]{0,63}$`.
    InvalidName,
    /// Matches the reserved 32-hex auto-id form.
    ReservedName,
}

impl std::fmt::Display for NamedSecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NamedSecretError::InvalidName => f.write_str(
                "must be 1-64 characters of lowercase letters, digits or underscore, starting with a letter",
            ),
            NamedSecretError::ReservedName => {
                f.write_str("looks like a reserved auto-generated id; pick a different name")
            }
        }
    }
}

impl std::error::Error for NamedSecretError {}

impl std::fmt::Display for MarkerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a captured secret originated. The variant determines whether
/// (and how) the user can be nudged to clean up the source — a Slack
/// paste needs a "delete the original message" prompt; a tool-result
/// capture is server-side and needs no user action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecretSource {
    /// Pasted into a Slack channel/thread. `message_ts` lets us build a
    /// permalink for the "please delete" prompt.
    Slack {
        workspace_id: String,
        channel: String,
        message_ts: String,
        user: String,
    },
    /// Came from a UI/web client message.
    WebGateway { user: String },
    /// Voice transcript — typically can't contain a real key but covered
    /// for completeness.
    Voice { user: String },
    /// Came from a NATS chat-worker enqueue (e.g. another agent surface).
    NatsChat { user: String },
    /// Supplied via `/secret <name> <value>`. The value is typed into a
    /// slash command, not posted as a channel message, so there is
    /// nothing to "delete".
    SlashCommand { user: String },
    /// Detected inside a tool's response content (web.fetch, web.search,
    /// http_request, …). Source can't be "deleted" — the secret was on
    /// the remote page.
    ToolResult { tool: String },
    /// Catch-all for paths that haven't been classified yet. Treated the
    /// same as ToolResult for UX (no delete prompt) — but NOT for
    /// [`SecretSource::is_tool_scraped`], because every path stamping it
    /// today is a user message. A capture path that reads external content
    /// must stamp `ToolResult`, not this.
    Unknown,
}

impl SecretSource {
    /// Whether the user can/should be prompted to delete the source.
    pub fn supports_delete_prompt(&self) -> bool {
        matches!(self, SecretSource::Slack { .. })
    }

    /// Whether the platform scraped this value out of content the agent
    /// fetched, rather than a human handing it over.
    ///
    /// The distinction matters wherever a marker may be sent to a destination
    /// the *agent* picked: a value the user supplied carries their intent about
    /// where it goes, while a token lifted from a remote response carries none
    /// — and that same response could be the injection telling the agent where
    /// to send it.
    ///
    /// Only [`SecretSource::ToolResult`] qualifies. [`SecretSource::Unknown`]
    /// deliberately does not: every path that stamps it today is a *user
    /// message* whose surface didn't classify itself further (gateway, Slack
    /// follow-up, the defence-in-depth pass in `serve`), so treating it as
    /// scraped would refuse the user's own pasted credential. A new capture
    /// path that reads external content must stamp `ToolResult`.
    pub fn is_tool_scraped(&self) -> bool {
        matches!(self, SecretSource::ToolResult { .. })
    }
}

/// Metadata returned to the caller of [`tokenize_secrets`] so it can
/// surface a user-facing confirmation/cleanup prompt. Carries no
/// cleartext.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedSecret {
    pub id: MarkerId,
    pub label: String,
    pub preview: String,
    pub source: SecretSource,
}

/// The serialized marker form embedded in text destined for the LLM.
///
/// Wire form: `[[secret:<id> type=<LABEL> preview=<PREVIEW>]]`
///
/// Example: `[[secret:8a3f...0c2e type=ANTHROPIC_KEY preview=sk-an...wXyZ]]`
///
/// Choices:
///  * `[[ ... ]]` delimiters are uncommon in normal prose / code and don't
///    collide with markdown.
///  * `type=` and `preview=` are space-separated key=value with no
///    quoting — neither value can contain spaces or `]`.
///  * Preview is purely informational; the substitution layer never
///    parses it (only the id is load-bearing). We always *write* both
///    decorations, and read a marker with or without them — a model quoting an
///    id back into a later tool call should not have to reproduce a preview
///    verbatim to authenticate a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub id: MarkerId,
    pub label: String,
    pub preview: String,
}

impl Marker {
    /// Render to the wire form `parse_markers` reads back. Named markers
    /// use the minimal `[[secret:<name>]]` shape (their value never
    /// transited the LLM, so there is no label/preview to carry); auto
    /// markers use the full `[[secret:<id> type=L preview=P]]` form.
    pub fn render(&self) -> String {
        if self.id.is_named() {
            format!("[[secret:{}]]", self.id)
        } else {
            format!(
                "[[secret:{} type={} preview={}]]",
                self.id, self.label, self.preview
            )
        }
    }
}

impl std::fmt::Display for Marker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render())
    }
}

/// Tight enough that random `[[ ... ]]` text won't false-match.
///
/// `type=`/`preview=` are **optional**. Only the id is load-bearing (see
/// [`Marker`]) — they exist so the rendered marker reads as a credential to a
/// human and to the model, not because resolution needs them. Requiring them
/// made a model echoing a marker back into a later tool call reproduce two
/// decorations verbatim, and one that wrote the id alone hit a coin flip: a
/// bare id beginning `a`–`f` matched the *named* grammar below and resolved
/// anyway, while one beginning `0`–`9` matched nothing and failed the call.
/// Identical intent, opposite outcome, decided by the first character of a
/// random id. Accepting the bare form grants no reference the decorated form
/// did not: lookup is by id against the same session cache, and every
/// destination check downstream is unchanged.
static MARKER_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\[\[secret:([0-9a-f]{32})(?: type=([A-Z0-9_]+) preview=([^\] ]+))?\]\]")
        .expect("marker regex must compile")
});

/// Label carried by an auto marker parsed in its bare form, which has no
/// `type=` to read.
pub const UNDECORATED_LABEL: &str = "SECRET";

/// Preview shown for a marker that carries none. Never a slice of the value.
const UNDECORATED_PREVIEW: &str = "***";

/// Minimal form a `/secret` name resolves to. The agent references a
/// named secret as `[[secret:<name>]]`; no `type`/`preview` because the
/// value never transited the LLM, so there is no preview to render.
///
/// The 32-hex shape is excluded at parse time by [`is_auto_form`], not by this
/// pattern (the regex crate has no lookahead). [`MarkerId::named`] already
/// refuses that shape when *minting* a name, so filtering here is what makes
/// parsing agree with minting: the hex keyspace belongs to the auto grammar
/// alone, and a bare id can never be read as a name that merely looks like one.
static NAMED_MARKER_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\[\[secret:([a-z][a-z0-9_]{0,63})\]\]").expect("named marker regex must compile")
});

/// Label carried by parsed named markers and stored on their cache
/// records.
pub const NAMED_LABEL: &str = "NAMED";

/// Minimum cleartext length for [`SecretCache::redact_known`] to act on a
/// value. Exact-match redaction of a very short value (a 4-digit PIN, the
/// word "test") would corrupt ordinary prose and tool output, so we only
/// scrub values at or above this length. Real credentials — API keys, DB
/// passwords, tokens — comfortably clear it; a rare short secret stays
/// protected by the marker grammar at its sink instead.
pub const MIN_KNOWN_REDACT_LEN: usize = 8;

/// Parse all markers out of a string. Recognises both the auto form
/// (`[[secret:<hex> type=L preview=P]]`) and the named form
/// (`[[secret:<name>]]`), returned in order of appearance. Named matches
/// overlapping an auto span are dropped so the two grammars can't
/// double-count the same region.
pub fn parse_markers(text: &str) -> Vec<Marker> {
    parse_markers_spanned(text)
        .into_iter()
        .map(|(_, marker)| marker)
        .collect()
}

/// [`parse_markers`], but each marker is paired with the byte range it
/// occupies in `text`.
///
/// A caller that substitutes more than one marker grammar in the same string
/// needs ranges rather than `render()` text: splicing every match of every
/// grammar in one pass over the *original* input is what guarantees no
/// resolver ever re-scans another's substituted cleartext.
pub fn parse_markers_spanned(text: &str) -> Vec<(std::ops::Range<usize>, Marker)> {
    let mut found: Vec<(std::ops::Range<usize>, Marker)> = MARKER_REGEX
        .captures_iter(text)
        .map(|c| {
            let span = c.get(0).expect("capture 0 present");
            (
                span.range(),
                Marker {
                    id: MarkerId(c[1].to_string()),
                    label: c
                        .get(2)
                        .map_or(UNDECORATED_LABEL, |m| m.as_str())
                        .to_string(),
                    preview: c
                        .get(3)
                        .map_or(UNDECORATED_PREVIEW, |m| m.as_str())
                        .to_string(),
                },
            )
        })
        .collect();
    let auto_spans: Vec<(usize, usize)> = MARKER_REGEX
        .find_iter(text)
        .map(|m| (m.start(), m.end()))
        .collect();
    found.extend(NAMED_MARKER_REGEX.captures_iter(text).filter_map(|c| {
        let span = c.get(0).expect("capture 0 present");
        let overlaps = auto_spans
            .iter()
            .any(|(s, e)| span.start() < *e && *s < span.end());
        // An id in the auto keyspace is never a name, however name-shaped it
        // looks. Without this, `[[secret:a17b…]]` (32 hex, leading `a`) parses
        // as a named marker while `[[secret:217b…]]` parses as nothing.
        (!overlaps && !is_auto_form(&c[1])).then(|| {
            (
                span.range(),
                Marker {
                    id: MarkerId(c[1].to_string()),
                    label: NAMED_LABEL.to_string(),
                    preview: "***".to_string(),
                },
            )
        })
    }));
    found.sort_by_key(|(range, _)| range.start);
    found
}

/// Replace every marker in `text` with a human form. Used at outbound
/// user surfaces so a leaked marker reads as `sk-an...wXyZ` (auto) or its
/// bare name (named) rather than the noisy bracket form.
pub fn render_markers_as_preview(text: &str) -> String {
    // A closure, not `"$3"`: `preview` is optional, and the literal expansion
    // renders a bare marker as the empty string — silently deleting the fact
    // that a credential stood there.
    let auto = MARKER_REGEX.replace_all(text, |c: &regex::Captures<'_>| {
        c.get(3)
            .map_or(UNDECORATED_PREVIEW, |m| m.as_str())
            .to_string()
    });
    NAMED_MARKER_REGEX.replace_all(&auto, "$1").into_owned()
}

/// Build the partial-preview form `first4...last4` for a captured
/// secret. Values shorter than 16 chars collapse to `***` to avoid
/// leaking via the prefix. Three ASCII dots, deliberately matching the
/// shape the operator asked for (`abcd...wxyz`).
pub fn preview_of(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() < 16 {
        return "***".to_string();
    }
    let head: String = chars.iter().take(4).collect();
    let tail: String = chars
        .iter()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}...{tail}")
}

// ───────────────────────────────────────────────────────────────────────
// SecretCache trait + global accessor
// ───────────────────────────────────────────────────────────────────────

/// Outcome of a substitution attempt — propagated up from the tool
/// dispatcher so a confirmation/cleanup prompt can be issued exactly
/// once per consumed marker.
#[derive(Clone, Debug)]
pub struct ConsumedSecret {
    pub id: MarkerId,
    pub label: String,
    pub preview: String,
    pub source: SecretSource,
}

/// Per-session secret cache. Keyed by the stable session key the
/// surface layer carries (`slack:<channel>` for the Slack ingress,
/// the chat session id for the web gateway), NOT a per-message UUID
/// that may rotate and orphan live markers.
///
/// Implementations must:
///  * be `Send + Sync` (the cache is shared across all ingress and
///    dispatch tasks);
///  * never log or `Debug`-print cleartext values (the trait surface is
///    designed so impls don't have to handle this themselves: cleartext
///    only crosses the boundary as `Secret<String>`);
///  * touch the entry's expiry on `lookup` so active conversations
///    don't TTL-evict their own markers.
pub trait SecretCache: Send + Sync {
    /// Stash a cleartext secret and return a fresh marker id. The
    /// implementation should also record the source/label/preview so
    /// the metadata can be surfaced to the user without re-deriving it
    /// from the cleartext.
    fn capture(
        &self,
        session_key: &str,
        cleartext: Secret<String>,
        label: &str,
        source: SecretSource,
    ) -> MarkerId;

    /// Stash a cleartext secret under a caller-chosen [`MarkerId::named`]
    /// id (from `/secret <name> <value>`). Unlike [`Self::capture`], the
    /// id is fixed by the caller, the record is scoped so every session
    /// in the same conversation channel can resolve it, and re-using a
    /// name overwrites the previous value.
    fn capture_named(
        &self,
        session_key: &str,
        id: &MarkerId,
        cleartext: Secret<String>,
        source: SecretSource,
    );

    /// Read the cleartext for a marker id WITHOUT removing it. Used by
    /// the substitution layer; the cache is allowed to touch the
    /// entry's TTL on read.
    ///
    /// Returns the cleartext alongside the captured metadata, so audit
    /// logs can be emitted with full context (label, source) even when
    /// the substitution is rejected later.
    fn lookup(&self, session_key: &str, id: &MarkerId) -> Option<(Secret<String>, ConsumedSecret)>;

    /// By-*value* redaction: replace every exact occurrence of a captured
    /// cleartext reachable from `session_key` with its marker form
    /// (`[[secret:<name>]]` for named secrets, the full auto marker
    /// otherwise).
    ///
    /// This is the safety net for credentials that re-enter context as raw
    /// text — e.g. a `/secret …` message that persists in Slack thread
    /// history and is later re-hydrated, or surfaced by a Slack-history
    /// tool — which pattern-based redaction cannot catch because an
    /// arbitrary password matches no known token shape. Applied at every
    /// boundary where text enters the LLM's view so the model can never
    /// see, echo, or transform a value it captured.
    ///
    /// Implementations MUST never return cleartext and SHOULD skip values
    /// shorter than [`MIN_KNOWN_REDACT_LEN`]. The default is a no-op so
    /// test doubles need not implement it; the production cache overrides.
    fn redact_known(&self, _session_key: &str, text: &str) -> String {
        text.to_string()
    }

    /// Names of the live named secrets ([`MarkerId::named`]) reachable
    /// from `session_key`, sorted. Carries no cleartext — used to make
    /// the agent aware of what `/secret` values it can reference. Auto
    /// markers are excluded.
    fn list_named(&self, session_key: &str) -> Vec<String>;

    /// Forget every marker for a given session. Called on session end /
    /// `/new` / explicit clear. Idempotent.
    fn purge_session(&self, session_key: &str);

    /// Forget a single marker (e.g. after the user explicitly says
    /// "drop that token"). Idempotent.
    fn forget(&self, session_key: &str, id: &MarkerId);
}

static GLOBAL_CACHE: OnceLock<RwLock<Option<Arc<dyn SecretCache>>>> = OnceLock::new();

fn slot() -> &'static RwLock<Option<Arc<dyn SecretCache>>> {
    GLOBAL_CACHE.get_or_init(|| RwLock::new(None))
}

/// Install the process-wide cache. Called once at hub init. Subsequent
/// calls overwrite (used in tests). Cheap to clone — the cache is
/// `Arc`'d.
pub fn set_global_cache(cache: Arc<dyn SecretCache>) {
    *slot().write().expect("secret cache lock poisoned") = Some(cache);
}

/// Fetch the process-wide cache. Returns `None` when no cache has been
/// installed (single-binary CLI workflows, unit tests that don't need
/// tokenization). Callers MUST handle `None` — typically by skipping
/// tokenization (the existing untokenized behaviour) and logging a
/// `sentinel.audit` warning so the gap is visible.
pub fn global_cache() -> Option<Arc<dyn SecretCache>> {
    slot()
        .read()
        .expect("secret cache lock poisoned")
        .as_ref()
        .map(Arc::clone)
}

/// Drop the process-wide cache. Tests use this to isolate state
/// between cases; production code should not need it.
pub fn clear_global_cache() {
    *slot().write().expect("secret cache lock poisoned") = None;
}

/// Scrub any captured cleartext reachable from `session_key` out of
/// `text`, replacing each occurrence with its marker. No-op when no cache
/// is installed. Use at inbound LLM boundaries that bypass
/// [`tokenize_secrets`](crate::redact::tokenize_secrets) — thread-history
/// hydration, conversation seeds — so a credential that re-entered as raw
/// text never reaches the model.
pub fn redact_known_secrets(session_key: &str, text: &str) -> String {
    match global_cache() {
        Some(cache) => cache.redact_known(session_key, text),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_id_is_32_lowercase_hex() {
        let id = MarkerId::fresh();
        assert_eq!(id.as_str().len(), 32);
        assert!(id
            .as_str()
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn derived_ids_are_stable_within_a_session_and_unrelated_across_sessions() {
        let id = MarkerId::derived("slack:T1:C1", "sk-ant-api03-value");
        assert_eq!(id, MarkerId::derived("slack:T1:C1", "sk-ant-api03-value"));
        assert_ne!(id, MarkerId::derived("slack:T1:C2", "sk-ant-api03-value"));
        assert_ne!(id, MarkerId::derived("slack:T1:C1", "sk-ant-api03-other"));
        assert!(!id.is_named());
        assert!(is_auto_form(id.as_str()));
    }

    #[test]
    fn marker_round_trips_via_render_and_parse() {
        let m = Marker {
            id: MarkerId::fresh(),
            label: "ANTHROPIC_KEY".to_string(),
            preview: "sk-a...wXyZ".to_string(),
        };
        let rendered = m.render();
        let parsed = parse_markers(&rendered);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0], m);
    }

    #[test]
    fn parse_finds_multiple_markers_in_one_string() {
        let m1 = Marker {
            id: MarkerId::fresh(),
            label: "GITHUB_PAT".to_string(),
            preview: "ghp_...AbCd".to_string(),
        };
        let m2 = Marker {
            id: MarkerId::fresh(),
            label: "SLACK_TOKEN".to_string(),
            preview: "xoxb...EfGh".to_string(),
        };
        let text = format!("first {m1} and second {m2} done");
        let parsed = parse_markers(&text);
        assert_eq!(parsed, vec![m1, m2]);
    }

    #[test]
    fn parse_ignores_unrelated_bracket_text() {
        let text = "see [[note]] and [[reference:abc]] and [[secret:Bad-Name]] (bad)";
        assert!(parse_markers(text).is_empty());
    }

    #[test]
    fn parse_recognises_named_marker() {
        let parsed = parse_markers("use [[secret:datadog_api_key]] now");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, MarkerId::named("datadog_api_key").unwrap());
        assert_eq!(parsed[0].label, NAMED_LABEL);
        assert_eq!(parsed[0].preview, "***");
        assert!(parsed[0].id.is_named());
    }

    /// A model quoting a captured marker back into a later tool call routinely
    /// drops `type=`/`preview=`. Before the decorations were made optional that
    /// bare id was a coin flip: 32 hex beginning `a`–`f` also satisfies the
    /// *named* pattern, so it parsed (and resolved, against the auto record of
    /// the same id) — while one beginning `0`–`9` matched nothing and failed
    /// the call. Both must now parse as the same auto marker.
    #[test]
    fn a_bare_auto_id_parses_whatever_hex_digit_it_starts_with() {
        for id in [
            "a17be95fb446484ca38650d488a8873b",
            "217be95fb446484ca38650d488a8873b",
        ] {
            let parsed = parse_markers(&format!("send [[secret:{id}]] now"));
            assert_eq!(parsed.len(), 1, "should parse: {id}");
            assert_eq!(parsed[0].id.as_str(), id);
            // The auto keyspace, not a name that happens to look like one.
            assert!(!parsed[0].id.is_named(), "{id} is an auto id");
            assert_eq!(parsed[0].label, UNDECORATED_LABEL);
        }
    }

    /// The decorated form is unchanged and still carries its real metadata —
    /// making them optional must not make them ignored.
    #[test]
    fn a_decorated_auto_marker_still_carries_its_type_and_preview() {
        let m = Marker {
            id: MarkerId::fresh(),
            label: "JWT".to_string(),
            preview: "eyJh...AbCd".to_string(),
        };
        let parsed = parse_markers(&format!("token {m} here"));
        assert_eq!(parsed, vec![m]);
    }

    /// A bare marker has no preview to show, and rendering it as the empty
    /// string would delete the fact that a credential stood there.
    #[test]
    fn a_bare_marker_renders_as_a_placeholder_not_as_nothing() {
        let id = "217be95fb446484ca38650d488a8873b";
        assert_eq!(
            render_markers_as_preview(&format!("auth: [[secret:{id}]]")),
            "auth: ***"
        );
    }

    #[test]
    fn parse_orders_mixed_auto_and_named_by_position() {
        let auto = Marker {
            id: MarkerId::fresh(),
            label: "GITHUB_PAT".to_string(),
            preview: "ghp_...AbCd".to_string(),
        };
        let text = format!("first [[secret:my_key]] then {auto} end");
        let parsed = parse_markers(&text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, MarkerId::named("my_key").unwrap());
        assert_eq!(parsed[1], auto);
    }

    #[test]
    fn named_regex_does_not_double_count_auto_marker() {
        let auto = Marker {
            id: MarkerId::fresh(),
            label: "SLACK_TOKEN".to_string(),
            preview: "xoxb...EfGh".to_string(),
        };
        let parsed = parse_markers(&auto.render());
        assert_eq!(parsed, vec![auto]);
    }

    #[test]
    fn marker_id_named_accepts_valid_handles() {
        let max_len = "a".repeat(64);
        for ok in ["a", "datadog_api_key", "key1", max_len.as_str()] {
            assert!(MarkerId::named(ok).is_ok(), "{ok} should be valid");
            assert!(MarkerId::named(ok).unwrap().is_named());
        }
    }

    #[test]
    fn marker_id_named_rejects_invalid_handles() {
        let too_long = "a".repeat(65);
        for bad in [
            "",
            "1key",
            "Key",
            "with space",
            "dash-name",
            too_long.as_str(),
        ] {
            assert_eq!(MarkerId::named(bad), Err(NamedSecretError::InvalidName));
        }
    }

    #[test]
    fn marker_id_named_rejects_reserved_auto_form() {
        let hex32 = "a".repeat(32);
        assert_eq!(MarkerId::named(&hex32), Err(NamedSecretError::ReservedName));
        // A fresh auto id is, by construction, the reserved shape.
        assert!(!MarkerId::fresh().is_named());
    }

    #[test]
    fn slash_command_source_does_not_support_delete_prompt() {
        let s = SecretSource::SlashCommand { user: "U1".into() };
        assert!(!s.supports_delete_prompt());
    }

    #[test]
    fn render_markers_as_preview_replaces_in_place() {
        let m = Marker {
            id: MarkerId::fresh(),
            label: "GITHUB_PAT".to_string(),
            preview: "ghp_...AbCd".to_string(),
        };
        let text = format!("token is {m}, please use it");
        let out = render_markers_as_preview(&text);
        assert_eq!(out, "token is ghp_...AbCd, please use it");
    }

    #[test]
    fn render_markers_collapses_named_marker_to_name() {
        let out = render_markers_as_preview("stored as [[secret:datadog_api_key]] ok");
        assert_eq!(out, "stored as datadog_api_key ok");
    }

    #[test]
    fn render_markers_passes_through_clean_text() {
        let text = "ordinary text without markers";
        assert_eq!(render_markers_as_preview(text), text);
    }

    #[test]
    fn preview_of_short_value_is_stars() {
        assert_eq!(preview_of("short"), "***");
        // Boundary: exactly 15 chars -> ***
        assert_eq!(preview_of("123456789012345"), "***");
    }

    #[test]
    fn preview_of_long_value_is_first4_dots_last4() {
        assert_eq!(preview_of("abcdefghij1234567890wxyz"), "abcd...wxyz");
    }

    #[test]
    fn slack_source_supports_delete_prompt() {
        let s = SecretSource::Slack {
            workspace_id: "T1".into(),
            channel: "C1".into(),
            message_ts: "1.0".into(),
            user: "U1".into(),
        };
        assert!(s.supports_delete_prompt());
    }

    #[test]
    fn tool_result_source_does_not_support_delete_prompt() {
        let s = SecretSource::ToolResult {
            tool: "web.fetch".into(),
        };
        assert!(!s.supports_delete_prompt());
    }

    #[test]
    fn global_cache_round_trip() {
        struct Stub;
        impl SecretCache for Stub {
            fn capture(&self, _: &str, _: Secret<String>, _: &str, _: SecretSource) -> MarkerId {
                MarkerId::fresh()
            }
            fn capture_named(&self, _: &str, _: &MarkerId, _: Secret<String>, _: SecretSource) {}
            fn lookup(&self, _: &str, _: &MarkerId) -> Option<(Secret<String>, ConsumedSecret)> {
                None
            }
            fn list_named(&self, _: &str) -> Vec<String> {
                Vec::new()
            }
            fn purge_session(&self, _: &str) {}
            fn forget(&self, _: &str, _: &MarkerId) {}
        }

        clear_global_cache();
        assert!(global_cache().is_none());
        set_global_cache(Arc::new(Stub));
        assert!(global_cache().is_some());
        clear_global_cache();
        assert!(global_cache().is_none());
    }
}
