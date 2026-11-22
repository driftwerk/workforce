//! Leaving mailing lists the way their own headers offer (RFC 2369
//! `List-Unsubscribe`, RFC 8058 one-click), never by a link in a body.

use std::collections::BTreeMap;
use std::time::Duration;

use mailparse::MailHeaderMap;
use serde::Serialize;

/// The headers an unsubscribe reads.
pub const HEADERS: &[&str] = &[
    "LIST-UNSUBSCRIBE",
    "LIST-UNSUBSCRIBE-POST",
    "LIST-ID",
    "FROM",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Method {
    /// An RFC 8058 POST to an https URL, which unsubscribes with no page to
    /// read.
    OneClick { url: String },
    /// A message to the list's unsubscribe address.
    Mailto {
        to: String,
        subject: String,
        body: String,
    },
    /// Nothing Desk can do for you: the list offers only a web page, or
    /// nothing.
    None {
        #[serde(skip_serializing_if = "Option::is_none")]
        page: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct List {
    pub id: String,
    pub name: String,
    pub messages: usize,
    #[serde(flatten)]
    pub method: Method,
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

/// The address, subject and body a `mailto:` URL asks for.
pub fn mailto(target: &str) -> Option<(String, String, String)> {
    let rest = target
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("mailto:"))
        .map(|_| &target[7..])?;
    let (to, query) = rest.split_once('?').unwrap_or((rest, ""));
    let to = percent_decoded(to);
    if !to.contains('@') || to.contains(['\r', '\n', ',']) {
        return None;
    }
    let field = |name: &str| {
        query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| percent_decoded(v).replace(['\r', '\n'], " "))
    };
    Some((
        to,
        field("subject").unwrap_or_else(|| "unsubscribe".to_string()),
        field("body").unwrap_or_default(),
    ))
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
        (_, _, Some((to, subject, body))) => Method::Mailto { to, subject, body },
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

/// The lists `headers` (one header block per message) belong to, each with
/// how to leave it, most messages first.
pub fn lists(headers: &[(u32, String)]) -> Vec<List> {
    let mut by_id: BTreeMap<String, List> = BTreeMap::new();
    for (_, block) in headers {
        let Ok((parsed, _)) = mailparse::parse_headers(block.as_bytes()) else {
            continue;
        };
        let Some((id, name)) = list_of(&parsed) else {
            continue;
        };
        let found = parsed
            .get_first_value("List-Unsubscribe")
            .map(|u| {
                method(
                    &u,
                    parsed.get_first_value("List-Unsubscribe-Post").as_deref(),
                )
            })
            .unwrap_or(Method::None { page: None });
        let entry = by_id.entry(id.clone()).or_insert_with(|| List {
            id,
            name,
            messages: 0,
            method: Method::None { page: None },
        });
        entry.messages += 1;
        let better = |m: &Method| match m {
            Method::OneClick { .. } => 2,
            Method::Mailto { .. } => 1,
            Method::None { page: Some(_) } => 0,
            Method::None { page: None } => -1,
        };
        if better(&found) > better(&entry.method) {
            entry.method = found;
        }
    }
    let mut lists: Vec<List> = by_id.into_values().collect();
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
        .map_err(|e| format!("The list's server didn't answer: {e}"))?;
    match response.status() {
        s if s.is_success() => Ok(()),
        s => Err(format!("The list's server answered {s}.")),
    }
}

/// The line every unsubscribe message Desk sends carries.
pub fn on_behalf_of(person: &str) -> String {
    format!("Sent by Desk on behalf of {person}.")
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
    fn a_mailto_gives_its_address_subject_and_body() {
        assert_eq!(
            mailto("mailto:leave%40list.com?subject=Unsubscribe%20me&body=please"),
            Some((
                "leave@list.com".to_string(),
                "Unsubscribe me".to_string(),
                "please".to_string()
            ))
        );
        assert_eq!(
            mailto("MAILTO:u@x.com").map(|m| m.1),
            Some("unsubscribe".to_string())
        );
        assert_eq!(mailto("mailto:a@x.com,b@y.com"), None);
        assert_eq!(mailto("mailto:a@x.com%0D%0ABcc:evil@y.com"), None);
        assert_eq!(mailto("https://x.com"), None);
        assert_eq!(
            mailto("mailto:a@x.com?subject=hi%0D%0ABcc:%20e@y.com").map(|m| m.1),
            Some("hi  Bcc: e@y.com".to_string())
        );
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
