//! Key pages: the pages where a service shows a new API key, for
//! Connections' agent-led setup to read it straight into the vault without
//! the model ever seeing it (`browser.capture_key`). Connections registers a
//! page for each connector it sets up ([`register`]); nothing else is a key
//! page.
//!
//! On a key page, every read hides key-shaped text, and no picture is
//! taken; the read of the key itself checks the page and the element in the
//! extension's own world, in the same call ([`super::extension`]).

use std::sync::{LazyLock, RwLock};

use regex::Regex;

/// The tool that reads a new key.
pub const CAPTURE_KEY: &str = "browser.capture_key";

/// One connector's key pages.
#[derive(Debug, Clone)]
pub struct KeyPage {
    pub connector: String,
    /// The exact origin: `https://linear.app`.
    pub origin: String,
    /// The path prefixes its key pages are under.
    pub paths: Vec<String>,
    /// What one of its keys looks like, whole.
    pub shape: Regex,
}

static PAGES: LazyLock<RwLock<Vec<KeyPage>>> = LazyLock::new(Default::default);

/// Registers (or replaces) `page` for its connector.
pub fn register(page: KeyPage) {
    let mut pages = PAGES.write().unwrap();
    pages.retain(|p| p.connector != page.connector);
    pages.push(page);
}

/// `connector`'s key pages, if it has any.
pub fn page_for(connector: &str) -> Option<KeyPage> {
    PAGES
        .read()
        .unwrap()
        .iter()
        .find(|p| p.connector == connector)
        .cloned()
}

/// Whether `url` is on one of `page`'s key pages.
pub fn matches(page: &KeyPage, url: &str) -> bool {
    let Some(u) = super::address::parse(url) else {
        return false;
    };
    super::address::origin_of(url).as_deref() == Some(page.origin.as_str())
        && page.paths.iter().any(|p| under(u.path(), p))
}

/// Whether `path` is `prefix` or under it, a whole segment at a time:
/// `/settings/api` holds `/settings/api/new`, never `/settings/apikeys`.
pub fn under(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    path == prefix || path.starts_with(&format!("{prefix}/")) || prefix.is_empty()
}

/// Whether `selector` could tell, by matching or not, what a field holds:
/// an attribute test on `value`, or any CSS escape that could spell one.
/// On a key page such a selector is refused: whether it matched would read
/// the key a character at a time.
pub fn probes_value(selector: &str) -> bool {
    let flat: String = selector
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    flat.contains("[value") || flat.contains("|value") || flat.contains('\\')
}

/// What a selector that tests a field's value is told on a key page.
pub const PROBES_VALUE: &str = "On a key page Desk doesn't take a selector that tests a field's value (`[value…]`, or an escape): use an id, a name or a label instead.";

/// Whether `url` is any connector's key page.
pub fn is_key_page(url: &str) -> bool {
    PAGES.read().unwrap().iter().any(|p| matches(p, url))
}

/// Whether `value` is, whole, a key of `page`'s connector.
pub fn shaped(page: &KeyPage, value: &str) -> bool {
    page.shape
        .find(value)
        .is_some_and(|m| m.start() == 0 && m.end() == value.len())
}

/// Every key page, as the extension takes them for a picture it mustn't
/// take.
pub fn refusals() -> serde_json::Value {
    serde_json::Value::Array(
        PAGES
            .read()
            .unwrap()
            .iter()
            .map(|p| serde_json::json!({ "origin": p.origin, "paths": p.paths }))
            .collect(),
    )
}

/// A long run of key-like characters: what a key page's text must not show.
static LONG_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9_\-]{24,}").expect("token regex"));

/// What a key-shaped run is replaced with.
pub const HIDDEN: &str = "[key hidden: use browser.capture_key]";

/// `text` read from the page at `url`, with key-shaped text hidden when it
/// is a key page; any other page's text as it is.
pub fn hide_keys(url: &str, text: &str) -> String {
    let pages = PAGES.read().unwrap();
    if !pages.iter().any(|p| matches(p, url)) {
        return text.to_string();
    }
    let mut out = text.to_string();
    for p in pages.iter() {
        out = p.shape.replace_all(&out, HIDDEN).into_owned();
    }
    let out = workforce_common::redact::redact_secrets(&out);
    LONG_TOKEN.replace_all(&out, HIDDEN).into_owned()
}

#[cfg(test)]
pub(super) fn fixture_page() -> KeyPage {
    KeyPage {
        connector: "fixture".into(),
        origin: "https://keys.example".into(),
        paths: vec!["/settings/api".into()],
        shape: Regex::new(r"fx_[A-Za-z0-9]{24}").unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_page_is_its_exact_origin_under_its_paths() {
        let page = fixture_page();
        assert!(matches(&page, "https://keys.example/settings/api/new"));
        for not in [
            "https://keys.example/settings",
            "https://evil.keys.example/settings/api",
            "http://keys.example/settings/api",
            "https://keys.example.evil/settings/api",
            "about:blank",
        ] {
            assert!(!matches(&page, not), "{not}");
        }
        assert!(!matches(&page, "https://keys.example/settings/apikeys"));
        assert!(matches(&page, "https://keys.example/settings/api"));
        assert!(shaped(&page, "fx_abcdefghijklmnopqrstuvwx"));
        assert!(!shaped(&page, "x fx_abcdefghijklmnopqrstuvwx"));
        assert!(!shaped(&page, "fx_short"));
    }

    #[test]
    fn a_selector_that_tests_a_value_is_known() {
        for probe in [
            "input[value^=\"fx_a\"]",
            "#key[ VALUE*=q]",
            "input[v\\61lue^=fx]",
            "form:has(input[value$=z])",
            "input[*|value^=fx]",
        ] {
            assert!(probes_value(probe), "{probe}");
        }
        for fine in [
            "#new-key",
            "input[name=token]",
            "[data-testid=key]",
            "button.copy",
        ] {
            assert!(!probes_value(fine), "{fine}");
        }
    }

    #[test]
    fn a_key_pages_text_hides_keys_and_other_pages_keep_theirs() {
        register(fixture_page());
        let text = "Your new key: fx_abcdefghijklmnopqrstuvwx. Also abcdef0123456789abcdef0123456789. Copy it now.";
        let hidden = hide_keys("https://keys.example/settings/api", text);
        assert!(!hidden.contains("fx_abcdef"), "{hidden}");
        assert!(!hidden.contains("abcdef0123456789abcdef"), "{hidden}");
        assert!(hidden.contains("Copy it now"));
        assert!(hidden.contains(HIDDEN));
        assert_eq!(hide_keys("https://shop.example/", text), text);
        assert!(is_key_page("https://keys.example/settings/api"));
        assert_eq!(refusals()[0]["origin"], "https://keys.example");
    }
}
