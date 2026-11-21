//! Signing in to a service on the person's own app: Google's and
//! FreeAgent's sign-in pages, PKCE where the service takes it, and a
//! loopback redirect to this hub's callback.
//!
//! Desk ships no app of its own (FreeAgent's terms forbid one in public
//! code; Google's restricted scopes would need a paid assessment for a
//! shared one). The person's app is kept hub-wide where the adapters read
//! it, `skills/<service>/oauth_client/`. Its secret is an installed app's,
//! which is not confidential by its nature: it is still kept only in the
//! vault and never shown, logged or handed to the model.
//!
//! A sign-in's `state` is Connections' own: both hub callbacks hand one to
//! [`callback`] first, and every other flow goes on as it did.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;
use tracing::{error, warn};
use workforce_common::settings::{SecretStore, VaultActor, VaultSource};
use workforce_desk::connections::Level;
use workforce_oauth_client::{
    ClientAuth, OAuthClient, OAuthClientCreds, OAuthProvider, PathLayout,
    TokenStore as GenericTokenStore,
};

use super::catalog::{self, Connector};
use super::check::{Found, Services};
use super::{key_at, scrub, Conn, Connections, FlowState, Refusal, Started, DEFAULT_INSTANCE};
use crate::vault::VaultOp;

/// Which service an app signs in to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Google,
    FreeAgent,
}

/// A service signed in to on the person's own app.
#[derive(Debug)]
pub struct App {
    pub connector: &'static str,
    kind: Kind,
    /// The hub route the service redirects to.
    callback: &'static str,
    /// The loopback host the redirect names: FreeAgent's is registered
    /// with the app as `localhost`.
    host: &'static str,
    scopes: &'static [&'static str],
    pkce: bool,
    auth: ClientAuth,
    extra: &'static [(&'static str, &'static str)],
    /// The sub that tells one account from another.
    identity: &'static str,
    /// What an app's id and secret look like, whole.
    id_shape: &'static str,
    secret_shape: &'static str,
}

const GOOGLE_SCOPES: &[(&str, &str)] = &[
    ("Calendar", "https://www.googleapis.com/auth/calendar"),
    ("Drive", "https://www.googleapis.com/auth/drive"),
    ("Gmail", "https://mail.google.com/"),
];

pub const APPS: [App; 2] = [
    App {
        connector: "google",
        kind: Kind::Google,
        callback: "/api/auth/google/callback",
        host: "127.0.0.1",
        scopes: workforce_skills::adapters::google::oauth::APP_SCOPES,
        pkce: true,
        auth: ClientAuth::RequestBody,
        extra: &[("access_type", "offline"), ("prompt", "consent")],
        identity: "email",
        id_shape: r"^[0-9]{6,20}-[0-9a-z]{32}\.apps\.googleusercontent\.com$",
        secret_shape: r"^GOCSPX-[A-Za-z0-9_-]{20,40}$",
    },
    App {
        connector: "freeagent",
        kind: Kind::FreeAgent,
        callback: "/api/auth/freeagent/callback",
        host: "localhost",
        scopes: &[],
        pkce: false,
        auth: ClientAuth::Basic,
        extra: &[],
        identity: "company_id",
        id_shape: r"^[A-Za-z0-9_-]{16,64}$",
        secret_shape: r"^[A-Za-z0-9_-]{16,64}$",
    },
];

/// The app `connector` signs in on, if it signs in that way.
pub fn of(connector: &str) -> Option<&'static App> {
    APPS.iter().find(|a| a.connector == connector)
}

/// Where the person's app is, for a sign-in to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppState {
    Ready,
    /// No app is kept yet.
    Missing,
    /// FreeAgent's app names an older address of this hub.
    Moved,
}

/// The person's app, as kept.
#[derive(Clone)]
pub(super) struct Creds {
    id: String,
    secret: String,
    redirect: Option<String>,
}

impl Creds {
    fn oauth(&self, redirect: &str) -> OAuthClientCreds {
        OAuthClientCreds {
            client_id: self.id.clone(),
            client_secret: Some(self.secret.clone()),
            redirect_uri: Some(redirect.to_string()),
        }
    }
}

/// A sign-in sent to the service's page, waiting for its redirect.
pub struct Pending {
    flow: String,
    connector: &'static str,
    verifier: String,
    redirect: String,
    /// The app it started on: the one its code is exchanged with.
    creds: Creds,
    until: Instant,
}

/// Who signed in.
struct Account {
    name: String,
    identity: String,
    subs: Vec<(&'static str, String)>,
}

/// A service's endpoints, as [`Services`] reaches them.
struct Provider {
    name: &'static str,
    authorize: String,
    token: String,
    pkce: bool,
    auth: ClientAuth,
    extra: &'static [(&'static str, &'static str)],
}

impl OAuthProvider for Provider {
    fn name(&self) -> &str {
        self.name
    }
    fn authorization_endpoint(&self) -> &str {
        &self.authorize
    }
    fn token_endpoint(&self) -> &str {
        &self.token
    }
    fn default_scopes(&self) -> &[&str] {
        &[]
    }
    fn extra_authorize_params(&self) -> Vec<(&str, &str)> {
        self.extra.to_vec()
    }
    fn uses_pkce(&self) -> bool {
        self.pkce
    }
    fn client_auth(&self) -> ClientAuth {
        self.auth
    }
}

/// Where a connection's tokens are, at its tier: what the adapter's token
/// store reads and writes, so a check refreshes as a call would.
#[derive(Debug)]
struct Layout {
    c: &'static Connector,
    tier: VaultSource,
    person: String,
}

impl PathLayout for Layout {
    fn path(&self, target: &str, suffix: &str) -> String {
        key_at(self.tier, &self.person, &self.c.at(target, suffix))
    }
}

impl App {
    fn service(&self) -> &'static Connector {
        catalog::find(self.connector).expect("an app's service is in the catalog")
    }

    fn client_path(&self, field: &str) -> String {
        format!("skills/{}/oauth_client/{field}", self.connector)
    }

    /// This hub's callback for the app, from the gateway's listen address.
    pub fn redirect(&self, gateway: &str) -> String {
        let port = gateway.rsplit_once(':').map_or("8080", |(_, port)| port);
        format!("http://{}:{port}{}", self.host, self.callback)
    }

    fn provider(&self, s: &Services) -> Provider {
        let (authorize, token) = match self.kind {
            Kind::Google => (s.google_auth.clone(), s.google_token.clone()),
            Kind::FreeAgent => (
                format!("{}/approve_app", s.freeagent_api),
                format!("{}/token_endpoint", s.freeagent_api),
            ),
        };
        Provider {
            name: self.connector,
            authorize,
            token,
            pkce: self.pkce,
            auth: self.auth,
            extra: self.extra,
        }
    }

    fn client(&self, s: &Services, creds: &Creds, redirect: &str) -> OAuthClient {
        OAuthClient::new(
            Arc::new(self.provider(s)),
            creds.oauth(redirect),
            s.http.clone(),
        )
    }

    /// Whether `v` looks like this app's `field`: `client_id` or
    /// `client_secret`.
    pub fn field_shaped(&self, field: &str, v: &str) -> bool {
        let shape = match field {
            "client_id" => self.id_shape,
            "client_secret" => self.secret_shape,
            _ => return false,
        };
        regex::Regex::new(shape)
            .expect("an app's shape compiles")
            .is_match(v)
    }

    /// Whether `id` and `secret` look like this app's.
    pub fn shaped(&self, id: &str, secret: &str) -> bool {
        self.field_shaped("client_id", id) && self.field_shaped("client_secret", secret)
    }

    /// Who an access token signs in as.
    async fn account(&self, s: &Services, token: &str) -> Result<Account, String> {
        let name = self.service().name;
        let url = match self.kind {
            Kind::Google => s.google_userinfo.clone(),
            Kind::FreeAgent => format!("{}/company", s.freeagent_api),
        };
        let answer = s
            .http
            .get(&url)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|_| format!("{name} couldn't be reached"))?;
        if !answer.status().is_success() {
            return Err(format!("{name} didn't say whose account this is"));
        }
        let v: Value = answer.json().await.unwrap_or_default();
        let text = |v: &Value| {
            v.as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        match self.kind {
            Kind::Google => {
                let email = text(&v["email"]).ok_or_else(|| format!("{name} gave no email"))?;
                Ok(Account {
                    name: email.clone(),
                    identity: email.to_lowercase(),
                    subs: vec![("email", email)],
                })
            }
            Kind::FreeAgent => {
                let company = &v["company"];
                // Its `url` is the same for every business: its `id` isn't.
                let id =
                    text(&company["id"]).or_else(|| company["id"].as_u64().map(|n| n.to_string()));
                let (Some(name), Some(id)) = (text(&company["name"]), id) else {
                    return Err("FreeAgent didn't say which business this is".into());
                };
                Ok(Account {
                    identity: id.clone(),
                    subs: vec![("company", name.clone()), ("company_id", id)],
                    name,
                })
            }
        }
    }

    /// What a granted scope list lacks, by the names the person knows.
    fn missing(&self, granted: &str) -> Vec<&'static str> {
        match self.kind {
            Kind::Google => GOOGLE_SCOPES
                .iter()
                .filter(|(_, scope)| !granted.split_whitespace().any(|g| g == *scope))
                .map(|(name, _)| *name)
                .collect(),
            Kind::FreeAgent => Vec::new(),
        }
    }

    /// What a connection lacks, said; none when it lacks nothing.
    fn lacking(&self, account: &str, granted: &str) -> Option<String> {
        let missing = self.missing(granted);
        (!missing.is_empty()).then(|| {
            let names = missing.join(" or ");
            format!(
                "Connected as {account} without {names}. Approve it again and tick {names} to add {}.",
                if missing.len() == 1 { "it" } else { "them" }
            )
        })
    }

    /// The words for approving connection `name` again as another account.
    fn not_this(&self, account: &str, name: &str) -> String {
        match self.kind {
            Kind::Google => format!(
                "That's {account}, but this connection is '{name}'. Sign in with that Google account, then allow."
            ),
            Kind::FreeAgent => format!(
                "That's {account}, but this connection is '{name}'. On FreeAgent's page, sign in as {name}, then approve."
            ),
        }
    }
}

/// The words for a sign-in a service no longer honours.
fn lapsed(c: &Connector) -> String {
    format!(
        "{} signed this connection out: its access was removed or has lapsed. Approve it again.",
        c.name
    )
}

/// The words for a connection approved on an app no longer kept.
fn other_app(c: &Connector) -> String {
    format!(
        "This connection was approved on an earlier {} app. Approve it again.",
        c.name
    )
}

/// The words for a FreeAgent app made on an older address of this hub.
fn moved(c: &Connector, redirect: &str) -> String {
    format!(
        "Desk's address changed since your {} app was made. Ask your agent to update it, or add {redirect} to the app's redirect URIs.",
        c.name
    )
}

impl Connections {
    fn app_creds(&self, app: &App) -> Option<Creds> {
        let global = self.global.as_ref()?;
        Some(Creds {
            id: global.get(&app.client_path("client_id"))?,
            secret: global.get(&app.client_path("client_secret"))?,
            redirect: global.get(&app.client_path("redirect_uri")),
        })
    }

    /// Where `c`'s app is, for a service that signs in on one.
    pub fn app_state(&self, c: &Connector) -> Option<AppState> {
        let app = of(c.id)?;
        Some(match self.app_creds(app) {
            None => AppState::Missing,
            Some(creds)
                if app.kind == Kind::FreeAgent
                    && creds.redirect.as_deref() != Some(app.redirect(&self.gateway).as_str()) =>
            {
                AppState::Moved
            }
            Some(_) => AppState::Ready,
        })
    }

    /// Keeps the person's own app for `id`, hub-wide where the adapter
    /// reads it, and loads the service again on every agent, so no client
    /// built on an earlier app is kept.
    pub async fn keep_app(
        &self,
        person: &str,
        id: &str,
        client_id: &str,
        client_secret: &str,
    ) -> Result<(), Refusal> {
        let c = Self::connector(id)?;
        let app = of(c.id).ok_or_else(|| {
            Refusal::NotFound(format!("{} doesn't sign in on an app of yours", c.name))
        })?;
        let (client_id, client_secret) = (client_id.trim(), client_secret.trim());
        if client_id.is_empty() || client_secret.is_empty() {
            return Err(Refusal::Invalid(format!(
                "Your {} app's id and secret are both needed",
                c.name
            )));
        }
        if !app.shaped(client_id, client_secret) {
            return Err(Refusal::Invalid(format!(
                "That doesn't look like a {} app's id and secret",
                c.name
            )));
        }
        let vault = self.vault_at(VaultSource::Global, &Level::All)?;
        let redirect = app.redirect(&self.gateway);
        let (id_path, secret_path, redirect_path) = (
            app.client_path("client_id"),
            app.client_path("client_secret"),
            app.client_path("redirect_uri"),
        );
        let mut ops = vec![
            VaultOp::Set {
                path: &id_path,
                value: client_id,
            },
            VaultOp::Set {
                path: &secret_path,
                value: client_secret,
            },
        ];
        if app.kind == Kind::FreeAgent {
            ops.push(VaultOp::Set {
                path: &redirect_path,
                value: &redirect,
            });
        }
        vault
            .apply(&ops, VaultActor::User(person))
            .map_err(Refusal::Internal)?;
        self.reload(c, &Level::All, person).await;
        Ok(())
    }

    /// Keeps that the person's FreeAgent app now names this hub's address
    /// too, once they or their agent added it there.
    pub(super) fn keep_app_address(&self, person: &str, c: &Connector) -> Result<(), Refusal> {
        let app = of(c.id)
            .filter(|a| a.kind == Kind::FreeAgent)
            .ok_or_else(|| Refusal::Invalid(format!("{} keeps no address of Desk's", c.name)))?;
        let vault = self.vault_at(VaultSource::Global, &Level::All)?;
        let (path, redirect) = (app.client_path("redirect_uri"), app.redirect(&self.gateway));
        vault
            .apply(
                &[VaultOp::Set {
                    path: &path,
                    value: &redirect,
                }],
                VaultActor::User(person),
            )
            .map(drop)
            .map_err(Refusal::Internal)
    }

    /// Sends the person to `c`'s sign-in page on their app, for instance
    /// `key` (`None`: named after its account once signed in) at `level`.
    /// `again` signs connection `key` in again, keeping its name.
    pub(super) fn begin_app(
        &self,
        person: &str,
        c: &'static Connector,
        level: Level,
        key: Option<String>,
        name: Option<String>,
        again: bool,
    ) -> Result<Started, Refusal> {
        let app = of(c.id).ok_or_else(|| {
            Refusal::Invalid(format!("{} doesn't sign in on an app of yours", c.name))
        })?;
        let redirect = app.redirect(&self.gateway);
        let creds = match self.app_state(c) {
            Some(AppState::Moved) => return Err(Refusal::Invalid(moved(c, &redirect))),
            _ => self.app_creds(app).ok_or_else(|| {
                Refusal::Invalid(format!(
                    "Your {} app isn't set up yet. Ask your agent to set it up.",
                    c.name
                ))
            })?,
        };
        self.vault_at(Self::tiers(c, &level)[0], &level)?;
        let web = app
            .client(&self.services, &creds, &redirect)
            .start_web_flow(&redirect, app.scopes);
        let flow = self.insert_flow(person, c.id, level, key, name, again);
        let mut pending = self.pending.lock().expect("pending sign-ins poisoned");
        let now = Instant::now();
        pending.retain(|_, p| p.until > now);
        pending.insert(
            web.state,
            Pending {
                flow: flow.clone(),
                connector: c.id,
                verifier: web.verifier,
                redirect,
                creds,
                until: now + super::FLOW_TTL,
            },
        );
        Ok(Started {
            flow,
            url: Some(web.authorization_url),
            user_code: None,
            verification_uri: None,
            expires_in: super::FLOW_TTL.as_secs(),
        })
    }

    /// Signs connection `conn` of `id` in again on the person's app: its
    /// name, its agents' choice of it and its default stay as they are.
    pub async fn approve_again(
        &self,
        person: &str,
        id: &str,
        conn: &str,
    ) -> Result<Started, Refusal> {
        let c = Self::connector(id)?;
        let conn = self.conn(conn)?;
        self.held_at(c, &conn.level, &conn.key, person)
            .ok_or_else(|| {
                Refusal::NotFound(format!("That {} connection isn't there any more", c.name))
            })?;
        self.begin_app(person, c, conn.level, Some(conn.key), None, true)
    }

    /// The sign-in waiting on `state`, taken: once.
    pub fn take_pending(&self, state: &str) -> Option<Pending> {
        let mut pending = self.pending.lock().expect("pending sign-ins poisoned");
        let p = pending.remove(state)?;
        (p.until > Instant::now()).then_some(p)
    }

    /// The person's connection of `c` that is `identity`'s: where it is
    /// and its name.
    fn same_account(
        &self,
        c: &Connector,
        app: &App,
        person: &str,
        identity: &str,
    ) -> Option<(Conn, String)> {
        self.levels().into_iter().find_map(|level| {
            self.instances(c, &level, person)
                .into_iter()
                .find_map(|(key, tier)| {
                    self.identity_at(c, app, &level, &key, tier, person)
                        .filter(|kept| kept.eq_ignore_ascii_case(identity))?;
                    let (name, _) = self.named_at(c, &level, &key, tier, person);
                    Some((
                        Conn {
                            level: level.clone(),
                            key: key.clone(),
                        },
                        name.unwrap_or(key),
                    ))
                })
        })
    }

    /// The account connection `key` at `level` was signed in as.
    fn identity_at(
        &self,
        c: &Connector,
        app: &App,
        level: &Level,
        key: &str,
        tier: VaultSource,
        person: &str,
    ) -> Option<String> {
        self.vault_at(tier, level)
            .ok()?
            .get(&key_at(tier, person, &c.at(key, app.identity)))
    }

    /// Completes sign-in `p` with the service's `code`: the exchange on the
    /// app and redirect it started on, then its tokens and account kept as
    /// its instance at its level's tier. An account connected already is
    /// signed in again where it is kept, under its name; approving a
    /// connection again takes only the account it is. Nothing is kept once
    /// it was cancelled.
    pub async fn complete(&self, p: Pending, code: &str) -> Result<String, String> {
        let c = catalog::find(p.connector).expect("a sign-in's service is in the catalog");
        let app = of(c.id).expect("a sign-in's service signs in on an app");
        let gone = || "This sign-in was cancelled or has expired. Nothing was saved.".to_string();
        let Some(f) = self.waiting_flow(&p.flow) else {
            return Err(gone());
        };
        let fail = |said: String| {
            self.settle(
                &p.flow,
                FlowState::Failed {
                    error: said.clone(),
                },
            );
            Err(format!("{said} Nothing was saved."))
        };
        let tokens = match app
            .client(&self.services, &p.creds, &p.redirect)
            .exchange_web_code(code, &p.verifier, &p.redirect)
            .await
        {
            Ok(tokens) => tokens,
            Err(e) => {
                let secrets = BTreeMap::from([("secret".to_string(), p.creds.secret.clone())]);
                error!(connector = c.id, error = %scrub(&e.to_string(), &secrets), "a sign-in failed at the exchange");
                return fail(format!("{} didn't finish the sign-in.", c.name));
            }
        };
        let account = match app.account(&self.services, &tokens.access_token).await {
            Ok(account) => account,
            Err(said) => return fail(format!("{said}.")),
        };
        if self.waiting_flow(&p.flow).is_none() {
            return Err(gone());
        }
        let (conn, again) = match (f.again, f.key.clone()) {
            (true, Some(key)) => {
                let held = self.held_at(c, &f.level, &key, &f.person);
                let kept =
                    held.and_then(|tier| self.identity_at(c, app, &f.level, &key, tier, &f.person));
                if kept.is_some_and(|kept| !kept.eq_ignore_ascii_case(&account.identity)) {
                    let name = held
                        .and_then(|tier| self.named_at(c, &f.level, &key, tier, &f.person).0)
                        .unwrap_or_else(|| key.clone());
                    return fail(app.not_this(&account.name, &name));
                }
                (
                    Conn {
                        level: f.level.clone(),
                        key,
                    },
                    true,
                )
            }
            (_, key) => match self.same_account(c, app, &f.person, &account.identity) {
                Some((conn, _)) => (conn, true),
                None => {
                    let key = match key {
                        Some(key) => key,
                        None if self.keys_in_use(c, &f.person).is_empty() => {
                            DEFAULT_INSTANCE.to_string()
                        }
                        None => self.new_key(c, &f.person, &account.name),
                    };
                    (
                        Conn {
                            level: f.level.clone(),
                            key,
                        },
                        false,
                    )
                }
            },
        };
        let refreshed = again && !f.again;
        let key = conn.key.clone();
        let tier = match again {
            true => self
                .held_at(c, &conn.level, &key, &f.person)
                .unwrap_or_else(|| Self::tiers(c, &conn.level)[0]),
            false => Self::tiers(c, &conn.level)[0],
        };
        let Ok(vault) = self.vault_at(tier, &conn.level) else {
            return fail("Desk couldn't keep the sign-in.".into());
        };
        let at = |sub: &str| key_at(tier, &f.person, &c.at(&key, sub));
        let expires_at = tokens.expires_at.to_string();
        let label = (!again).then(|| f.name.clone().unwrap_or_else(|| account.name.clone()));
        let values: Vec<(String, &str)> = [
            ("refresh_token", tokens.refresh_token.as_str()),
            ("access_token", tokens.access_token.as_str()),
            ("expires_at", expires_at.as_str()),
            ("scopes", tokens.scopes.as_str()),
        ]
        .into_iter()
        .chain(account.subs.iter().map(|(sub, v)| (*sub, v.as_str())))
        .chain([("client_id", p.creds.id.as_str())])
        .chain(label.as_deref().map(|l| (catalog::LABEL, l)))
        .map(|(sub, v)| (at(sub), v))
        .collect();
        let ops: Vec<VaultOp<'_>> = values
            .iter()
            .map(|(path, value)| VaultOp::Set { path, value })
            .collect();
        if let Err(e) = vault.apply(&ops, VaultActor::User(&f.person)) {
            error!(connector = c.id, error = %format!("{e:#}"), "keeping a sign-in failed");
            return fail("Desk couldn't keep the sign-in.".into());
        }
        let note = app.lacking(&account.name, &tokens.scopes);
        let found = Found::Works {
            account: account.name.clone(),
        };
        self.record_noted(c, &conn, &f.person, &found, true, note.clone())
            .await;
        self.reload(c, &conn.level, &f.person).await;
        let said = match refreshed {
            true => {
                let tier = self.held_at(c, &conn.level, &key, &f.person);
                let name = tier
                    .and_then(|tier| self.named_at(c, &conn.level, &key, tier, &f.person).0)
                    .unwrap_or_else(|| key.clone());
                format!(
                    "{} was connected already as '{name}': it's signed in again as {}.",
                    c.name, account.name
                )
            }
            false => format!("{} is connected as {}.", c.name, account.name),
        };
        self.settle_at(
            &p.flow,
            conn,
            FlowState::Connected {
                account: account.name.clone(),
            },
        );
        Ok(note.unwrap_or(said))
    }

    /// Checks connection `conn` of `c` through the adapter's own token
    /// store, which refreshes only when due and keeps a rotated refresh
    /// token, then asks whose it is. What it lacks is said beside it.
    pub(super) async fn test_app(
        &self,
        c: &Connector,
        conn: &Conn,
        tier: VaultSource,
        person: &str,
    ) -> (Found, Option<String>) {
        let Some(app) = of(c.id) else {
            return (Found::Failed(format!("{} has no app", c.name)), None);
        };
        let c = app.service();
        let Some(creds) = self.app_creds(app) else {
            return (
                Found::Refused(format!("Your {} app isn't kept in Desk any more", c.name)),
                None,
            );
        };
        let Ok(vault) = self.vault_at(tier, &conn.level) else {
            return (
                Found::Failed("Desk couldn't read this connection".into()),
                None,
            );
        };
        let at = |sub: &str| key_at(tier, person, &c.at(&conn.key, sub));
        if vault
            .get(&at("client_id"))
            .is_some_and(|minted| minted != creds.id)
        {
            return (Found::Refused(other_app(c)), None);
        }
        let layout: Arc<dyn PathLayout> = Arc::new(Layout {
            c,
            tier,
            person: person.to_string(),
        });
        let client = Arc::new(app.client(&self.services, &creds, &app.redirect(&self.gateway)));
        let store = Arc::clone(&vault) as Arc<dyn SecretStore>;
        let tokens = GenericTokenStore::new(store, conn.key.clone(), layout, client);
        let refused = |e: eyre::Report| {
            let said = format!("{e:#}");
            match said.contains("invalid_grant") {
                true => Found::Refused(lapsed(c)),
                false => {
                    let secrets = BTreeMap::from([("secret".to_string(), creds.secret.clone())]);
                    warn!(connector = c.id, error = %scrub(&said, &secrets), "a connection's check couldn't refresh");
                    Found::Failed(format!("{} couldn't be reached", c.name))
                }
            }
        };
        let token = match tokens.access_token().await {
            Ok(token) => token,
            Err(e) => return (refused(e), None),
        };
        // A token kept fresh may still have been revoked: signed in again
        // once before it is said to fail.
        let account = match app.account(&self.services, &token).await {
            Ok(account) => Ok(account),
            Err(_) => match tokens.force_refresh().await {
                Ok(token) => app.account(&self.services, &token).await,
                Err(e) => return (refused(e), None),
            },
        };
        match account {
            Ok(account) => {
                let granted = vault.get(&at("scopes")).unwrap_or_default();
                let note = app.lacking(&account.name, &granted);
                (
                    Found::Works {
                        account: account.name,
                    },
                    note,
                )
            }
            Err(said) => (Found::Failed(said), None),
        }
    }
}

/// Completes a sign-in Connections started, when `state` is one of its:
/// the page the person's browser ends on. `None` for any other flow, which
/// its callback completes as before.
pub async fn callback(state: Option<&str>, code: Option<&str>) -> Option<Response> {
    let connections = super::installed()?;
    let pending = connections.take_pending(state?)?;
    Some(match code {
        None => {
            connections.settle(&pending.flow, FlowState::Cancelled);
            page(false, "The sign-in was turned down. Nothing was saved.")
        }
        Some(code) => match connections.complete(pending, code).await {
            Ok(said) => page(true, &said),
            Err(said) => page(false, &said),
        },
    })
}

/// The page a sign-in ends on.
fn page(done: bool, said: &str) -> Response {
    let (title, status) = match done {
        true => ("Connected", StatusCode::OK),
        false => ("Not connected", StatusCode::BAD_REQUEST),
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=utf-8><title>{title}</title>\
<style>body{{font-family:system-ui,sans-serif;max-width:480px;margin:4rem auto;text-align:center}}</style>\
</head><body><h1>{title}</h1><p>{}</p><p>You can close this tab and go back to Desk.</p></body></html>",
        crate::formatting::html_escape(said)
    );
    (status, Html(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_app_is_a_catalog_service_that_signs_in_on_one() {
        for app in &APPS {
            let c = catalog::find(app.connector).expect("in the catalog");
            assert_eq!(c.sign_in, catalog::SignIn::App, "{}", c.id);
            assert!(
                c.subs.contains(&app.identity),
                "{} keeps its identity",
                c.id
            );
            assert!(c.subs.contains(&"client_id"), "{} keeps its app", c.id);
        }
        for c in catalog::CATALOG
            .iter()
            .filter(|c| c.sign_in == catalog::SignIn::App)
        {
            assert!(of(c.id).is_some(), "{} has an app", c.id);
        }
    }

    #[test]
    fn the_redirect_is_this_hubs_loopback_callback() {
        let google = of("google").unwrap();
        let freeagent = of("freeagent").unwrap();
        assert_eq!(
            google.redirect("127.0.0.1:50111"),
            "http://127.0.0.1:50111/api/auth/google/callback"
        );
        assert_eq!(
            freeagent.redirect("127.0.0.1:50111"),
            "http://localhost:50111/api/auth/freeagent/callback"
        );
    }

    #[test]
    fn an_apps_id_and_secret_are_checked_by_their_shape() {
        let google = of("google").unwrap();
        let id = format!(
            "123456789012-{}.apps.googleusercontent.com",
            "a1".repeat(16)
        );
        let secret = format!("GOCSPX-{}", "x".repeat(28));
        assert!(google.shaped(&id, &secret));
        assert!(!google.shaped("123", &secret));
        assert!(!google.shaped(&id, "not-a-secret"));
        let freeagent = of("freeagent").unwrap();
        assert!(freeagent.shaped("p0sIUgWBrTC8BOy5B-6bmA", "Zx9_kq2LmN4pR7sT1vW3yA"));
        assert!(!freeagent.shaped("short", "Zx9_kq2LmN4pR7sT1vW3yA"));
        assert!(!freeagent.shaped("p0sIUgWBrTC8BOy5B-6bmA", "has spaces in it, sixteen+"));
    }

    #[test]
    fn missing_google_scopes_are_named() {
        let google = of("google").unwrap();
        let all = "https://www.googleapis.com/auth/calendar https://www.googleapis.com/auth/drive https://mail.google.com/";
        assert!(google.missing(all).is_empty());
        assert_eq!(
            google.missing("https://www.googleapis.com/auth/calendar"),
            ["Drive", "Gmail"]
        );
        assert_eq!(
            google.lacking("sarah@example.com", "https://www.googleapis.com/auth/calendar https://www.googleapis.com/auth/drive").as_deref(),
            Some("Connected as sarah@example.com without Gmail. Approve it again and tick Gmail to add it.")
        );
        assert!(of("freeagent").unwrap().missing("").is_empty());
    }
}
