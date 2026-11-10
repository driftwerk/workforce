//! The models a plan's login runs, as the plan's own service lists them now,
//! so a model released after this hub was shows up without a new release:
//!
//! - a ChatGPT plan: the backend's catalogue, the list Codex reads
//!   (`GET chatgpt.com/backend-api/codex/models`), asked on the login's own
//!   tokens after the same refresh its calls take;
//! - a Claude plan: the Claude Code CLI's own list, from its SDK
//!   `initialize` reply ([`crate::claude_code::list_models`]). A Claude plan
//!   may only be used through Anthropic's own apps, so the hub asks the CLI,
//!   which knows the models it runs for that login, and never calls
//!   Anthropic with the login itself.
//!
//! Each list is kept for [`FRESH_FOR`] per login. A read that fails keeps the
//! last good list and isn't tried again for [`RETRY_AFTER`]; with no list,
//! the caller's built-in one stands.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use chrono::TimeDelta;
use eyre::{eyre, Result};
use workforce_agent::llm::responses::{CatalogModel, ChatGptAccess, ChatGptModel};
use workforce_common::subscription::SubscriptionKind;

use super::completion::Workdir;
use super::llm::CredentialScope;
use super::resolve::Login;
use super::{ChatGptBackend, SharedSubscriptions};

/// How long a list read is kept before it is read again.
pub const FRESH_FOR: Duration = Duration::from_secs(3 * 60 * 60);
/// How long a list that couldn't be read rests before it is tried again.
pub const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);
/// How long the Claude Code CLI gets to list its models.
const CLAUDE_LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// A login is refreshed first when its access token expires this soon.
const REFRESH_HORIZON: TimeDelta = TimeDelta::minutes(5);

/// One model a plan lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanModel {
    pub id: String,
    /// The model an alias runs now (`sonnet` → `claude-sonnet-5-5`).
    pub resolves_to: Option<String>,
    pub name: String,
    pub description: String,
    /// The effort levels the plan lists for it, lowest first; empty for a
    /// model that takes none.
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
    pub context_window: Option<u32>,
    /// How the plan words its faster tier ("2x speed, increased usage"),
    /// when the model has one.
    pub fast: Option<String>,
}

impl PlanModel {
    fn of_chatgpt(model: &CatalogModel) -> Self {
        Self {
            id: model.slug.clone(),
            resolves_to: None,
            name: model
                .display_name
                .clone()
                .unwrap_or_else(|| model.slug.clone()),
            description: model.description.clone().unwrap_or_default(),
            efforts: model
                .supported_reasoning_levels
                .iter()
                .map(|l| l.effort.clone())
                .collect(),
            default_effort: model.default_reasoning_level.clone(),
            context_window: model.context_window,
            fast: model.fast_tier().map(|tier| match tier.description.trim() {
                "" => "Faster replies".to_string(),
                description => description.to_string(),
            }),
        }
    }

    fn of_claude(model: crate::claude_code::CliModel) -> Self {
        Self {
            name: model.display_name.unwrap_or_else(|| model.value.clone()),
            description: model.description.unwrap_or_default(),
            efforts: model.supported_effort_levels,
            default_effort: None,
            context_window: None,
            fast: None,
            resolves_to: model.resolved_model.filter(|m| *m != model.value),
            id: model.value,
        }
    }
}

/// What a login's list is kept as.
struct Kept {
    models: Option<Arc<Vec<PlanModel>>>,
    read_at: Option<Instant>,
    tried_at: Instant,
}

/// The lists read so far, by plan and login.
#[derive(Default)]
pub struct PlanCatalog {
    lists: parking_lot::Mutex<HashMap<(SubscriptionKind, String), Kept>>,
}

impl PlanCatalog {
    /// The hub's one catalogue.
    pub fn global() -> &'static Self {
        static CATALOG: LazyLock<PlanCatalog> = LazyLock::new(PlanCatalog::default);
        &CATALOG
    }

    /// `kind`'s models for the login `login` names (its fingerprint, never
    /// its secret): the list kept when it is fresh or its last read failed
    /// too recently to try again, else what `read` lists now, else the
    /// last list kept.
    pub async fn models<F, Fut>(
        &self,
        kind: SubscriptionKind,
        login: &str,
        read: F,
    ) -> Option<Arc<Vec<PlanModel>>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<PlanModel>>>,
    {
        let key = (kind, login.to_string());
        if let Some(kept) = self.lists.lock().get(&key) {
            let fresh = kept.read_at.is_some_and(|at| at.elapsed() < FRESH_FOR);
            if fresh || kept.tried_at.elapsed() < RETRY_AFTER {
                return kept.models.clone();
            }
        }
        let result = read().await;
        let now = Instant::now();
        let mut lists = self.lists.lock();
        let kept = lists.entry(key).or_insert(Kept {
            models: None,
            read_at: None,
            tried_at: now,
        });
        kept.tried_at = now;
        match result {
            Ok(models) if !models.is_empty() => {
                kept.models = Some(Arc::new(models));
                kept.read_at = Some(now);
            }
            Ok(_) => tracing::warn!(
                plan = kind.label(),
                "the plan listed no models: keeping the list Desk had"
            ),
            Err(e) => tracing::warn!(
                plan = kind.label(),
                error = %format!("{e:#}"),
                "the plan's models couldn't be read: keeping the list Desk had"
            ),
        }
        kept.models.clone()
    }

    /// The list read most recently for any login of `kind`.
    pub fn last(&self, kind: SubscriptionKind) -> Option<Arc<Vec<PlanModel>>> {
        self.lists
            .lock()
            .iter()
            .filter(|((k, _), kept)| *k == kind && kept.models.is_some())
            .max_by_key(|(_, kept)| kept.read_at)
            .and_then(|(_, kept)| kept.models.clone())
    }
}

/// The models the ChatGPT backend lists for `login`, which the in-process
/// provider learns too ([`ChatGptModel::learn`]).
pub async fn read_chatgpt(backend: &ChatGptBackend, login: &Login) -> Result<Vec<PlanModel>> {
    let seen = login
        .auth()
        .ok_or_else(|| eyre!("the login is not a ChatGPT sign-in"))?;
    let auth = backend
        .refresher()
        .fresh(&login.slot, seen, REFRESH_HORIZON)
        .await
        .map_err(|e| eyre!("the ChatGPT sign-in couldn't be refreshed: {e}"))?;
    let (Some(access_token), Some(account_id)) = (auth.access_token(), auth.account_id()) else {
        return Err(eyre!(
            "the ChatGPT sign-in carries no access token or account"
        ));
    };
    let catalog = backend
        .client()
        .models(&ChatGptAccess {
            access_token,
            account_id: &account_id,
        })
        .await?;
    ChatGptModel::learn(&catalog);
    Ok(catalog
        .iter()
        .filter(|m| m.listed())
        .map(PlanModel::of_chatgpt)
        .collect())
}

/// The models the Claude Code CLI runs for `login`, asked of it in a
/// directory of its own under `root`.
pub async fn read_claude(login: &Login, root: &Path) -> Result<Vec<PlanModel>> {
    let workdir = Workdir::create(root, "List the models you offer.")?;
    let models = crate::claude_code::list_models(
        login.clone(),
        workdir.path(),
        &workdir.system_prompt(),
        CLAUDE_LIST_TIMEOUT,
    )
    .await?;
    Ok(models
        .into_iter()
        // Claude Code's own pick, which names the model it stands for.
        .filter(|m| m.value != "default")
        .map(PlanModel::of_claude)
        .collect())
}

/// The ChatGPT models `scope`'s login lists, while the hub calls the
/// backend at all (`[subscription] chatgpt_direct`).
pub async fn chatgpt_models(
    shared: &SharedSubscriptions,
    scope: &CredentialScope,
) -> Option<Arc<Vec<PlanModel>>> {
    if !shared.chatgpt().enabled() {
        return None;
    }
    let login = scope.resolve(SubscriptionKind::ChatGpt).ok()?;
    let key = login.key.fingerprint.clone();
    PlanCatalog::global()
        .models(SubscriptionKind::ChatGpt, &key, || {
            read_chatgpt(shared.chatgpt(), &login)
        })
        .await
}

/// The Claude models `scope`'s login runs, as the Claude Code CLI lists
/// them.
pub async fn claude_models(
    shared: &SharedSubscriptions,
    scope: &CredentialScope,
) -> Option<Arc<Vec<PlanModel>>> {
    let login = scope.resolve(SubscriptionKind::Claude).ok()?;
    let key = login.key.fingerprint.clone();
    PlanCatalog::global()
        .models(SubscriptionKind::Claude, &key, || {
            read_claude(&login, shared.completions_dir())
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn model(id: &str) -> PlanModel {
        PlanModel {
            id: id.into(),
            resolves_to: None,
            name: id.into(),
            description: String::new(),
            efforts: Vec::new(),
            default_effort: None,
            context_window: None,
            fast: None,
        }
    }

    #[tokio::test]
    async fn a_list_is_read_once_while_fresh_and_a_failed_read_keeps_the_last() {
        let catalog = PlanCatalog::default();
        let reads = AtomicUsize::new(0);
        let read = |result: Result<Vec<PlanModel>>| {
            reads.fetch_add(1, Ordering::SeqCst);
            async move { result }
        };
        let first = catalog
            .models(SubscriptionKind::ChatGpt, "acct:1", || {
                read(Ok(vec![model("gpt-6.2-nova")]))
            })
            .await
            .expect("read");
        assert_eq!(first[0].id, "gpt-6.2-nova");
        let again = catalog
            .models(SubscriptionKind::ChatGpt, "acct:1", || {
                read(Ok(vec![model("never-read")]))
            })
            .await
            .expect("kept");
        assert_eq!(again[0].id, "gpt-6.2-nova");
        assert_eq!(reads.load(Ordering::SeqCst), 1, "fresh: not read again");

        // Stale: the next read fails, and the last list stands.
        catalog.lists.lock().values_mut().for_each(|kept| {
            kept.read_at = Instant::now().checked_sub(FRESH_FOR * 2);
            kept.tried_at = Instant::now().checked_sub(RETRY_AFTER * 2).unwrap();
        });
        let after_failure = catalog
            .models(SubscriptionKind::ChatGpt, "acct:1", || {
                read(Err(eyre!("offline")))
            })
            .await
            .expect("the last list");
        assert_eq!(after_failure[0].id, "gpt-6.2-nova");
        assert_eq!(reads.load(Ordering::SeqCst), 2);

        // Another login has its own list; one never read has none.
        let none = catalog
            .models(SubscriptionKind::ChatGpt, "acct:2", || {
                read(Err(eyre!("refused")))
            })
            .await;
        assert!(none.is_none(), "the caller's built-in list stands");
        let resting = catalog
            .models(SubscriptionKind::ChatGpt, "acct:2", || {
                read(Ok(vec![model("too-soon")]))
            })
            .await;
        assert!(
            resting.is_none(),
            "a failed read rests before it is tried again"
        );
        assert_eq!(reads.load(Ordering::SeqCst), 3);
        assert_eq!(
            catalog.last(SubscriptionKind::ChatGpt).expect("last")[0].id,
            "gpt-6.2-nova"
        );
        assert!(catalog.last(SubscriptionKind::Claude).is_none());
    }

    #[test]
    fn a_chatgpt_entry_keeps_its_wording_and_its_fast_tier() {
        let body: serde_json::Value = serde_json::from_str(include_str!(
            "../../../agent-runtime/src/llm/testdata/chatgpt/models_live.json"
        ))
        .unwrap();
        let catalog = workforce_agent::llm::responses::parse_catalog(&body);
        let nova = PlanModel::of_chatgpt(
            catalog
                .iter()
                .find(|m| m.slug == "gpt-6.2-nova")
                .expect("nova"),
        );
        assert_eq!(nova.name, "GPT-6.2-Nova");
        assert_eq!(nova.efforts, ["low", "medium", "high"]);
        assert_eq!(nova.default_effort.as_deref(), Some("medium"));
        assert_eq!(nova.context_window, Some(400_000));
        assert_eq!(nova.fast.as_deref(), Some("1.5x speed"));
        let luna = PlanModel::of_chatgpt(
            catalog
                .iter()
                .find(|m| m.slug == "gpt-6-luna")
                .expect("luna"),
        );
        assert_eq!(luna.fast, None);
    }

    /// A `claude` that answers `initialize` with the CLI's list, then waits
    /// for a prompt that never comes.
    fn lister(dir: &Path, reply: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let program = dir.join("claude");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s' \"$*\" > \"{args}\"\nprintf '%s' \"$CLAUDE_CODE_OAUTH_TOKEN\" > \"{token}\"\nread line\ncase \"$line\" in *initialize*) ;; *) exit 3;; esac\nprintf '%s\\n' '{{\"type\":\"system\",\"subtype\":\"init\"}}'\nprintf '%s\\n' '{reply}'\nsleep 30\n",
                args = dir.join("args").display(),
                token = dir.join("token").display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    #[tokio::test]
    async fn the_claude_cli_is_asked_for_its_models_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let reply = serde_json::json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "wf-list-models",
                "response": { "models": [
                    { "value": "default", "resolvedModel": "claude-opus-5-5", "displayName": "Default (recommended)" },
                    { "value": "sonnet", "resolvedModel": "claude-sonnet-5-5", "displayName": "Sonnet 5.5",
                      "description": "Most efficient for simpler tasks",
                      "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"] },
                    { "value": "claude-fable-5-2", "resolvedModel": "claude-fable-5-2", "displayName": "Fable 5.2",
                      "supportedEffortLevels": ["low", "medium", "high", "max"] },
                    { "value": "claude-haiku-4-5-20251001", "displayName": "Haiku 4.5" }
                ] }
            }
        })
        .to_string();
        let program = lister(dir.path(), &reply);
        let root = dir.path().join("completions");
        let login = crate::subscription::test_support::claude_login("sk-ant-oat01-list");
        let started = Instant::now();
        let models = crate::claude_code::tests::CLAUDE
            .scope(program, read_claude(&login, &root))
            .await
            .expect("listed");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the CLI is stopped once it has listed"
        );
        let ids: Vec<(&str, Option<&str>, &str)> = models
            .iter()
            .map(|m| (m.id.as_str(), m.resolves_to.as_deref(), m.name.as_str()))
            .collect();
        assert_eq!(
            ids,
            [
                ("sonnet", Some("claude-sonnet-5-5"), "Sonnet 5.5"),
                ("claude-fable-5-2", None, "Fable 5.2"),
                ("claude-haiku-4-5-20251001", None, "Haiku 4.5"),
            ]
        );
        assert_eq!(models[0].efforts, ["low", "medium", "high", "xhigh", "max"]);
        assert!(models[2].efforts.is_empty());
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        assert!(args.contains("--input-format stream-json"), "{args}");
        assert!(args.contains("--tools "), "tool-less: {args}");
        assert!(!args.contains("--model"), "no model asked for: {args}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("token")).unwrap(),
            "sk-ant-oat01-list",
            "the login reaches the CLI as its token"
        );
        assert!(
            std::fs::read_dir(&root).unwrap().next().is_none(),
            "its directory is removed"
        );
    }

    #[tokio::test]
    async fn a_cli_that_refuses_or_says_nothing_lists_no_models() {
        let dir = tempfile::tempdir().unwrap();
        let refused = serde_json::json!({
            "type": "control_response",
            "response": { "subtype": "error", "request_id": "wf-list-models", "error": "not signed in" }
        })
        .to_string();
        let program = lister(dir.path(), &refused);
        let login = crate::subscription::test_support::claude_login("sk-ant-oat01-list");
        let err = crate::claude_code::tests::CLAUDE
            .scope(program, read_claude(&login, &dir.path().join("c")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not signed in"), "{err}");
        assert!(!err.to_string().contains("sk-ant"), "no secret in {err}");

        let silent = tempfile::tempdir().unwrap();
        let program = lister(silent.path(), r#"{"type":"assistant"}"#);
        let system = silent.path().join("system.md");
        let err = crate::claude_code::tests::CLAUDE
            .scope(
                program,
                crate::claude_code::list_models(
                    login,
                    silent.path(),
                    &system,
                    Duration::from_millis(500),
                ),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("didn't list"), "{err}");
    }
}
