//! Leaving mailing lists the way their own headers offer (RFC 2369
//! `List-Unsubscribe`, RFC 8058 one-click), never by a link in a body.

use std::collections::BTreeMap;
use std::time::Duration;

use mailparse::MailHeaderMap;

/// The subject of every unsubscribe email Desk sends: its own, never one a
/// list's header asks for.
pub const SUBJECT: &str = "unsubscribe";

/// The headers an unsubscribe reads.
pub const HEADERS: &[&str] = &[
    "LIST-UNSUBSCRIBE",
    "LIST-UNSUBSCRIBE-POST",
    "LIST-ID",
    "FROM",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// An RFC 8058 POST to an https URL, which unsubscribes with no page to
    /// read.
    OneClick { url: String },
    /// A message to the list's unsubscribe address, in Desk's own words.
    Mailto { to: String },
    /// Nothing Desk can do for you: the list offers only a web page, or
    /// nothing.
    None { page: Option<String> },
}

impl Method {
    /// Where leaving writes to: the address an email goes to, or the host
    /// a one-click request is sent to.
    pub fn via(&self) -> Option<String> {
        match self {
            Method::OneClick { url } => url::Url::parse(url)
                .ok()?
                .host_str()
                .map(str::to_ascii_lowercase),
            Method::Mailto { to } => Some(to.to_ascii_lowercase()),
            Method::None { .. } => None,
        }
    }

    pub fn how(&self) -> &'static str {
        match self {
            Method::OneClick { .. } => "one_click",
            Method::Mailto { .. } => "email",
            Method::None { page: Some(_) } => "web_page_only",
            Method::None { page: None } => "none",
        }
    }

    /// What tells this way out from another: how, and to which host or
    /// mail domain, so a token a list puts in each message's address or
    /// link never makes two ways of one.
    fn way(&self) -> (&'static str, Option<String>) {
        let to = match self {
            Method::None { page: Some(page) } => url::Url::parse(page)
                .ok()
                .and_then(|u| u.host_str().map(str::to_ascii_lowercase)),
            Method::Mailto { to } => Some(domain_of(to).to_ascii_lowercase()),
            other => other.via(),
        };
        (self.how(), to)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List {
    pub id: String,
    pub name: String,
    pub messages: usize,
    pub method: Method,
    /// Messages of the list offering another way out than `method`.
    pub others: usize,
}

impl List {
    /// The list as `email.unsubscribe_lists` shows it: how it is left,
    /// whether that goes somewhere other than the list's own domain, and
    /// whether its messages disagree on it.
    pub fn view(&self) -> serde_json::Value {
        let via = self.method.via();
        let page = match &self.method {
            Method::None { page } => page.as_deref(),
            _ => None,
        };
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "messages": self.messages,
            "how": self.method.how(),
            "via": via,
            "elsewhere": via.as_deref().is_some_and(|v| !same_site(&self.id, v)),
            "mixed": self.others > 0,
            "offering_another_way": self.others,
            "page": page,
        })
    }
}

/// The domain of an address, or a host as it is.
fn domain_of(text: &str) -> &str {
    text.rsplit_once('@').map_or(text, |(_, domain)| domain)
}

/// The registrable part of `host`, near enough with no public suffix list:
/// its last two labels, or three under a country's own second level
/// (`shop.co.uk`).
fn site(host: &str) -> String {
    const SECOND_LEVELS: &[&str] = &["ac", "co", "com", "edu", "gov", "ne", "net", "or", "org"];
    let labels: Vec<String> = host
        .trim_end_matches('.')
        .split('.')
        .filter(|l| !l.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let keep = match labels.as_slice() {
        [.., second, tld] if tld.len() == 2 && SECOND_LEVELS.contains(&second.as_str()) => 3,
        _ => 2,
    };
    labels[labels.len().saturating_sub(keep)..].join(".")
}

/// Whether `a` and `b` (addresses, hosts or list ids) are of one domain.
pub fn same_site(a: &str, b: &str) -> bool {
    site(domain_of(a)) == site(domain_of(b))
}

fn angle_parts(header: &str) -> Vec<String> {
    header
        .split('<')
        .skip(1)
        .filter_map(|part| {
            part.split_once('>')
                .map(|(inside, _)| inside.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn percent_decoded(text: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", text.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// The address a `mailto:` URL writes to; what else it asks for (a
/// subject, a body) is the list's words and never used.
pub fn mailto(target: &str) -> Option<String> {
    let rest = target
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("mailto:"))
        .map(|_| &target[7..])?;
    let to = percent_decoded(rest.split_once('?').map_or(rest, |(to, _)| to));
    (to.contains('@') && !to.contains(['\r', '\n', ','])).then_some(to)
}

fn method(unsubscribe: &str, post: Option<&str>) -> Method {
    let targets = angle_parts(unsubscribe);
    let https = targets.iter().find(|t| {
        t.get(..8)
            .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
    });
    let one_click = post.is_some_and(|p| {
        p.split(';')
            .any(|v| v.trim().eq_ignore_ascii_case("List-Unsubscribe=One-Click"))
    });
    let mail = targets.iter().find_map(|t| mailto(t));
    match (https, one_click, mail) {
        (Some(url), true, _) => Method::OneClick { url: url.clone() },
        (_, _, Some(to)) => Method::Mailto { to },
        _ => Method::None {
            page: targets
                .iter()
                .find(|t| t.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("http")))
                .cloned(),
        },
    }
}

/// Which list a message belongs to: its `List-Id`, else its sender.
fn list_of(headers: &[mailparse::MailHeader<'_>]) -> Option<(String, String)> {
    let list_id = headers.get_first_value("List-Id");
    let from = headers.get_first_value("From").unwrap_or_default();
    let address = |text: &str| {
        angle_parts(text)
            .into_iter()
            .next()
            .unwrap_or_else(|| text.trim().to_string())
            .to_ascii_lowercase()
    };
    match list_id {
        Some(id) if !address(&id).is_empty() => {
            let name = id.split('<').next().unwrap_or("").trim().trim_matches('"');
            let id = address(&id);
            let name = match name.is_empty() {
                true => id.clone(),
                false => name.to_string(),
            };
            Some((id, name))
        }
        _ if !from.trim().is_empty() => {
            let name = from
                .split('<')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"');
            let id = address(&from);
            let name = match name.is_empty() {
                true => id.clone(),
                false => name.to_string(),
            };
            Some((id, name))
        }
        _ => None,
    }
}

/// The way out most of `offers` (one per message offering any, newest
/// first) agree on, and how many offer another. A tie goes to the way the
/// older messages offered, so one new message never replaces an
/// established way out; the newest message of the way taken gives it.
fn agreed(offers: &[Method]) -> (Method, usize) {
    let mut ways: BTreeMap<(&'static str, Option<String>), (usize, usize, &Method)> =
        BTreeMap::new();
    for (at, offer) in offers.iter().enumerate() {
        let way = ways.entry(offer.way()).or_insert((0, at, offer));
        way.0 += 1;
        way.1 = at;
    }
    ways.into_values()
        .max_by_key(|(agree, oldest, _)| (*agree, *oldest))
        .map_or((Method::None { page: None }, 0), |(agree, _, offer)| {
            (offer.clone(), offers.len() - agree)
        })
}

/// The lists `headers` (one header block per message, newest first) belong
/// to, each with the way out most of its messages offer, most messages
/// first.
pub fn lists(headers: &[(u32, String)]) -> Vec<List> {
    let mut by_id: BTreeMap<String, (String, usize, Vec<Method>)> = BTreeMap::new();
    for (_, block) in headers {
        let Ok((parsed, _)) = mailparse::parse_headers(block.as_bytes()) else {
            continue;
        };
        let Some((id, name)) = list_of(&parsed) else {
            continue;
        };
        let (_, messages, offers) = by_id.entry(id).or_insert((name, 0, Vec::new()));
        *messages += 1;
        if let Some(unsubscribe) = parsed.get_first_value("List-Unsubscribe") {
            let post = parsed.get_first_value("List-Unsubscribe-Post");
            offers.push(method(&unsubscribe, post.as_deref()));
        }
    }
    let mut lists: Vec<List> = by_id
        .into_iter()
        .map(|(id, (name, messages, offers))| {
            let offers: Vec<Method> = offers
                .into_iter()
                .filter(|m| *m != Method::None { page: None })
                .collect();
            let (method, others) = agreed(&offers);
            List {
                id,
                name,
                messages,
                method,
                others,
            }
        })
        .collect();
    lists.sort_by(|a, b| b.messages.cmp(&a.messages).then_with(|| a.id.cmp(&b.id)));
    lists
}

/// The RFC 8058 one-click POST: https only, to a public address, no
/// redirects followed, no cookies, ten seconds.
pub async fn one_click(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Not a URL: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("Only an https one-click address is used.".to_string());
    }
    let checked = parsed.clone();
    tokio::task::spawn_blocking(move || {
        workforce_agent::security::ssrf::check_url_for_ssrf(&checked)
    })
    .await
    .map_err(|e| e.to_string())??;
    let client = workforce_agent::security::ssrf::ssrf_safe_client(Duration::from_secs(10), false);
    let response = client
        .post(parsed)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("List-Unsubscribe=One-Click")
        .send()
        .await
        .map_err(|e| format!("The list's server didn't answer: {}", e.without_url()))?;
    match response.status() {
        s if s.is_success() => Ok(()),
        s => Err(format!("The list's server answered {s}.")),
    }
}

/// The body of every unsubscribe message Desk sends: its own words, never
/// the list's.
pub fn request(person: &str) -> String {
    format!("Please unsubscribe this address from the list.\n\nSent by Desk on behalf of {person}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(lines: &[&str]) -> (u32, String) {
        (1, format!("{}\r\n\r\n", lines.join("\r\n")))
    }

    #[test]
    fn one_click_needs_the_post_header_and_https() {
        assert_eq!(
            method(
                "<mailto:u@list.com?subject=stop>, <https://list.com/u/1>",
                Some("List-Unsubscribe=One-Click")
            ),
            Method::OneClick {
                url: "https://list.com/u/1".to_string()
            }
        );
        assert!(matches!(
            method("<http://list.com/u/1>", Some("List-Unsubscribe=One-Click")),
            Method::None { page: Some(_) }
        ));
        assert!(matches!(
            method("<https://list.com/u/1>, <mailto:u@list.com>", None),
            Method::Mailto { .. }
        ));
        assert_eq!(
            method("<https://list.com/u/1>", None),
            Method::None {
                page: Some("https://list.com/u/1".to_string())
            }
        );
    }

    #[test]
    fn a_mailto_gives_its_address_and_nothing_of_the_lists_words() {
        assert_eq!(
            mailto("mailto:leave%40list.com?subject=Unsubscribe%20me&body=please"),
            Some("leave@list.com".to_string())
        );
        assert_eq!(mailto("MAILTO:u@x.com"), Some("u@x.com".to_string()));
        assert_eq!(mailto("mailto:a@x.com,b@y.com"), None);
        assert_eq!(mailto("mailto:a@x.com%0D%0ABcc:evil@y.com"), None);
        assert_eq!(mailto("https://x.com"), None);
        assert_eq!(
            method("<mailto:a@x.com?subject=Wire%20the%20money>", None),
            Method::Mailto {
                to: "a@x.com".to_string()
            }
        );
        assert_eq!(SUBJECT, "unsubscribe");
    }

    #[test]
    fn one_new_message_never_replaces_the_way_out_the_list_has_offered() {
        let offer = |unsubscribe: &str| {
            block(&[
                "From: Shop <news@shop.com>",
                "List-Id: Shop News <news.shop.com>",
                &format!("List-Unsubscribe: {unsubscribe}"),
            ])
        };
        let newest_first = vec![
            offer("<mailto:boss@victim.com>"),
            offer("<mailto:leave+3@shop.com>"),
            offer("<mailto:leave+2@shop.com>"),
            block(&[
                "From: Shop <news@shop.com>",
                "List-Id: Shop News <news.shop.com>",
            ]),
        ];
        let shop = &lists(&newest_first)[0];
        assert_eq!(shop.messages, 4);
        assert_eq!(
            shop.method,
            Method::Mailto {
                to: "leave+3@shop.com".to_string()
            },
            "the way most messages offer, as the newest of them gives it"
        );
        assert_eq!(shop.others, 1);
        assert_eq!(shop.view()["mixed"], true);
        assert_eq!(shop.view()["offering_another_way"], 1);

        let tied = vec![
            offer("<mailto:boss@victim.com>"),
            offer("<mailto:leave@shop.com>"),
        ];
        assert_eq!(
            lists(&tied)[0].method.via().as_deref(),
            Some("leave@shop.com"),
            "a tie keeps the way offered first"
        );
        let agreed = vec![
            offer("<mailto:leave@shop.com>"),
            offer("<mailto:leave@shop.com>"),
        ];
        assert_eq!(lists(&agreed)[0].others, 0);
        assert_eq!(lists(&agreed)[0].view()["mixed"], false);
    }

    #[test]
    fn messages_group_by_list_id_else_sender() {
        let headers = vec![
            block(&[
                "From: Shop <news@shop.com>",
                "List-Id: Shop News <news.shop.com>",
                "List-Unsubscribe: <https://shop.com/u>",
                "List-Unsubscribe-Post: List-Unsubscribe=One-Click",
            ]),
            block(&[
                "From: Shop <deals@shop.com>",
                "List-Id: Shop News <news.shop.com>",
            ]),
            block(&[
                "From: \"Weekly\" <weekly@blog.com>",
                "List-Unsubscribe: <mailto:leave@blog.com>",
            ]),
            block(&["From: friend@x.com"]),
        ];
        let lists = lists(&headers);
        assert_eq!(lists.len(), 3);
        assert_eq!(lists[0].id, "news.shop.com");
        assert_eq!(lists[0].name, "Shop News");
        assert_eq!(lists[0].messages, 2);
        assert!(matches!(lists[0].method, Method::OneClick { .. }));
        let weekly = lists.iter().find(|l| l.id == "weekly@blog.com").unwrap();
        assert_eq!(weekly.name, "Weekly");
        assert!(matches!(weekly.method, Method::Mailto { .. }));
        let friend = lists.iter().find(|l| l.id == "friend@x.com").unwrap();
        assert_eq!(friend.method, Method::None { page: None });
        assert_eq!(lists[0].view()["via"], "shop.com");
        assert_eq!(lists[0].view()["elsewhere"], false);
        assert_eq!(weekly.view()["via"], "leave@blog.com");
        assert_eq!(weekly.view()["how"], "email");
    }

    #[test]
    fn a_way_out_on_another_domain_is_told_apart() {
        assert!(same_site("news.shop.com", "shop.com"));
        assert!(same_site("weekly@blog.com", "leave@mail.blog.com"));
        assert!(same_site("news@shop.co.uk", "u.shop.co.uk"));
        assert!(!same_site("news@shop.co.uk", "other.co.uk"));
        assert!(!same_site("news.shop.com", "boss@victim.com"));
        let list = List {
            id: "news.shop.com".to_string(),
            name: "Shop".to_string(),
            messages: 1,
            method: Method::Mailto {
                to: "ceo@victim.com".to_string(),
            },
            others: 0,
        };
        assert_eq!(list.view()["elsewhere"], true);
    }

    #[test]
    fn the_request_is_desks_own_words() {
        let body = request("Kat");
        assert!(body.ends_with("Sent by Desk on behalf of Kat."));
    }

    #[tokio::test]
    async fn one_click_never_posts_to_plain_http_or_a_private_address() {
        assert!(one_click("http://shop.com/u").await.is_err());
        assert!(one_click("https://127.0.0.1/u")
            .await
            .unwrap_err()
            .contains("private"));
        assert!(one_click("https://169.254.169.254/latest").await.is_err());
    }
}
