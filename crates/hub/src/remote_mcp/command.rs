//! `cmd: remote-mcp …` — a human mints, lists and revokes their own remote MCP
//! tokens from any chat surface. Human-only like every `cmd:`; on top of that
//! the caller must resolve to an active identity, which is what stops a
//! workflow step, heartbeat or scheduler prompt carrying the text from minting
//! anything (those run as pseudo-users).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use workforce_identity::remote_mcp_token::{RemoteMcpToken, TOKEN_PREFIX};

use super::tokens::{is_expired, Minted, RemoteMcpTokenService, TokenError};
use crate::commands::CommandResult;
use crate::serve::AgentService;

const DEFAULT_LABEL: &str = "cmd";

const USAGE: &str = "Usage:\n\
  `cmd: remote-mcp token [label]` — mint a token (shown once)\n\
  `cmd: remote-mcp tokens` — list your tokens\n\
  `cmd: remote-mcp token revoke <id|label>` — revoke one";

pub(crate) async fn execute(
    service: &Arc<AgentService>,
    user: &str,
    args: Option<&str>,
) -> CommandResult {
    let identity_id =
        crate::endpoint_cmd::identity_id(user).and_then(|id| Uuid::parse_str(&id).ok());
    let tokens = RemoteMcpTokenService::for_agent(service);
    run(tokens.as_ref(), identity_id, args)
}

enum Subcommand<'a> {
    Mint(&'a str),
    List,
    Revoke(&'a str),
    Usage,
}

fn parse(args: Option<&str>) -> Subcommand<'_> {
    let args = args.unwrap_or("").trim();
    let (head, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(h, r)| (h, r.trim()));
    match (head.to_ascii_lowercase().as_str(), rest) {
        ("tokens", "") => Subcommand::List,
        ("token", "") => Subcommand::Mint(DEFAULT_LABEL),
        ("token", rest) => match rest.split_once(char::is_whitespace) {
            Some((verb, target)) if verb.eq_ignore_ascii_case("revoke") => {
                Subcommand::Revoke(target.trim())
            }
            _ if rest.eq_ignore_ascii_case("revoke") => Subcommand::Usage,
            _ => Subcommand::Mint(rest),
        },
        _ => Subcommand::Usage,
    }
}

/// Pure core over the resolved dependencies so the surfaces and the tests
/// share one path.
pub(crate) fn run(
    service: Option<&RemoteMcpTokenService>,
    identity_id: Option<Uuid>,
    args: Option<&str>,
) -> CommandResult {
    let Some(service) = service else {
        return CommandResult::info(
            "Remote MCP tokens are unavailable: no token store is bound on this agent.",
        );
    };
    let Some(identity_id) = identity_id else {
        return CommandResult::info(
            "`cmd: remote-mcp` needs a linked workforce identity — this account is not linked, or the caller is not a person.",
        );
    };
    let outcome = match parse(args) {
        Subcommand::Usage => return CommandResult::info(USAGE),
        Subcommand::Mint(label) => service.mint(identity_id, label).map(render_minted),
        Subcommand::List => service
            .list(identity_id)
            .map(|t| render_list(&t, Utc::now())),
        Subcommand::Revoke(target) => revoke(service, identity_id, target),
    };
    CommandResult::info(outcome.unwrap_or_else(|e| format!("❌ {e}")))
}

fn revoke(
    service: &RemoteMcpTokenService,
    identity_id: Uuid,
    target: &str,
) -> Result<String, TokenError> {
    let tokens = service.list(identity_id)?;
    let by_id = Uuid::parse_str(target).ok();
    let Some(token) = tokens
        .iter()
        .find(|t| by_id == Some(t.id) || t.label == target)
    else {
        return Ok(format!("No token matches `{target}`. {USAGE}"));
    };
    Ok(if service.revoke(identity_id, token.id)? {
        format!(
            "Revoked `{}` ({}{}…).",
            token.label, TOKEN_PREFIX, token.token_prefix
        )
    } else {
        format!("`{}` was already revoked.", token.label)
    })
}

fn render_minted(minted: Minted) -> String {
    let Minted {
        plaintext,
        record,
        connect,
    } = minted;
    let url_note = if connect.public_url_known {
        String::new()
    } else {
        "\n\n⚠️ This install has no `hub.public_url`; replace the placeholder URL in both commands."
            .to_string()
    };
    format!(
        "🔑 Remote MCP token `{label}` — expires {expires}\n\
         `{plaintext}`\n\
         Shown once; store it now.\n\n\
         Connect Claude Code:\n`{claude}`\n\n\
         Or register it in workforce-local:\n`{local}`{url_note}\n\n\
         Revoke any time with `cmd: remote-mcp token revoke {label}`.",
        label = record.label,
        expires = day(record.expires_at),
        claude = connect.claude_code,
        local = connect.workforce_local,
    )
}

fn render_list(tokens: &[RemoteMcpToken], now: DateTime<Utc>) -> String {
    if tokens.is_empty() {
        return "No remote MCP tokens. Mint one with `cmd: remote-mcp token [label]`.".to_string();
    }
    let lines = tokens.iter().map(|t| {
        let last_used = t.last_used_at.map_or("never".to_string(), day);
        let expired = if is_expired(t, now) { " (expired)" } else { "" };
        format!(
            "• `{}` — {TOKEN_PREFIX}{}… created {}, expires {}{expired}, last used {last_used}",
            t.label,
            t.token_prefix,
            day(t.created_at),
            day(t.expires_at),
        )
    });
    std::iter::once("🔑 Your remote MCP tokens:".to_string())
        .chain(lines)
        .collect::<Vec<_>>()
        .join("\n")
}

fn day(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_mcp::tokens::test_support::{enabled_config, identity, service};
    use workforce_common::settings::RemoteMcpConfig;
    use workforce_identity::store::IdentityStore as _;
    use workforce_identity::types::IdentityStatus;

    #[test]
    fn refuses_a_caller_without_an_identity() {
        let (svc, _) = service(enabled_config(), None);
        let out = run(Some(&svc), None, Some("token"));
        assert!(out.text.contains("linked workforce identity"));
        assert!(!out.text.contains(TOKEN_PREFIX));
    }

    #[test]
    fn refuses_when_no_store_is_bound() {
        let out = run(None, Some(Uuid::new_v4()), Some("token"));
        assert!(out.text.contains("no token store"));
    }

    #[test]
    fn refuses_a_suspended_identity_and_a_disabled_feature() {
        let (svc, store) = service(enabled_config(), None);
        let suspended = identity(IdentityStatus::Suspended);
        store.create(&suspended).unwrap();
        let out = run(Some(&svc), Some(suspended.id), Some("token"));
        assert!(out.text.contains("active identity"));

        let (disabled, store) = service(RemoteMcpConfig::default(), None);
        let me = identity(IdentityStatus::Active);
        store.create(&me).unwrap();
        let out = run(Some(&disabled), Some(me.id), Some("token laptop"));
        assert!(out.text.contains("not enabled"));
    }

    #[test]
    fn unknown_subcommands_show_usage() {
        let (svc, store) = service(enabled_config(), None);
        let me = identity(IdentityStatus::Active);
        store.create(&me).unwrap();
        for args in [
            None,
            Some(""),
            Some("help"),
            Some("bogus"),
            Some("token revoke"),
        ] {
            assert_eq!(run(Some(&svc), Some(me.id), args).text, USAGE, "{args:?}");
        }
    }

    #[test]
    fn mint_list_and_revoke_happy_path() {
        let (svc, store) = service(enabled_config(), Some("https://workforce.acme.dev"));
        let me = identity(IdentityStatus::Active);
        store.create(&me).unwrap();

        let minted = run(Some(&svc), Some(me.id), Some("token laptop")).text;
        assert!(minted.contains("`laptop`"));
        assert!(minted.contains("claude mcp add --transport http workforce-acme"));
        assert!(minted.contains("workforce-local-mcp config workforce add acme"));
        assert!(minted.contains("Shown once"));
        assert!(!minted.contains("hub.public_url"));
        let plaintext = minted
            .split('`')
            .find(|s| s.starts_with(TOKEN_PREFIX))
            .expect("token shown");
        assert_eq!(plaintext.len(), TOKEN_PREFIX.len() + 43);

        let default_label = run(Some(&svc), Some(me.id), Some("token")).text;
        assert!(default_label.contains("`cmd`"));

        let listed = run(Some(&svc), Some(me.id), Some("tokens")).text;
        assert!(listed.contains("`laptop`") && listed.contains("`cmd`"));
        assert!(listed.contains("last used never"));
        assert!(!listed.contains(plaintext));

        let revoked = run(Some(&svc), Some(me.id), Some("token revoke laptop")).text;
        assert!(revoked.starts_with("Revoked `laptop`"));
        let again = run(Some(&svc), Some(me.id), Some("token revoke laptop")).text;
        assert!(again.starts_with("No token matches"));

        let id = svc.list(me.id).unwrap()[0].id;
        let by_id = run(Some(&svc), Some(me.id), Some(&format!("token revoke {id}"))).text;
        assert!(by_id.starts_with("Revoked `cmd`"));
        assert!(run(Some(&svc), Some(me.id), Some("tokens"))
            .text
            .starts_with("No remote MCP tokens"));
    }

    #[test]
    fn mint_without_a_public_url_flags_the_placeholder() {
        let (svc, store) = service(enabled_config(), None);
        let me = identity(IdentityStatus::Active);
        store.create(&me).unwrap();
        let minted = run(Some(&svc), Some(me.id), Some("token")).text;
        assert!(minted.contains("hub.public_url"));
    }
}
