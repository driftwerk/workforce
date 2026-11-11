//! Where a browser is, read the way Chrome reads it: the WHATWG URL parser
//! (`url::Url`, with IDNA), so `FILE:///x`, `file:/x`, `https:bank`,
//! `http:/x`, `http://a\b@c`, `%2E` and a trailing dot all mean here what
//! they mean to the browser.

use std::path::PathBuf;

use url::Url;

/// Which browser an address is opened in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum In {
    /// Desk's own: the web, a blank page, an inline page, or a file.
    Desk,
    /// The person's Chrome: the web and a blank page only.
    #[allow(dead_code)] // opened in by My Chrome
    Chrome,
}

/// What an address is, for what may be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// `http` or `https`.
    Web,
    /// `about:blank`.
    Blank,
    /// `data:`, an inline page.
    Inline,
    /// A file on this computer, by its path.
    File(PathBuf),
    /// Anything else: a script, the browser's own pages, a file on another
    /// computer. Never opened.
    Other(String),
}

pub fn parse(url: &str) -> Option<Url> {
    Url::parse(url.trim()).ok()
}

/// What `url` is.
pub fn kind(url: &str) -> Kind {
    let Some(u) = parse(url) else {
        return Kind::Other("an address that can't be read".into());
    };
    match u.scheme() {
        "http" | "https" => Kind::Web,
        "about" if u.path() == "blank" => Kind::Blank,
        "data" => Kind::Inline,
        "file" => match u
            .host_str()
            .filter(|h| !h.is_empty() && !h.eq_ignore_ascii_case("localhost"))
        {
            Some(_) => Kind::Other("a file on another computer".into()),
            None => match u.to_file_path() {
                Ok(p) => Kind::File(p),
                Err(()) => Kind::Other("a file address that can't be read".into()),
            },
        },
        other => Kind::Other(format!("`{other}:`")),
    }
}

/// Whether `kind` may be opened in `place`.
pub fn openable(kind: &Kind, place: In) -> bool {
    matches!(
        (kind, place),
        (Kind::Web | Kind::Blank, _) | (Kind::Inline | Kind::File(_), In::Desk)
    )
}

/// The host of `url`, as Chrome reads it: lower case, IDNA, no trailing dot.
pub fn host_of(url: &str) -> Option<String> {
    let u = parse(url)?;
    let host = u.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// Whether `url` is a page where a service shows its keys: the ones an
/// agent setting a connection up makes a key on. Desk takes no picture of
/// one, and hides what looks like a key in what it reads of one, so a key
/// made there reaches no transcript or record; `connections.capture` reads
/// it for the vault instead.
pub fn shows_keys(url: &str) -> bool {
    let Some(u) = parse(url) else {
        return false;
    };
    let host = host_of(url).unwrap_or_default();
    let path = u.path().trim_end_matches('/').to_ascii_lowercase();
    let datadog = ["datadoghq.com", "datadoghq.eu", "ddog-gov.com"]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")));
    (host == "id.atlassian.com" && path.contains("/api-tokens"))
        || (host == "linear.app"
            && (path.ends_with("/settings/account/security") || path.ends_with("/settings/api")))
        || (host == "github.com"
            && (path.starts_with("/settings/tokens")
                || path.starts_with("/settings/personal-access-tokens")))
        || (datadog && (path.contains("/api-keys") || path.contains("/application-keys")))
        || path.ends_with("/admin/settings/authentication/api-keys")
}

/// What looks like a key in text read off a page that shows them.
static KEY_LIKE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    // A run of key characters, or Metabase's base64 (slashes and all); a
    // path's slashes break an address into short runs, which stay.
    regex::Regex::new(r"mb_[A-Za-z0-9+/=_\-]{16,}|[A-Za-z0-9_\-+=]{24,}")
        .expect("key-like pattern compiles")
});

/// `value` with every run that looks like a key hidden, in every string it
/// holds.
pub fn without_keys(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(s) => Value::String(KEY_LIKE.replace_all(&s, "[a key, hidden]").into_owned()),
        Value::Array(a) => Value::Array(a.into_iter().map(without_keys).collect()),
        Value::Object(o) => {
            Value::Object(o.into_iter().map(|(k, v)| (k, without_keys(v))).collect())
        }
        other => other,
    }
}

/// The origin of `url` (`https://shop.example`, `http://127.0.0.1:8080`),
/// what an allowance holds for; `None` for a page with none of its own
/// (`about:blank`, `data:`, a file).
pub fn origin_of(url: &str) -> Option<String> {
    let u = parse(url)?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = host_of(url)?;
    let host = match u.host() {
        Some(url::Host::Ipv6(_)) => format!("[{}]", host.trim_matches(['[', ']'])),
        _ => host,
    };
    Some(match u.port() {
        Some(port) => format!("{}://{host}:{port}", u.scheme()),
        None => format!("{}://{host}", u.scheme()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pages_services_show_their_keys_on_are_known() {
        for url in [
            "https://id.atlassian.com/manage-profile/security/api-tokens",
            "https://linear.app/settings/account/security",
            "https://linear.app/acme/settings/api",
            "https://github.com/settings/personal-access-tokens/new",
            "https://github.com/settings/tokens",
            "https://app.datadoghq.eu/organization-settings/api-keys",
            "https://us5.datadoghq.com/personal-settings/application-keys",
            "https://metabase.acme.example/admin/settings/authentication/api-keys",
        ] {
            assert!(shows_keys(url), "{url}");
        }
        for url in [
            "https://id.atlassian.com/login",
            "https://linear.app/acme/issue/ACM-1",
            "https://github.com/acme/web/settings",
            "https://datadoghq.com.evil.example/api-keys",
            "about:blank",
        ] {
            assert!(!shows_keys(url), "{url}");
        }
        let shown = without_keys(serde_json::json!({
            "body_text": "Your new token lin_api_0123456789abcdefghij0123 and a word",
            "items": ["0123456789abcdef0123456789abcdef", "mb_AbC/dEf+GhIjKlMnOp=="],
            "url": "https://id.atlassian.com/manage-profile/security/api-tokens"
        }));
        let text = shown.to_string();
        assert!(
            !text.contains("lin_api_0123") && !text.contains("0123456789abcdef0123"),
            "{text}"
        );
        assert!(text.contains("and a word") && !text.contains("mb_AbC"));
        assert!(text.contains("https://id.atlassian.com/manage-profile/security/api-tokens"));
    }

    #[test]
    fn files_are_read_as_chrome_reads_them() {
        for url in [
            "file:///etc/hosts",
            "FILE:///etc/hosts",
            "file:/etc/hosts",
            "file://localhost/etc/hosts",
            "file://LOCALHOST/etc/hosts",
            "File:///etc/%68osts",
        ] {
            assert_eq!(kind(url), Kind::File(PathBuf::from("/etc/hosts")), "{url}");
        }
        assert!(matches!(kind("file://host/etc/hosts"), Kind::Other(_)));
        assert!(matches!(kind("file://192.168.1.2/share/x"), Kind::Other(_)));
    }

    #[test]
    fn only_the_web_blank_inline_and_files_open_and_only_the_web_in_chrome() {
        for (url, desk, chrome) in [
            ("https://a.test/", true, true),
            ("HTTP://a.test", true, true),
            ("about:blank", true, true),
            ("about:srcdoc", false, false),
            ("about:settings", false, false),
            ("data:text/html,<p>x</p>", true, false),
            ("file:///tmp/a.html", true, false),
            ("javascript:alert(1)", false, false),
            ("JavaScript:alert(1)", false, false),
            ("chrome://settings", false, false),
            ("devtools://x", false, false),
            ("view-source:https://a.test", false, false),
            ("blob:https://a.test/x", false, false),
            ("not a url", false, false),
        ] {
            let k = kind(url);
            assert_eq!(openable(&k, In::Desk), desk, "{url} in Desk's");
            assert_eq!(openable(&k, In::Chrome), chrome, "{url} in Chrome");
        }
    }

    #[test]
    fn hosts_are_read_as_chrome_reads_them() {
        for (url, host) in [
            ("https://Shop.Example./cart", "shop.example"),
            ("https:bank.test", "bank.test"),
            ("http:/x.test/y", "x.test"),
            ("http://evil.test\\@bank.test/", "evil.test"),
            ("https://bank%2Etest/", "bank.test"),
            ("https://xn--bcher-kva.example/", "xn--bcher-kva.example"),
            ("https://bücher.example/", "xn--bcher-kva.example"),
            ("https://user:pw@a.test:8443/", "a.test"),
        ] {
            assert_eq!(host_of(url).as_deref(), Some(host), "{url}");
        }
        assert_eq!(host_of("about:blank"), None);
    }

    #[test]
    fn an_origin_is_scheme_host_and_port_and_only_the_webs() {
        assert_eq!(
            origin_of("https://a.test/x?y").as_deref(),
            Some("https://a.test")
        );
        assert_eq!(
            origin_of("http://127.0.0.1:8080/").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(
            origin_of("https://a.test:443/").as_deref(),
            Some("https://a.test")
        );
        assert_ne!(origin_of("http://a.test/"), origin_of("https://a.test/"));
        for none in ["about:blank", "data:text/html,x", "file:///tmp/a"] {
            assert_eq!(origin_of(none), None, "{none}");
        }
    }
}
