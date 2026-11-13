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
    /// Whether one leading segment of the account's own may come before a
    /// prefix: Linear's `/acme/settings/account/security`.
    pub workspace_segment: bool,
    /// What one of its keys looks like, whole.
    pub shape: Regex,
}

static PAGES: LazyLock<RwLock<Vec<KeyPage>>> = LazyLock::new(Default::default);

/// Key pages whose service runs at an address of each person's own (a
/// Metabase): their paths on any `https` origin. Their text is hidden and a
/// picture of one is thrown away wherever they are, before a setup has
/// kept the address and after; one entry per connector, so nothing grows.
static ANYWHERE: LazyLock<RwLock<Vec<KeyPage>>> = LazyLock::new(Default::default);

/// Registers (or replaces) `page` for its connector at its origin. A
/// connector may have key pages at several origins (Datadog's regions, each
/// person's own Metabase): every one is hidden, and none replaces another's.
/// The registry says which pages to hide and never to picture; which page a
/// key is *read* off is the hub's, per setup (`BrowserHooks::key_page`).
pub fn register(page: KeyPage) {
    let mut pages = PAGES.write().unwrap();
    pages.retain(|p| p.connector != page.connector || p.origin != page.origin);
    pages.push(page);
}

/// Registers (or replaces) `page`'s paths as key pages on every `https`
/// origin: its `origin` is not looked at. They are never sent the extension
/// as a picture to refuse (its refusals are exact origins); the hub's own
/// check of what was pictured throws such a picture away.
pub fn register_anywhere(page: KeyPage) {
    let mut pages = ANYWHERE.write().unwrap();
    pages.retain(|p| p.connector != page.connector);
    pages.push(page);
}

/// Whether `url` is, on any `https` origin, under one of `page`'s paths.
fn matches_anywhere(page: &KeyPage, url: &str) -> bool {
    match super::address::origin_of(url) {
        Some(origin) if origin.starts_with("https://") => matches(
            &KeyPage {
                origin,
                ..page.clone()
            },
            url,
        ),
        _ => false,
    }
}

/// Whether `url` is on one of `page`'s key pages.
pub fn matches(page: &KeyPage, url: &str) -> bool {
    let Some(u) = super::address::parse(url) else {
        return false;
    };
    super::address::origin_of(url).as_deref() == Some(page.origin.as_str())
        && page.paths.iter().any(|p| {
            under(u.path(), p)
                || (page.workspace_segment
                    && after_segment(u.path()).is_some_and(|rest| under(rest, p)))
        })
}

/// `path` without its first segment (`/acme/settings` → `/settings`), when
/// that segment is one an account could be named (letters, digits, `-`,
/// `_`).
fn after_segment(path: &str) -> Option<&str> {
    let rest = path.strip_prefix('/')?;
    let cut = rest.find('/')?;
    let segment = &rest[..cut];
    (!segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then(|| &rest[cut..])
}

/// `page`'s prefixes as they stand on the tab at `url`: where one leading
/// segment of the account's own may come first and the tab has one, that
/// segment is written in (`/acme/settings/account/security`), so the
/// extension's read checks the page whole, in the same call, without
/// knowing about workspaces.
pub fn paths_at(page: &KeyPage, url: &str) -> Vec<String> {
    let Some(u) = super::address::parse(url) else {
        return page.paths.clone();
    };
    let path = u.path();
    page.paths
        .iter()
        .map(|p| match under(path, p) || !page.workspace_segment {
            true => p.clone(),
            false => match after_segment(path).filter(|rest| under(rest, p)) {
                Some(rest) => format!("{}{}", &path[..path.len() - rest.len()], p),
                None => p.clone(),
            },
        })
        .collect()
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
        || ANYWHERE
            .read()
            .unwrap()
            .iter()
            .any(|p| matches_anywhere(p, url))
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
    if !is_key_page(url) {
        return text.to_string();
    }
    let pages = PAGES.read().unwrap();
    let anywhere = ANYWHERE.read().unwrap();
    let mut out = text.to_string();
    for p in pages.iter().chain(anywhere.iter()) {
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
        workspace_segment: false,
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
    fn a_workspace_segment_may_come_first_and_is_written_into_the_paths_sent() {
        let page = KeyPage {
            connector: "fixture-ws".into(),
            origin: "https://ws.example".into(),
            paths: vec!["/settings/account/security".into(), "/settings/api".into()],
            workspace_segment: true,
            shape: Regex::new(r"ws_[a-z]{8}").unwrap(),
        };
        assert!(matches(&page, "https://ws.example/settings/api"));
        assert!(matches(
            &page,
            "https://ws.example/acme/settings/account/security"
        ));
        assert!(matches(
            &page,
            "https://ws.example/acme-2_x/settings/api/new"
        ));
        for not in [
            "https://ws.example/a/b/settings/api",
            "https://ws.example/acme/settings/apikeys",
            "https://ws.example/ac%20me/settings/api",
            "https://ws.example/acme",
        ] {
            assert!(!matches(&page, not), "{not}");
        }
        // Without the flag, a segment first is another page.
        let flat = KeyPage {
            workspace_segment: false,
            ..page.clone()
        };
        assert!(!matches(&flat, "https://ws.example/acme/settings/api"));
        assert_eq!(
            paths_at(&page, "https://ws.example/acme/settings/account/security"),
            vec![
                "/acme/settings/account/security".to_string(),
                "/settings/api".to_string()
            ]
        );
        assert_eq!(
            paths_at(&page, "https://ws.example/settings/api"),
            vec![
                "/settings/account/security".to_string(),
                "/settings/api".to_string()
            ]
        );
    }

    #[test]
    fn pages_at_several_origins_stay_registered_side_by_side() {
        let at = |origin: &str| KeyPage {
            connector: "fixture-many".into(),
            origin: origin.into(),
            paths: vec!["/admin/keys".into()],
            workspace_segment: false,
            shape: Regex::new(r"mm_[a-z]{8}").unwrap(),
        };
        register(at("https://one.example"));
        register(at("https://two.example"));
        register(at("https://one.example"));
        assert!(is_key_page("https://one.example/admin/keys"));
        assert!(is_key_page("https://two.example/admin/keys"));
        let n = PAGES
            .read()
            .unwrap()
            .iter()
            .filter(|p| p.connector == "fixture-many")
            .count();
        assert_eq!(n, 2);
    }

    #[test]
    fn a_page_registered_anywhere_is_hidden_on_every_https_origin_and_never_sent_to_refuse() {
        let page = KeyPage {
            connector: "fixture-anywhere".into(),
            origin: String::new(),
            paths: vec!["/admin/api-keys".into()],
            workspace_segment: false,
            shape: Regex::new(r"aw_[a-z]{8}").unwrap(),
        };
        register_anywhere(page.clone());
        register_anywhere(page);
        for at in [
            "https://bi.acme.example/admin/api-keys",
            "https://10.0.0.5:3000/admin/api-keys/new",
        ] {
            assert!(is_key_page(at), "{at}");
            assert_eq!(hide_keys(at, "aw_abcdefgh"), HIDDEN);
        }
        for not in [
            "http://bi.acme.example/admin/api-keys",
            "https://bi.acme.example/admin/api-keysx",
            "https://bi.acme.example/admin",
        ] {
            assert!(!is_key_page(not), "{not}");
        }
        assert_eq!(
            ANYWHERE
                .read()
                .unwrap()
                .iter()
                .filter(|p| p.connector == "fixture-anywhere")
                .count(),
            1
        );
        assert!(!refusals().to_string().contains("/admin/api-keys"));
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
