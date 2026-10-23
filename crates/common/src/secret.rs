//! `Secret<T>` — a wrapper that prevents accidental leaks via `Debug`,
//! `Display`, or `serde::Serialize`.
//!
//! The threat model is operator-grade hygiene rather than cryptographic
//! security: secrets *will* exist in process memory in cleartext (we have
//! to send them as HTTP headers, write them to the vault, etc.). What we
//! want is for an `eprintln!("{:?}", config)` or a `tracing::error!(?
//! state, "...")` to never embed a credential in logs / error messages
//! / Slack notifications by accident.
//!
//! Convention:
//!  * Wrap any field that holds a credential in `Secret<String>`.
//!  * Reach for the cleartext via [`Secret::expose_secret`] only at the
//!    point of use (HTTP `Authorization` header, vault write, etc.).
//!  * Never `.clone().expose_secret()` into a `tracing::*` field. Use
//!    [`Secret::fingerprint`] instead.
//!
//! This module is intentionally minimal — adoption is incremental;
//! existing call sites that pass `String` keep working until they're
//! migrated. The type is the building block that makes future audits
//! greppable: `rg 'secret: String'` finds the holdouts.
//!
//! For full-featured ergonomics see the `secrecy` crate; we keep this
//! local to avoid adding a dep for what is a very thin wrapper.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Where an integration finds its API key. Asked on every request, so a key
/// rotated at its source is used on the next one and each caller gets theirs.
pub type ApiKeySource = std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// `source`'s key now: trimmed, and `None` when it has none or an empty one.
pub fn current_key(source: &ApiKeySource) -> Option<String> {
    source()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
}

/// `text` with every occurrence of `key` removed, for an upstream error body
/// that may echo the key it was sent: whole, or masked with its first and
/// last characters left showing (`sk-ab****wxyz`).
pub fn scrub(text: &str, key: Option<&str>) -> String {
    static MASKED: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"[A-Za-z0-9_-]*\*{2,}[A-Za-z0-9_-]*").expect("a valid pattern")
    });
    const REDACTED: &str = "<redacted>";
    let Some(key) = key.filter(|key| !key.is_empty()) else {
        return text.to_string();
    };
    let whole = text.replace(key, REDACTED);
    MASKED
        .replace_all(&whole, |masked: &regex::Captures<'_>| {
            let (head, tail) = masked[0].split_once('*').map_or(("", ""), |(head, rest)| {
                (head, rest.trim_start_matches('*'))
            });
            let shows_key = !(head.is_empty() && tail.is_empty())
                && key.starts_with(head)
                && key.ends_with(tail);
            match shows_key {
                true => REDACTED.to_string(),
                false => masked[0].to_string(),
            }
        })
        .into_owned()
}

/// A source that always answers `key`.
pub fn fixed_key(key: impl Into<String>) -> ApiKeySource {
    let key = key.into();
    std::sync::Arc::new(move || Some(key.clone()))
}

/// Why a call that needs an API key was not made: whoever it is for has
/// none. Displays as the sentence said to them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoKey(pub String);

impl fmt::Display for NoKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoKey {}

/// An [`ApiKeySource`] together with what to say when it has no key, for a
/// client that must never send a request without one.
#[derive(Clone)]
pub struct KeySlot {
    source: ApiKeySource,
    missing: std::sync::Arc<str>,
}

impl KeySlot {
    pub fn new(source: ApiKeySource, missing: impl Into<std::sync::Arc<str>>) -> Self {
        Self {
            source,
            missing: missing.into(),
        }
    }

    /// A slot that always holds `key`.
    pub fn fixed(key: impl Into<String>) -> Self {
        Self::new(fixed_key(key), "no API key is set")
    }

    /// The key for whoever is in scope now.
    pub fn get(&self) -> Result<String, NoKey> {
        self.peek().ok_or_else(|| NoKey(self.missing.to_string()))
    }

    /// [`Self::get`] for an endpoint that also takes requests without a key.
    pub fn peek(&self) -> Option<String> {
        current_key(&self.source)
    }

    /// The key for whoever is in scope now and the digest that names its
    /// payer, from one read of the source so the two always agree.
    pub fn paid(&self) -> Result<(String, [u8; 32]), NoKey> {
        use sha2::Digest;
        self.get().map(|key| {
            let payer = sha2::Sha256::new()
                .chain_update(*PAYER_SALT)
                .chain_update(key.as_bytes())
                .finalize()
                .into();
            (key, payer)
        })
    }

    /// A digest of the current key, to tell one payer's results from
    /// another's without keeping the key.
    pub fn payer(&self) -> Result<[u8; 32], NoKey> {
        self.paid().map(|(_, payer)| payer)
    }
}

/// Mixed into every payer digest and drawn afresh by each process, so a
/// digest names a key only inside the process that made it.
static PAYER_SALT: std::sync::LazyLock<[u8; 16]> =
    std::sync::LazyLock::new(|| uuid::Uuid::new_v4().into_bytes());

impl fmt::Debug for KeySlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeySlot(***)")
    }
}

/// A value that should never be embedded in logs / Debug output / error
/// messages.
///
/// `Debug` prints `Secret(***)`, `Display` prints `***`, and `Serialize`
/// emits `null`. Cleartext is reachable only through
/// [`Self::expose_secret`].
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Wrap a cleartext value.
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Borrow the cleartext value. Use at the point of consumption only.
    pub fn expose_secret(&self) -> &T {
        &self.0
    }

    /// Consume the wrapper and return the cleartext.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(***)")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "***")
    }
}

impl<T: Serialize> Serialize for Secret<T> {
    /// Serializing a `Secret` emits `null` rather than the underlying
    /// value. Persistence callers that need the actual value go through
    /// [`Self::expose_secret`] before encoding.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_unit()
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Secret<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Secret)
    }
}

impl<T> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Secret(value)
    }
}

impl Secret<String> {
    /// Short hex prefix of the SHA-256 of the secret. Useful as an audit
    /// fingerprint in logs without revealing the value (mirrors
    /// `crate::redact::short_fingerprint`). Returns the first 8 hex
    /// chars (4 bytes) of the digest.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.0.as_bytes());
        let digest = hasher.finalize();
        hex::encode(&digest[..4])
    }

    /// Whether the wrapped secret is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_slot_reads_its_source_each_time_and_says_why_it_has_none() {
        let held = std::sync::Arc::new(std::sync::Mutex::new(Some(" first \n".to_string())));
        let slot = KeySlot::new(
            {
                let held = std::sync::Arc::clone(&held);
                std::sync::Arc::new(move || held.lock().unwrap().clone())
            },
            "nobody set one",
        );
        assert_eq!(slot.get().unwrap(), "first");
        let first = slot.payer().unwrap();

        *held.lock().unwrap() = Some("second".into());
        assert_eq!(slot.peek().as_deref(), Some("second"));
        assert_ne!(slot.payer().unwrap(), first, "another key is another payer");

        *held.lock().unwrap() = Some("  ".into());
        assert_eq!(slot.peek(), None);
        assert_eq!(slot.get(), Err(NoKey("nobody set one".into())));
        assert_eq!(slot.payer(), Err(NoKey("nobody set one".into())));
        assert_eq!(slot.get().unwrap_err().to_string(), "nobody set one");
    }

    #[test]
    fn scrubbing_removes_a_key_echoed_whole_or_masked_and_leaves_the_rest() {
        let key = "sk-proj-abcdef123456wxyz";
        assert_eq!(
            scrub(&format!("bad key {key}, twice {key}."), Some(key)),
            "bad key <redacted>, twice <redacted>."
        );
        assert_eq!(
            scrub(
                "Incorrect API key provided: sk-proj-********************wxyz. Find yours.",
                Some(key)
            ),
            "Incorrect API key provided: <redacted>. Find yours."
        );
        assert_eq!(
            scrub("key sk-pr**** refused", Some(key)),
            "key <redacted> refused"
        );
        assert_eq!(
            scrub("key ****wxyz refused", Some(key)),
            "key <redacted> refused"
        );
        for untouched in [
            "use **bold** text",
            "rate limit: 5 req/s",
            "other key sk-zz****9999 is fine",
            "a row of ***** stars",
        ] {
            assert_eq!(scrub(untouched, Some(key)), untouched);
        }
        assert_eq!(scrub("anything", None), "anything");
        assert_eq!(scrub("anything", Some("")), "anything");
    }

    #[test]
    fn a_key_slot_never_prints_its_key() {
        let slot = KeySlot::fixed("sk-very-secret");
        assert_eq!(format!("{slot:?}"), "KeySlot(***)");
        assert!(!format!("{:?}", slot.payer().unwrap()).contains("sk-very-secret"));
    }

    #[test]
    fn a_payer_digest_is_salted_and_comes_from_the_same_read_as_its_key() {
        use sha2::Digest;
        let slot = KeySlot::fixed("sk-very-secret");
        let payer = slot.payer().unwrap();
        assert_eq!(payer, KeySlot::fixed(" sk-very-secret ").payer().unwrap());
        let unsalted: [u8; 32] = sha2::Sha256::digest(b"sk-very-secret").into();
        assert_ne!(
            payer, unsalted,
            "a bare hash of the key would name it anywhere"
        );

        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let turning = KeySlot::new(
            {
                let reads = std::sync::Arc::clone(&reads);
                std::sync::Arc::new(move || {
                    let read = reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Some(format!("key-{read}"))
                })
            },
            "none",
        );
        let (key, payer) = turning.paid().unwrap();
        assert_eq!(key, "key-0");
        assert_eq!(payer, KeySlot::fixed("key-0").payer().unwrap());
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn debug_does_not_leak_value() {
        let s = Secret::new("my-very-secret-token".to_string());
        let dbg = format!("{:?}", s);
        assert_eq!(dbg, "Secret(***)");
        assert!(!dbg.contains("secret-token"));
    }

    #[test]
    fn display_does_not_leak_value() {
        let s = Secret::new("very-secret".to_string());
        let disp = format!("{}", s);
        assert_eq!(disp, "***");
    }

    #[test]
    fn serialize_emits_null() {
        let s = Secret::new("very-secret".to_string());
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "null");
    }

    #[test]
    fn round_trip_via_serde_loses_value() {
        // Important property: round-tripping through serde *cannot* be
        // used to copy a secret. The Deserialize path produces a Secret
        // wrapping whatever was in the JSON (or a default), so the
        // accidental `tracing::info!(state = ?serde_json::to_string(&s))`
        // pattern doesn't leak even if the field contains a Secret.
        let original = Secret::new("very-secret".to_string());
        let json = serde_json::to_string(&original).unwrap();
        // Round-trip yields a Secret<()> not Secret<String>, since we
        // serialised as unit. We only test that the JSON itself is null.
        assert_eq!(json, "null");
        let _ = original.expose_secret();
    }

    #[test]
    fn deserialize_from_string() {
        // Direct deserialize path (e.g. when the value comes from a
        // trusted source like a vault or config file): Secret<String>
        // accepts a JSON string.
        let s: Secret<String> = serde_json::from_str(r#""hello""#).unwrap();
        assert_eq!(s.expose_secret(), "hello");
    }

    #[test]
    fn fingerprint_is_short_and_stable() {
        let a = Secret::new("token-A".to_string());
        let b = Secret::new("token-A".to_string());
        let c = Secret::new("token-B".to_string());
        let fa = a.fingerprint();
        assert_eq!(fa.len(), 8);
        assert_eq!(fa, b.fingerprint());
        assert_ne!(fa, c.fingerprint());
        // The fingerprint never embeds the value.
        assert!(!fa.contains("token"));
    }

    #[test]
    fn expose_secret_returns_cleartext() {
        let s = Secret::new("real-value".to_string());
        assert_eq!(s.expose_secret(), "real-value");
    }

    #[test]
    fn into_inner_returns_owned_cleartext() {
        let s = Secret::new("real-value".to_string());
        let inner = s.into_inner();
        assert_eq!(inner, "real-value");
    }

    #[test]
    fn equality_compares_underlying() {
        let a: Secret<String> = "hello".to_string().into();
        let b: Secret<String> = "hello".to_string().into();
        let c: Secret<String> = "world".to_string().into();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
