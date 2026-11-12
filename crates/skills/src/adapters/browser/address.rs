//! Where a browser is, read the way Chrome reads it: the WHATWG URL parser
//! (`url::Url`, with IDNA), so `FILE:///x`, `file:/x`, `https:bank`,
//! `http:/x`, `http://a\b@c`, `%2E` and a trailing dot all mean here what
//! they mean to the browser.

use std::path::PathBuf;

use url::Url;

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

/// Whether Desk may open an address of `kind` in the person's Chrome: the
/// web and a blank page, never a file, an inline page, a script or
/// Chrome's own pages.
pub fn opens_in_chrome(kind: &Kind) -> bool {
    matches!(kind, Kind::Web | Kind::Blank)
}

/// The host of `url`, as Chrome reads it: lower case, IDNA, no trailing dot.
pub fn host_of(url: &str) -> Option<String> {
    let u = parse(url)?;
    let host = u.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
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
    fn only_the_web_and_a_blank_page_open_in_chrome() {
        for (url, _was_desks, chrome) in [
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
            assert_eq!(opens_in_chrome(&kind(url)), chrome, "{url}");
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
