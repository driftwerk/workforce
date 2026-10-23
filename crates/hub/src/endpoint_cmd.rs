//! `cmd: endpoint …` — provision the `http.*` endpoints from chat.
//!
//! The `http/` vault namespace is denied to every `vault.*` tool
//! ([`crate::vault_policy::agent_can_read`]) so an agent can never point a
//! stored credential at a host of its choosing. That left provisioning as a
//! host-shell-only task, which in practice meant an agent telling the user to
//! go and run `workforce vault set` — a wall in the middle of the workflow it
//! was building.
//!
//! This command reopens that path for *humans* without reopening it for the
//! agent. A `cmd:` is parsed from the user's own message text only
//! ([`crate::commands::parse_command`], called from `slack_manager` on inbound
//! user messages), never from a model response or a tool result, so the agent
//! can describe this command but cannot issue one.
//!
//! Two privilege levels, split on what each field can do:
//!
//! * **`add` / `auth` — SystemAdmin only, written to the shared tier.** These
//!   name the *host* and how it authenticates. Whoever controls `base_url`
//!   controls where every credential under that endpoint is sent, so this is
//!   the operator half of the containment invariant and it stays admin-gated.
//! * **`cred` — any identity, written to their own per-user tier.** The host is
//!   already fixed by the time these are read, so a member supplying their own
//!   password widens nothing: `config::resolve_at` prefers a per-user override
//!   over the shared value, so each person's calls use their own credential
//!   and no one can read anyone else's.
//!
//! A value typed here is a live credential. It is never echoed back, never
//! logged, and the audit record carries its length only.

use std::sync::Arc;

use workforce_agent::tools::http::config::{self, ENDPOINTS_PREFIX};
use workforce_common::settings::{SecretStore, VaultActor, VaultSource};

use crate::commands::CommandResult;
use crate::serve::AgentService;

pub const ENDPOINT_USAGE: &str =
    "`cmd: endpoint` — configure the endpoints `http.request` can call.\n\n\
     *Anyone:*\n\
     • `cmd: endpoint list` — what is configured, and what each endpoint is still missing\n\
     • `cmd: endpoint cred <endpoint> <key> <value>` — set YOUR OWN credential for an \
     endpoint. Stored against your identity, never shown, never shared. The agent \
     references it as `[[vault:<key>]]`.\n\n\
     *Admins only* (these name the host every credential is sent to):\n\
     • `cmd: endpoint add <name> <base_url>`\n\
     • `cmd: endpoint auth <name> <bearer|basic|header|none> [header-name]`\n\n\
     For a one-off call that needs no endpoint at all, use `cmd: secret <name> <value>` \
     instead and let the agent pass `[[secret:<name>]]`.";

/// Endpoint fields that are the endpoint's *own* authentication credential
/// rather than a value the request body references. `cred` routes these to
/// `http/endpoints/<name>/<field>`; everything else goes under `creds/`.
const AUTH_CREDENTIAL_FIELDS: &[&str] = &["token", "username", "password"];

/// Auth kinds `read_auth` understands.
const AUTH_KINDS: &[&str] = &["bearer", "basic", "header", "none"];

pub async fn execute_endpoint(
    service: &Arc<AgentService>,
    user: &str,
    args: Option<&str>,
) -> CommandResult {
    let args = args.unwrap_or("").trim();
    let (sub, rest) = split_word(args);
    match sub.to_ascii_lowercase().as_str() {
        "" | "help" => CommandResult::info(ENDPOINT_USAGE),
        "list" => list(service, user),
        "add" => add(service, user, rest),
        "auth" => auth(service, user, rest),
        "cred" => cred(service, user, rest),
        other => CommandResult::info(format!("Unknown subcommand `{other}`.\n\n{ENDPOINT_USAGE}")),
    }
}

// ── subcommands ─────────────────────────────────────────────────────────────

/// Rendered under the caller's identity: the per-user vault tiers only resolve
/// inside a `CURRENT_USER_ID` scope, and a command runs outside the turn's.
/// Without it the listing reports a credential the caller has just set as still
/// missing, which is worse than not reporting at all.
fn list(service: &Arc<AgentService>, user: &str) -> CommandResult {
    let Some(store) = store_for(service) else {
        return CommandResult::info(VAULT_UNAVAILABLE);
    };
    match identity_id(user) {
        Some(uid) => {
            workforce_agent::runner::CURRENT_USER_ID.sync_scope(uid, || render_list(&store))
        }
        None => render_list(&store),
    }
}

fn render_list(store: &Arc<dyn SecretStore>) -> CommandResult {
    let endpoints = config::list_endpoints(store.as_ref());
    if endpoints.is_empty() {
        return CommandResult::info(format!(
            "No HTTP endpoints are configured yet.\n\n{ENDPOINT_USAGE}"
        ));
    }
    let lines: Vec<String> = endpoints
        .iter()
        .map(|c| {
            let gaps = c.missing_auth_fields(store.as_ref());
            let status = if gaps.is_empty() {
                "ready".to_string()
            } else {
                format!("MISSING {}", gaps.join(", "))
            };
            let creds = config::creds_keys(store.as_ref(), &c.name);
            let creds = if creds.is_empty() {
                String::new()
            } else {
                format!(" · creds: {}", creds.join(", "))
            };
            // `redact_userinfo`, not the raw `base_url`: `reqwest` sends a
            // `user:pass@` authority as basic auth, so it is a credential and
            // this listing is readable by anyone in the channel.
            format!(
                "• `{}` → {} · auth {} · {status}{creds}",
                c.name,
                config::redact_userinfo(&c.base_url),
                c.auth.kind()
            )
        })
        .collect();
    CommandResult::info(format!(
        "*HTTP endpoints* — your own credentials, values never shown\n{}",
        lines.join("\n")
    ))
}

fn add(service: &Arc<AgentService>, user: &str, rest: &str) -> CommandResult {
    let (name, base_url) = split_word(rest);
    if name.is_empty() || base_url.is_empty() {
        return CommandResult::info(format!(
            "Usage: `cmd: endpoint add <name> <base_url>`\n\n{ENDPOINT_USAGE}"
        ));
    }
    let Ok(name) = config::validate_name(name) else {
        return CommandResult::info(format!(
            "`{name}` is not a valid endpoint name — use letters, digits, `_` and `-`."
        ));
    };
    let base_url = match validate_base_url(base_url) {
        Ok(u) => u,
        Err(message) => return CommandResult::info(message),
    };
    write_shared(
        service,
        user,
        &format!("{ENDPOINTS_PREFIX}{name}/base_url"),
        base_url,
        &format!(
            "Endpoint `{name}` now points at {base_url}. Add a credential with \
             `cmd: endpoint cred {name} <key> <value>`, then the agent can call it with \
             `http.request(endpoint=\"{name}\", path=\"/…\")`."
        ),
    )
    .1
}

fn auth(service: &Arc<AgentService>, user: &str, rest: &str) -> CommandResult {
    let (name, tail) = split_word(rest);
    let (kind, header_name) = split_word(tail);
    let kind = kind.to_ascii_lowercase();
    if name.is_empty() || !AUTH_KINDS.contains(&kind.as_str()) {
        return CommandResult::info(format!(
            "Usage: `cmd: endpoint auth <name> <{}> [header-name]`\n\n{ENDPOINT_USAGE}",
            AUTH_KINDS.join("|")
        ));
    }
    let Ok(name) = config::validate_name(name) else {
        return CommandResult::info(format!("`{name}` is not a valid endpoint name."));
    };
    let (wrote, result) = write_shared(
        service,
        user,
        &format!("{ENDPOINTS_PREFIX}{name}/auth"),
        &kind,
        &format!("Endpoint `{name}` now authenticates with `{kind}`."),
    );
    // Keyed on the write's own outcome, not on how its message happens to be
    // worded — a reworded refusal must not silently start writing.
    if wrote && kind == "header" && !header_name.is_empty() {
        return write_shared(
            service,
            user,
            &format!("{ENDPOINTS_PREFIX}{name}/header_name"),
            header_name,
            &format!("Endpoint `{name}` now authenticates with `header` on `{header_name}`."),
        )
        .1;
    }
    result
}

fn cred(service: &Arc<AgentService>, user: &str, rest: &str) -> CommandResult {
    let (name, tail) = split_word(rest);
    let (key, value) = split_word(tail);
    // Same recovery `cmd: secret` applies: a Slack-linkified email or an
    // escaped `&` stored verbatim is a credential that fails with no visible
    // cause, because the value is never displayed back.
    let value = &crate::commands::credential_value(value);
    if name.is_empty() || key.is_empty() || value.is_empty() {
        return CommandResult::info(format!(
            "Usage: `cmd: endpoint cred <endpoint> <key> <value>`\n\n{ENDPOINT_USAGE}"
        ));
    }
    let Ok(name) = config::validate_name(name) else {
        return CommandResult::info(format!("`{name}` is not a valid endpoint name."));
    };
    if !is_safe_key(key) {
        return CommandResult::info(format!(
            "`{key}` is not a valid credential key — use letters, digits, `_`, `-` and `.`."
        ));
    }
    let Some(uid) = identity_id(user) else {
        return CommandResult::info(NO_IDENTITY);
    };
    let Some(store) = store_for(service) else {
        return CommandResult::info(VAULT_UNAVAILABLE);
    };
    write_own_cred(&store, &uid, name, key, value)
}

/// The per-user half, with the store already resolved. No admin gate by
/// design: the host is fixed by the time this is read, and the write lands in
/// the caller's own tier, so it widens nothing.
fn write_own_cred(
    store: &Arc<dyn SecretStore>,
    uid: &str,
    name: &str,
    key: &str,
    value: &str,
) -> CommandResult {
    if config::read_endpoint(store.as_ref(), name).is_none() {
        return CommandResult::info(format!(
            "No endpoint named `{name}` — an admin creates one with \
             `cmd: endpoint add {name} <base_url>`. Run `cmd: endpoint list` to see what \
             exists."
        ));
    }

    // An auth credential is an endpoint field; anything else is a value the
    // request body references as `[[vault:<key>]]`.
    let is_auth_field = AUTH_CREDENTIAL_FIELDS.contains(&key);
    let path = if is_auth_field {
        format!("{ENDPOINTS_PREFIX}{name}/{key}")
    } else {
        format!("{ENDPOINTS_PREFIX}{name}/creds/{key}")
    };

    // The per-user tier resolves under `CURRENT_USER_ID`, which a command runs
    // outside of — commands are dispatched before the turn's session scope.
    let written = workforce_agent::runner::CURRENT_USER_ID.sync_scope(uid.to_string(), || {
        store.set_at_tier(
            &path,
            value,
            VaultSource::AgentPerUser,
            VaultActor::User(uid),
        )
    });
    if let Err(e) = written {
        tracing::warn!(
            target: "sentinel.audit",
            endpoint = %name, key = %key, actor = %uid,
            error = %e,
            "endpoint credential write failed"
        );
        return CommandResult::info(format!("Could not store that credential: {e}"));
    }
    tracing::info!(
        target: "sentinel.audit",
        endpoint = %name,
        key = %key,
        actor = %uid,
        tier = "agent_user",
        len = value.len(),
        "endpoint credential stored via cmd: endpoint"
    );
    let reference = if is_auth_field {
        format!("It is now this endpoint's `{key}` — the agent never names it.")
    } else {
        format!("The agent references it as `[[vault:{key}]]`.")
    };
    CommandResult::info(format!(
        "Stored `{key}` for endpoint `{name}` (value hidden). {reference} It is stored \
         against your identity only — other users' calls to `{name}` use their own. \
         Delete the message you just sent so the value doesn't linger in this channel."
    ))
}

// ── helpers ─────────────────────────────────────────────────────────────────

const VAULT_UNAVAILABLE: &str = "The vault is not available on this hub, so HTTP endpoints \
                                 cannot be configured.";
const NO_IDENTITY: &str = "I can't tell which identity you are, so I can't store this \
                           against you. This usually means your chat account isn't linked \
                           to a Workforce identity yet.";

/// Shared-tier write, admin only. `success` is shown verbatim on success, so it
/// must never contain the value.
///
/// Returns whether the write actually happened alongside the message, so a
/// caller chaining a second write branches on the outcome rather than on how
/// the message reads.
fn write_shared(
    service: &Arc<AgentService>,
    user: &str,
    path: &str,
    value: &str,
    success: &str,
) -> (bool, CommandResult) {
    let Some(uid) = identity_id(user) else {
        return (false, CommandResult::info(NO_IDENTITY));
    };
    let admin = crate::authz::is_admin(service, &uid);
    let Some(store) = store_for(service) else {
        return (false, CommandResult::info(VAULT_UNAVAILABLE));
    };
    write_shared_at(&store, &uid, admin, path, value, success)
}

/// The gate and the write, with the decision already made — the security core
/// of the shared half, independent of how a service supplies it.
fn write_shared_at(
    store: &Arc<dyn SecretStore>,
    uid: &str,
    admin: bool,
    path: &str,
    value: &str,
    success: &str,
) -> (bool, CommandResult) {
    if !admin {
        tracing::warn!(
            target: "sentinel.audit",
            path = %path, actor = %uid,
            "non-admin attempted a shared endpoint write"
        );
        return (
            false,
            CommandResult::info(
                "Only an admin can point an endpoint at a host — whoever sets `base_url` \
                 decides where every credential under it is sent. Ask an admin to run it, or \
                 set your own credential on an existing endpoint with `cmd: endpoint cred …`.",
            ),
        );
    }
    match store.set_at_tier(
        path,
        value,
        VaultSource::AgentSpecific,
        VaultActor::User(uid),
    ) {
        Ok(()) => {
            tracing::info!(
                target: "sentinel.audit",
                path = %path, actor = %uid, tier = "agent",
                "endpoint configuration written via cmd: endpoint"
            );
            (true, CommandResult::info(success))
        }
        Err(e) => (
            false,
            CommandResult::info(format!("Could not write that: {e}")),
        ),
    }
}

/// The layered store the `http.*` tools read from, rebuilt from the service's
/// vaults so per-user overrides resolve exactly as they do at request time.
fn store_for(service: &Arc<AgentService>) -> Option<Arc<dyn SecretStore>> {
    let vault = service.vault.clone()?;
    Some(Arc::new(crate::user_config::LayeredUserVault::new(
        vault,
        service.global_vault.clone(),
    )))
}

/// `user` is the identity UUID on every surface that resolves one upstream; a
/// non-UUID means the account was never linked, and there is no per-user tier
/// (nor any role) to act under.
pub(crate) fn identity_id(user: &str) -> Option<String> {
    uuid::Uuid::parse_str(user).ok().map(|u| u.to_string())
}

fn is_safe_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// The URL as typed is not the URL to store. Slack linkifies a bare URL in a
/// message, so `https://api.example.com` reaches us as
/// `<https://api.example.com>` — or `<https://api.example.com|api.example.com>`
/// when it renders link text — and the leading `<` is not a valid scheme
/// character, so `Url::parse` reports "relative URL without a base". The user
/// sees their own correct URL rejected for a reason that says nothing about the
/// chat surface that altered it.
///
/// Returns the unwrapped form so the same string is validated, stored and
/// echoed back. Unwrapping only for the check would be worse than not
/// unwrapping at all: the `|label` form still parses once the brackets are off,
/// so an endpoint pointing at a host `http.request` can never reach would be
/// written to the shared tier under a success message.
fn validate_base_url(raw: &str) -> Result<&str, String> {
    let url = crate::commands::unwrap_link_token(raw.trim());
    match url::Url::parse(url) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => Ok(url),
        Ok(u) => Err(format!(
            "`{url}` uses scheme `{}` — an endpoint must be http or https.",
            u.scheme()
        )),
        Err(e) => Err(format!("`{url}` is not a valid URL: {e}")),
    }
}

/// Split off the first whitespace-delimited word, returning it and the trimmed
/// remainder. The remainder is deliberately *not* split further, so a credential
/// containing spaces survives.
pub(crate) fn split_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim()),
        None => (s, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_word_keeps_spaces_in_the_remainder() {
        assert_eq!(
            split_word("cred svc password  a b c"),
            ("cred", "svc password  a b c")
        );
        assert_eq!(split_word("  add  svc  "), ("add", "svc"));
        assert_eq!(split_word(""), ("", ""));
    }

    /// A password with spaces must survive intact — `cred` splits exactly twice.
    #[test]
    fn a_credential_value_may_contain_spaces() {
        let (_, tail) = split_word("svc password correct horse battery staple");
        let (key, value) = split_word(tail);
        assert_eq!(key, "password");
        assert_eq!(value, "correct horse battery staple");
    }

    /// Both bracket forms Slack produces reach the command verbatim, and both
    /// must store the URL the user actually typed. The `|label` form is the one
    /// that fails quietly: strip only the brackets and the remainder still
    /// parses, as a host with `|label` glued to it.
    #[test]
    fn a_slack_linkified_base_url_is_unwrapped_before_it_is_stored() {
        let typed = "https://api.staging.harbor.example";
        for delivered in [
            typed,
            "<https://api.staging.harbor.example>",
            "<https://api.staging.harbor.example|api.staging.harbor.example>",
        ] {
            assert_eq!(validate_base_url(delivered), Ok(typed), "for: {delivered}");
        }
    }

    #[test]
    fn a_base_url_must_be_an_http_url() {
        // What the bug reporter saw: a correct URL rejected as relative.
        assert!(validate_base_url("api.staging.harbor.example").is_err());
        assert!(validate_base_url("ftp://files.example.com").is_err());
        assert!(validate_base_url("").is_err());
    }

    #[test]
    fn safe_key_predicates() {
        for k in ["email", "api.key", "client-id", "a_b"] {
            assert!(is_safe_key(k), "should accept: {k}");
        }
        for k in ["", "with space", "a/b", "../x", &"x".repeat(65)] {
            assert!(!is_safe_key(k), "should reject: {k}");
        }
    }

    #[test]
    fn auth_credential_fields_route_to_the_endpoint_not_creds() {
        // Pins the one piece of routing a user could be surprised by.
        assert!(AUTH_CREDENTIAL_FIELDS.contains(&"password"));
        assert!(!AUTH_CREDENTIAL_FIELDS.contains(&"email"));
    }

    #[test]
    fn a_non_uuid_user_has_no_identity() {
        assert!(identity_id("U123SLACK").is_none());
        assert!(identity_id("8a3f0c2e-0000-4000-8000-000000000000").is_some());
    }

    // ── the split gate ──────────────────────────────────────────────────
    //
    // The premise of this command is that only an admin names a host while
    // anyone may supply their own credential. These exercise that directly,
    // over a real layered vault, rather than trusting the wiring.

    const ALICE: &str = "8a3f0c2e-0000-4000-8000-00000000a11c";
    const BOB: &str = "8a3f0c2e-0000-4000-8000-00000000b0b0";

    fn layered() -> Arc<dyn SecretStore> {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("vault.enc");
        // Leaked so the file outlives the test; the OS reaps it at exit.
        std::mem::forget(tmp);
        let vault = Arc::new(crate::vault::Vault::open(&path, "test-key").expect("vault"));
        Arc::new(crate::user_config::LayeredUserVault::new(vault, None))
    }

    fn add_endpoint(store: &Arc<dyn SecretStore>, name: &str) {
        let (wrote, _) = write_shared_at(
            store,
            ALICE,
            true,
            &format!("{ENDPOINTS_PREFIX}{name}/base_url"),
            "https://api.example.com",
            "ok",
        );
        assert!(wrote);
    }

    #[test]
    fn a_non_admin_cannot_point_an_endpoint_at_a_host() {
        let store = layered();
        let (wrote, result) = write_shared_at(
            &store,
            BOB,
            false,
            &format!("{ENDPOINTS_PREFIX}evil/base_url"),
            "https://evil.example.com",
            "ok",
        );
        assert!(!wrote, "a member must not write shared endpoint config");
        assert!(result.text.contains("Only an admin"), "{}", result.text);
        assert!(store
            .get(&format!("{ENDPOINTS_PREFIX}evil/base_url"))
            .is_none());
    }

    #[test]
    fn an_admin_can_point_an_endpoint_at_a_host() {
        let store = layered();
        add_endpoint(&store, "svc");
        assert_eq!(
            store
                .get(&format!("{ENDPOINTS_PREFIX}svc/base_url"))
                .as_deref(),
            Some("https://api.example.com")
        );
    }

    /// The whole point of the split: a member's credential lands in their own
    /// tier, so it never becomes shared config and never overwrites anyone.
    #[test]
    fn a_members_credential_lands_in_their_own_tier_only() {
        let store = layered();
        add_endpoint(&store, "svc");

        let result = write_own_cred(&store, BOB, "svc", "password", "bob-pw");
        assert!(result.text.contains("Stored"), "{}", result.text);
        assert!(
            !result.text.contains("bob-pw"),
            "value must never be echoed"
        );

        let path = format!("{ENDPOINTS_PREFIX}svc/password");
        // Nothing shared was written…
        assert!(store.get(&path).is_none(), "must not write the shared tier");
        // …and it resolves only for Bob.
        let as_bob = workforce_agent::runner::CURRENT_USER_ID
            .sync_scope(BOB.to_string(), || store.get_user_override(&path));
        let as_alice = workforce_agent::runner::CURRENT_USER_ID
            .sync_scope(ALICE.to_string(), || store.get_user_override(&path));
        assert_eq!(as_bob.as_deref(), Some("bob-pw"));
        assert_eq!(as_alice, None, "one member must not see another's");
    }

    /// A non-reserved key is a body value the agent names as `[[vault:<key>]]`,
    /// so it belongs under `creds/`, not beside the endpoint's own auth fields.
    #[test]
    fn a_non_reserved_key_lands_under_creds() {
        let store = layered();
        add_endpoint(&store, "svc");
        let result = write_own_cred(&store, BOB, "svc", "email", "a@b.com");
        assert!(result.text.contains("[[vault:email]]"), "{}", result.text);

        let creds = format!("{ENDPOINTS_PREFIX}svc/creds/email");
        let resolved = workforce_agent::runner::CURRENT_USER_ID
            .sync_scope(BOB.to_string(), || store.get_user_override(&creds));
        assert_eq!(resolved.as_deref(), Some("a@b.com"));
    }

    /// The per-user vault tiers only resolve inside a `CURRENT_USER_ID` scope,
    /// and a command runs outside the turn's. Unscoped, the listing reports a
    /// credential the caller has just set as still missing — worse than silence.
    #[test]
    fn the_listing_reports_a_members_own_credentials_only_when_scoped() {
        let store = layered();
        add_endpoint(&store, "svc");
        write_own_cred(&store, BOB, "svc", "email", "a@b.com");

        let unscoped = render_list(&store).text;
        let as_bob = workforce_agent::runner::CURRENT_USER_ID
            .sync_scope(BOB.to_string(), || render_list(&store))
            .text;

        assert!(!unscoped.contains("email"), "{unscoped}");
        assert!(as_bob.contains("creds: email"), "{as_bob}");
        assert!(!as_bob.contains("a@b.com"), "a value must never be listed");
    }

    /// The gap report is per-user for the same reason: Bob supplying his own
    /// basic-auth pair makes the endpoint ready *for Bob*, and still missing
    /// for someone who hasn't.
    #[test]
    fn the_missing_report_is_resolved_per_user() {
        let store = layered();
        add_endpoint(&store, "svc");
        let (wrote, _) = write_shared_at(
            &store,
            ALICE,
            true,
            &format!("{ENDPOINTS_PREFIX}svc/auth"),
            "basic",
            "ok",
        );
        assert!(wrote);
        write_own_cred(&store, BOB, "svc", "username", "u");
        write_own_cred(&store, BOB, "svc", "password", "p");

        let as_bob = workforce_agent::runner::CURRENT_USER_ID
            .sync_scope(BOB.to_string(), || render_list(&store))
            .text;
        let as_alice = workforce_agent::runner::CURRENT_USER_ID
            .sync_scope(ALICE.to_string(), || render_list(&store))
            .text;

        assert!(as_bob.contains("ready"), "{as_bob}");
        assert!(
            as_alice.contains("MISSING username, password"),
            "{as_alice}"
        );
    }

    /// Without this, a typo'd endpoint name silently stores a live credential
    /// at a path nothing will ever read.
    #[test]
    fn a_credential_for_an_unknown_endpoint_is_refused() {
        let store = layered();
        let result = write_own_cred(&store, BOB, "typo", "password", "pw");
        assert!(result.text.contains("No endpoint named"), "{}", result.text);
    }

    #[test]
    fn usage_names_both_privilege_levels_and_the_one_off_alternative() {
        assert!(ENDPOINT_USAGE.contains("Admins only"));
        assert!(ENDPOINT_USAGE.contains("cmd: endpoint cred"));
        assert!(ENDPOINT_USAGE.contains("cmd: secret"));
    }
}
