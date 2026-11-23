//! Test calls: one cheap read per service that says whether a key works and
//! whose account it is. They run on the keys in hand, before anything is
//! saved, so a failed Test keeps nothing.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;
use workforce_skills::adapters::metabase::client::access_headers;

/// What a Test found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// The key works; whose account it is.
    Works { account: String },
    /// The service refused the key, in its own words.
    Refused(String),
    /// The service couldn't be reached or answered with an error.
    Failed(String),
}

/// Where each service is reached: the real ones, or a test's.
#[derive(Clone)]
pub struct Services {
    pub http: reqwest::Client,
    pub github_api: String,
    pub github_web: String,
    pub linear_api: String,
    pub slack_api: String,
    /// `None`: `https://api.{site}`.
    pub datadog_api: Option<String>,
    pub bank_api: String,
    /// BankSync's per-person limit, which a check spends: the process's one
    /// outside tests.
    pub bank_limiter: std::sync::Arc<workforce_skills::adapters::bank::banksync::Limiter>,
    pub google_auth: String,
    pub google_token: String,
    pub google_userinfo: String,
    /// FreeAgent's API, whose `approve_app` and `token_endpoint` its
    /// sign-in uses too.
    pub freeagent_api: String,
    /// The shortest wait between two polls of a device sign-in.
    pub poll_floor: Duration,
    /// The GitHub CLI's sign-in on this computer, a GitHub setup offers.
    pub gh: super::gh::GhCli,
}

impl Services {
    pub fn live() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .user_agent("workforce-desk")
                // A redirect would carry the key's header to another host:
                // a Test follows none, and an answer that redirects fails.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
            github_api: "https://api.github.com".into(),
            github_web: "https://github.com".into(),
            linear_api: "https://api.linear.app".into(),
            slack_api: "https://slack.com/api".into(),
            datadog_api: None,
            bank_api: workforce_skills::adapters::bank::banksync::API.into(),
            bank_limiter: workforce_skills::adapters::bank::banksync::Limiter::shared(),
            google_auth: workforce_skills::adapters::google::oauth::AUTH_URL.into(),
            google_token: workforce_skills::adapters::google::oauth::TOKEN_URL.into(),
            google_userinfo: workforce_skills::adapters::google::oauth::USERINFO_URL.into(),
            freeagent_api: "https://api.freeagent.com/v2".into(),
            poll_floor: Duration::from_secs(5),
            gh: super::gh::GhCli::live(),
        }
    }
}

type Fields = BTreeMap<String, String>;

fn field<'a>(fields: &'a Fields, name: &str) -> &'a str {
    fields.get(name).map(String::as_str).unwrap_or_default()
}

/// `site` as an address: `acme.atlassian.net` is `https://acme.atlassian.net`;
/// one given with its scheme keeps it.
pub fn address(site: &str) -> String {
    let site = site.trim().trim_end_matches('/');
    match site.starts_with("https://") || site.starts_with("http://") {
        true => site.to_string(),
        false => format!("https://{site}"),
    }
}

/// Whether `url` is on this computer: `127.0.0.1`, `::1` or `localhost`.
pub fn is_loopback_url(url: &str) -> bool {
    url::Url::parse(url).ok().is_some_and(|u| match u.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    })
}

/// [`address`], refusing plain http anywhere but this computer: a key is
/// never sent unencrypted across a network.
fn secure_address(name: &str, site: &str) -> Result<String, Found> {
    let address = address(site);
    if address.starts_with("http://") && !is_loopback_url(&address) {
        return Err(Found::Refused(format!(
            "{name}'s address must start with https://"
        )));
    }
    Ok(address)
}

/// What a service's answer said, in its own words, short.
fn said(body: &str) -> String {
    let words = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            ["message", "error_description", "error", "errors"]
                .iter()
                .find_map(|k| match v.get(k)? {
                    Value::String(s) => Some(s.clone()),
                    Value::Array(a) => a
                        .first()
                        .and_then(|e| e.get("message").or(Some(e)))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    _ => None,
                })
        })
        .unwrap_or_else(|| body.trim().to_string());
    words.chars().take(200).collect()
}

async fn answer(
    name: &str,
    request: reqwest::RequestBuilder,
) -> Result<(reqwest::StatusCode, String), Found> {
    let response = request
        .send()
        .await
        .map_err(|e| Found::Failed(format!("{name} couldn't be reached: {}", reach(&e))))?;
    let status = response.status();
    if status.is_redirection() {
        return Err(Found::Failed(format!(
            "{name} answered with a redirect to another address; check the address"
        )));
    }
    let body = response.text().await.unwrap_or_default();
    Ok((status, body))
}

/// Why a request didn't reach a service, without its address's query.
fn reach(e: &reqwest::Error) -> &'static str {
    match () {
        _ if e.is_timeout() => "it took too long to answer",
        _ if e.is_connect() => "the connection was refused or the address is wrong",
        _ if e.is_builder() => "that address isn't one Desk can use",
        _ => "the request failed",
    }
}

fn refused_or_failed(name: &str, status: reqwest::StatusCode, body: &str, key: &str) -> Found {
    match status.as_u16() {
        401 | 403 => Found::Refused(format!("{name} says this {key} is invalid")),
        _ => Found::Failed(format!(
            "{name} answered {}: {}",
            status.as_u16(),
            said(body)
        )),
    }
}

/// What a bank whose approval BankSync says lapsed is told: renewed there,
/// then tried again, first connect or Reconnect alike. It names no bank:
/// the name the person gave is scrubbed from what a check says.
const LAPSED: &str = "BankSync says this bank's approval has lapsed: the person reconnects it in BankSync, approving it again at the bank, then tries again here";

fn account(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        k.split('.')
            .try_fold(value, |v, part| v.get(part))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

impl Services {
    /// Tests `fields` against `connector`'s service.
    pub async fn check(&self, connector: &str, person: &str, fields: &Fields) -> Found {
        match self.check_inner(connector, person, fields).await {
            Ok(found) | Err(found) => found,
        }
    }

    async fn check_inner(&self, connector: &str, person: &str, f: &Fields) -> Result<Found, Found> {
        let http = &self.http;
        Ok(match connector {
            "github" => self.github_user(field(f, "token")).await?,
            "linear" => {
                let (status, body) = answer(
                    "Linear",
                    http.post(format!("{}/graphql", self.linear_api))
                        .header("Authorization", field(f, "api_key"))
                        .json(&serde_json::json!({ "query": "{ viewer { email name } }" })),
                )
                .await?;
                let v: Value = serde_json::from_str(&body).unwrap_or_default();
                match account(&v, &["data.viewer.email", "data.viewer.name"]) {
                    Some(account) if status.is_success() => Found::Works { account },
                    _ if status.is_success() || status.as_u16() == 400 => {
                        Found::Refused("Linear says this key is invalid".into())
                    }
                    _ => refused_or_failed("Linear", status, &body, "key"),
                }
            }
            "jira" => {
                let (status, body) = answer(
                    "Jira",
                    http.get(format!(
                        "{}/rest/api/3/myself",
                        secure_address("Jira", field(f, "site"))?
                    ))
                    .basic_auth(field(f, "email"), Some(field(f, "api_token"))),
                )
                .await?;
                match status.is_success() {
                    true => Found::Works {
                        account: account(
                            &serde_json::from_str(&body).unwrap_or_default(),
                            &["emailAddress", "displayName"],
                        )
                        .unwrap_or_else(|| field(f, "email").to_string()),
                    },
                    false => refused_or_failed("Jira", status, &body, "token"),
                }
            }
            "slack" => {
                let (status, body) = answer(
                    "Slack",
                    http.post(format!("{}/auth.test", self.slack_api))
                        .bearer_auth(field(f, "bot_token")),
                )
                .await?;
                let v: Value = serde_json::from_str(&body).unwrap_or_default();
                match v.get("ok").and_then(Value::as_bool) {
                    Some(true) => Found::Works {
                        account: account(&v, &["team", "url"]).unwrap_or_else(|| "Slack".into()),
                    },
                    Some(false) => match v.get("error").and_then(Value::as_str) {
                        Some(
                            "invalid_auth" | "not_authed" | "token_revoked" | "account_inactive",
                        ) => Found::Refused("Slack says this token is invalid".into()),
                        other => Found::Failed(format!(
                            "Slack said {}",
                            other.unwrap_or("it couldn't check the token")
                        )),
                    },
                    None => refused_or_failed("Slack", status, &body, "token"),
                }
            }
            "datadog" => {
                let site = match field(f, "site").trim() {
                    "" => "us1",
                    site => site,
                };
                let api = match &self.datadog_api {
                    Some(api) => api.clone(),
                    None => workforce_skills::adapters::datadog::client::site_to_base_url(site)
                        .map_err(Found::Refused)?,
                };
                let (status, body) = answer(
                    "Datadog",
                    http.get(format!("{api}/api/v1/validate"))
                        .header("DD-API-KEY", field(f, "api_key"))
                        .header("DD-APPLICATION-KEY", field(f, "app_key")),
                )
                .await?;
                match status.is_success() {
                    true => Found::Works {
                        account: site.to_string(),
                    },
                    false => refused_or_failed("Datadog", status, &body, "key"),
                }
            }
            "metabase" => {
                let (status, body) = answer(
                    "Metabase",
                    http.get(format!(
                        "{}/api/user/current",
                        secure_address("Metabase", field(f, "site_url"))?
                    ))
                    .header("x-api-key", field(f, "api_key"))
                    .headers(
                        access_headers(
                            f.get("access_client_id").map(String::as_str),
                            f.get("access_client_secret").map(String::as_str),
                        )
                        .map_err(|e| Found::Refused(e.to_string()))?,
                    ),
                )
                .await?;
                match status.is_success() {
                    true => Found::Works {
                        account: account(
                            &serde_json::from_str(&body).unwrap_or_default(),
                            &["email", "common_name"],
                        )
                        .unwrap_or_else(|| address(field(f, "site_url"))),
                    },
                    false => refused_or_failed("Metabase", status, &body, "key"),
                }
            }
            "email" => check_mailbox(f).await,
            "bank" => self.bank(person, f).await,
            other => Found::Failed(format!("{other} can't be tested")),
        })
    }

    /// Whether a BankSync key only reads, and the bank it names is connected
    /// with its approval standing: the bank's name. It spends `person`'s limit
    /// at BankSync, never another's.
    async fn bank(&self, person: &str, f: &Fields) -> Found {
        use workforce_skills::adapters::bank::model::BankError;
        match workforce_skills::adapters::bank::banksync::check(
            field(f, "api_key"),
            field(f, "bank"),
            &self.bank_api,
            person,
            std::sync::Arc::clone(&self.bank_limiter),
        )
        .await
        {
            Ok(c) if c.connected => Found::Works { account: c.bank },
            Ok(_) | Err(BankError::NeedsReconnect) => Found::Refused(LAPSED.into()),
            Err(
                e @ (BankError::Revoked
                | BankError::ScopeMissing
                | BankError::NotReadOnly
                | BankError::Invalid(_)
                | BankError::NotConfigured(_)),
            ) => Found::Refused(e.to_string()),
            Err(e) => Found::Failed(e.to_string()),
        }
    }

    /// GitHub's `GET /user` with `token`.
    pub async fn github_user(&self, token: &str) -> Result<Found, Found> {
        let (status, body) = answer(
            "GitHub",
            self.http
                .get(format!("{}/user", self.github_api))
                .bearer_auth(token)
                .header("Accept", "application/vnd.github+json"),
        )
        .await?;
        Ok(match status.is_success() {
            true => Found::Works {
                account: account(&serde_json::from_str(&body).unwrap_or_default(), &["login"])
                    .unwrap_or_else(|| "GitHub".into()),
            },
            false => refused_or_failed("GitHub", status, &body, "token"),
        })
    }
}

/// Signs in to the mailbox over IMAP and leaves.
async fn check_mailbox(f: &Fields) -> Found {
    use workforce_skills::adapters::email::client::ImapClient;
    use workforce_skills::adapters::email::error::EmailError;
    let imap = field(f, "imap_host").trim();
    let config = serde_json::json!({
        "imap_host": imap,
        "smtp_host": match field(f, "smtp_host").trim() { "" => imap, smtp => smtp },
        "username": field(f, "username").trim(),
        "password": field(f, "password"),
        "connect_timeout_secs": 15,
        "command_timeout_secs": 20,
    });
    let config = match serde_json::from_value(config) {
        Ok(config) => config,
        Err(_) => return Found::Failed("Those mail settings aren't complete".into()),
    };
    match ImapClient::connect(config).await {
        Ok(_) => Found::Works {
            account: field(f, "username").trim().to_string(),
        },
        Err(EmailError::AuthFailed { .. }) => {
            Found::Refused("The mail server says this address or password is wrong".into())
        }
        Err(EmailError::ConnectionRefused { host, .. }) => {
            Found::Failed(format!("{host} couldn't be reached"))
        }
        Err(EmailError::Timeout { .. }) => Found::Failed(format!("{imap} took too long to answer")),
        Err(EmailError::TlsHandshake { host, .. }) => {
            Found::Failed(format!("{host} didn't accept a secure connection"))
        }
        Err(_) => Found::Failed(format!("{imap} couldn't be signed in to")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_site_is_an_address() {
        assert_eq!(address("acme.atlassian.net"), "https://acme.atlassian.net");
        assert_eq!(
            address(" https://acme.atlassian.net/ "),
            "https://acme.atlassian.net"
        );
        assert_eq!(address("http://127.0.0.1:9"), "http://127.0.0.1:9");
    }

    #[test]
    fn only_this_computer_is_loopback_and_plain_http_goes_nowhere_else() {
        assert!(is_loopback_url(
            "http://127.0.0.1:50111/api/auth/google/callback"
        ));
        assert!(is_loopback_url("http://localhost:8080/cb"));
        assert!(is_loopback_url("http://[::1]:8080/cb"));
        assert!(!is_loopback_url(
            "https://hub.acme.example/api/auth/google/callback"
        ));
        assert!(!is_loopback_url("http://127.0.0.1.acme.example/cb"));
        assert!(secure_address("Jira", "acme.atlassian.net").is_ok());
        assert!(secure_address("Jira", "http://127.0.0.1:9").is_ok());
        assert!(matches!(
            secure_address("Jira", "http://jira.acme.example"),
            Err(Found::Refused(_))
        ));
    }

    #[test]
    fn a_services_words_are_short_and_its_own() {
        assert_eq!(said(r#"{"message":"Bad credentials"}"#), "Bad credentials");
        assert_eq!(
            said(r#"{"errors":[{"message":"Authentication required"}]}"#),
            "Authentication required"
        );
        assert_eq!(said(&"x".repeat(500)).len(), 200);
    }
}
