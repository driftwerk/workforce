use std::collections::BTreeSet;

use serde_json::json;
use workforce_agent::runner::CURRENT_USER_ID;
use workforce_agent::tools::Tool;
use workforce_common::identity::ACTION_DISPATCH_TOOLS;
use workforce_common::redact::contains_secret;
use workforce_common::settings::{AgentLlmConfig, KnowHowKind};

use super::*;
use crate::agent_turn::tests::{hub, Hub, ALICE};
use crate::credentials::{BRAVE_API_KEY, JEV_API_KEY};
use crate::effective_settings::tool::SettingsTool;
use crate::effective_settings::{report, Refusal, Scope};
use crate::vault::Vault;

const BOB: &str = "9d3b6a0e-7c41-4f0b-8e2a-1f5c2d7b8a90";
const AGENT: &str = "ada";

/// Install tables the report leaves out, by why. A new install table
/// belongs in one of these groups or in a section.
const NOT_REPORTED: &[(&str, &[&str])] = &[
    (
        "where the hub runs and keeps its data: addresses, stores and their credentials",
        &[
            "artifacts",
            "aws_infra",
            "cluster",
            "gateway",
            "hub",
            "mcp_store",
            "nats",
            "oauth_state",
            "pairing_store",
            "postgres",
            "revisions",
            "scheduler_store",
            // The store's backend; its digest knobs are `session_digest`.
            "sessions",
            "tracing",
            "vault",
            "vellev",
            "weaviate",
        ],
    ),
    (
        "who may reach which agent: other people and other agents, shown on the access pages",
        &[
            "agent_comms",
            "agents",
            "dm_routing",
            "group_overrides",
            "groups",
            "identity",
            "pairing",
            "permissions",
            "session_policies",
            "shared_channels",
            "teams",
        ],
    ),
    (
        "features that report their own state through a tool, a command or a page",
        &[
            "briefing",
            "claude_chat",
            "codegraph",
            "codex_chat",
            "coding_session",
            "curator",
            "desk",
            "environments",
            "exec",
            "games",
            "github",
            "heartbeat",
            "knowledge",
            "local_agents",
            "mcp",
            "monitor",
            "privacy",
            "projects",
            "remote_mcp",
            "sentinel",
            "session_lifecycle",
            "skills",
            "subscription",
            "training",
            "workflows",
        ],
    ),
    (
        "install-wide scalars no layer overrides",
        &["max_daily_notes_chars", "timezone"],
    ),
];

fn member(person: &str) -> Viewer {
    Viewer {
        person: Some(person.to_string()),
        admin: false,
    }
}

fn admin() -> Viewer {
    Viewer {
        admin: true,
        ..member(ALICE)
    }
}

fn session(hub: &Hub) -> Session {
    Session::new("test", ALICE, hub.service.agent_id.as_str())
}

/// The agent's own `config.toml`, loaded as an edit to it is.
async fn configured(hub: &Hub, config_toml: &str) {
    std::fs::write(hub.service.workspace.join("config.toml"), config_toml).unwrap();
    hub.service.reload_config().await.unwrap();
}

async fn all(hub: &Hub, viewer: &Viewer) -> Vec<Row> {
    report(&hub.service, viewer, &session(hub), &Scope::All, true)
        .await
        .unwrap()
        .rows
}

fn row<'a>(rows: &'a [Row], path: &str) -> &'a Row {
    rows.iter()
        .find(|row| row.path() == path)
        .unwrap_or_else(|| panic!("no row {path}"))
}

fn reason<'a>(rows: &'a [Row], path: &str) -> &'a str {
    row(rows, path).reason.as_deref().unwrap_or_default()
}

fn layers(row: &Row) -> Vec<Source> {
    row.chain.iter().map(|layer| layer.source).collect()
}

fn vault(dir: &std::path::Path, name: &str) -> Arc<Vault> {
    Arc::new(Vault::open(&dir.join(name), "test-key").unwrap())
}

#[tokio::test]
async fn each_value_names_the_layer_it_comes_from() {
    let hub = hub(|config| {
        config.know_how.mode = ProgramsMode::On;
        config.know_how.max_tokens = 600;
        config.agents.push(AgentConfig {
            id: AGENT.into(),
            default: true,
            llm: Some(AgentLlmConfig {
                max_rounds: Some(77),
                ..Default::default()
            }),
            ..Default::default()
        });
    })
    .await;
    configured(&hub, "[know_how]\nmax_entries = 3\n").await;
    *hub.service.active_max_tokens.write() = Some(1234);

    let rows = all(&hub, &member(ALICE)).await;
    assert_eq!(
        row(&rows, "know_how.min_score").source,
        Some(Source::Default)
    );
    let install = row(&rows, "know_how.max_tokens");
    assert_eq!(
        (install.source, &install.value),
        (Some(Source::Install), &json!(600))
    );
    assert_eq!(layers(install), [Source::Default, Source::Install]);
    let entry = row(&rows, "llm.max_rounds");
    assert_eq!(
        (entry.source, &entry.value),
        (Some(Source::InstallAgent), &json!(77))
    );
    let own = row(&rows, "know_how.max_entries");
    assert_eq!((own.source, &own.value), (Some(Source::Agent), &json!(3)));
    assert_eq!(layers(own), [Source::Default, Source::Agent]);
    let live = row(&rows, "llm.max_tokens");
    assert_eq!(
        (live.source, &live.value),
        (Some(Source::Runtime), &json!(1234))
    );
    assert_eq!(layers(live).last(), Some(&Source::Runtime));

    let picked = row(&rows, "llm.model");
    assert_eq!(picked.source, None, "{picked:?}");
    assert!(picked.reason.as_deref().unwrap().contains("config default"));
}

#[tokio::test]
async fn a_session_override_and_a_playbook_step_are_runtime_sources_of_the_model() {
    let hub = hub(|_| {}).await;
    let model = |session: Session| {
        let service = Arc::clone(&hub.service);
        async move {
            let rows = report(&service, &member(ALICE), &session, &Scope::All, false)
                .await
                .unwrap()
                .rows;
            row(&rows, "llm.model").clone()
        }
    };

    let mut overridden = session(&hub);
    overridden.model_override = Some("gpt-5.5-mini".into());
    let picked = model(overridden.clone()).await;
    assert_eq!(
        (picked.source, &picked.value),
        (Some(Source::Runtime), &json!("gpt-5.5-mini"))
    );
    assert!(picked
        .reason
        .as_deref()
        .unwrap()
        .contains("session override"));

    overridden.set_playbook(
        &serde_json::from_value(json!({
            "id": "review",
            "nonce": "n",
            "artifacts": [],
            "step_model": "gpt-5.5-pro",
        }))
        .unwrap(),
    );
    let picked = model(overridden).await;
    assert_eq!(
        (picked.source, &picked.value),
        (Some(Source::Runtime), &json!("gpt-5.5-pro"))
    );
    assert!(picked.reason.as_deref().unwrap().contains("playbook step"));
}

#[test]
fn a_budget_override_is_a_runtime_source_of_the_ceiling_it_moves() {
    use crate::budget_overrides::{BudgetOverride, BudgetOverrideStore, OverrideScope};
    let overrides =
        Arc::new(crate::budget_overrides::sqlite::SqliteBudgetOverrideStore::in_memory().unwrap());
    overrides
        .upsert(&BudgetOverride {
            scope: OverrideScope::Agent,
            key: AGENT.into(),
            daily_usd: 750.0,
            expires_at: None,
            note: None,
            created_by: "admin".into(),
            updated_at: chrono::Utc::now(),
        })
        .unwrap();
    let config = BudgetConfig {
        identity_daily_usd: 20.0,
        ..BudgetConfig::default()
    };
    let governor = crate::cost_governor::CostGovernor::with_override_store(
        Arc::new(workforce_common::persistence::SqliteStore::in_memory().unwrap()),
        config,
        chrono_tz::UTC,
        Some(overrides),
    );

    let rows = budget_rows(&governor, AGENT, Some(ALICE));
    let agent = row(&rows, "budget.agent.daily_usd");
    assert_eq!(
        (agent.source, &agent.value),
        (Some(Source::Runtime), &json!(750.0))
    );
    assert_eq!(layers(agent), [Source::Default, Source::Runtime]);
    let mine = row(&rows, "budget.you.daily_usd");
    assert_eq!(
        (mine.source, &mine.value),
        (Some(Source::Install), &json!(20.0))
    );
    assert_eq!(
        row(&rows, "budget.agent.job_max_runs").source,
        Some(Source::Default)
    );
    assert!(row(&rows, "budget.hub_daily_usd").install_wide);

    assert!(
        budget_rows(&governor, AGENT, None)
            .iter()
            .all(|row| !row.key.starts_with("you.")),
        "nobody's ceiling is shown to nobody"
    );
}

#[tokio::test]
async fn a_switched_off_table_says_who_switched_it_off_and_what_is_left_inert() {
    let hub = hub(|config| {
        config.know_how.mode = ProgramsMode::Off;
        config.research.allow_adversarial = false;
    })
    .await;
    let rows = all(&hub, &member(ALICE)).await;
    let state = row(&rows, "know_how.state");
    assert_eq!(
        (state.status, &state.value),
        (Some(Status::KillSwitched), &json!("off"))
    );
    assert_eq!(
        reason(&rows, "know_how.state"),
        "the install has know-how switched off"
    );
    assert_eq!(row(&rows, "know_how.mode").value, json!("off"));
    assert_eq!(row(&rows, "tools.programs.state").status, Some(Status::Off));
    assert_eq!(
        reason(&rows, "tools.programs.state"),
        "programs is opt-in and the agent's config.toml does not enable it"
    );
    assert_eq!(
        row(&rows, "attachments.video.state").status,
        Some(Status::Active)
    );
    let detail = row(&rows, "attachments.video.detail_long_edge");
    assert_eq!(
        (detail.source, &detail.value),
        (Some(Source::Default), &json!(1568))
    );
    assert_eq!(
        row(&rows, "research.allow_adversarial").status,
        Some(Status::KillSwitched)
    );

    configured(
        &hub,
        "[know_how]\nenabled = true\nmax_entries = 3\n\n[research]\nadversarial = true\n\n\
         [attachments.video]\nenabled = false\n",
    )
    .await;
    let rows = all(&hub, &member(ALICE)).await;
    assert_eq!(row(&rows, "know_how.state").status, Some(Status::Inert));
    assert_eq!(
        reason(&rows, "know_how.state"),
        "the agent's config.toml enables know-how, but the install has it switched off"
    );
    let unused = row(&rows, "know_how.max_entries");
    assert_eq!(
        (unused.source, unused.status),
        (Some(Source::Agent), Some(Status::Inert))
    );
    assert!(reason(&rows, "know_how.max_entries").starts_with("not in effect: "));
    assert_eq!(
        row(&rows, "know_how.enabled").status,
        None,
        "the switch itself is not unused"
    );
    assert_eq!(
        row(&rows, "research.adversarial").status,
        Some(Status::Inert)
    );
    assert!(reason(&rows, "research.adversarial").contains("the install forbids it"));
    assert_eq!(
        row(&rows, "attachments.video.state").status,
        Some(Status::Off)
    );
    assert_eq!(
        reason(&rows, "attachments.video.state"),
        "the agent's config.toml turns video frames off"
    );
}

#[tokio::test]
async fn the_know_how_kinds_an_agent_matches_are_reported() {
    let hub = hub(|config| config.know_how.mode = ProgramsMode::On).await;
    let rows = all(&hub, &member(ALICE)).await;
    let every = row(&rows, "know_how.kinds");
    assert_eq!(
        (every.source, &every.value),
        (
            Some(Source::Default),
            &json!([
                "data_location",
                "query_shape",
                "schema_correction",
                "code_location"
            ])
        )
    );
    configured(
        &hub,
        "[know_how]\nkinds = [\"query_shape\", \"schema_correction\"]\n",
    )
    .await;
    let rows = all(&hub, &member(ALICE)).await;
    let own = row(&rows, "know_how.kinds");
    assert_eq!(
        (own.source, own.status, &own.value),
        (
            Some(Source::Agent),
            None,
            &json!(["query_shape", "schema_correction"])
        )
    );
    configured(
        &hub,
        "[know_how]\nkinds = [\"schema_correction\", \"query_shape\"]\n",
    )
    .await;
    let rows = all(&hub, &member(ALICE)).await;
    let reordered = row(&rows, "know_how.kinds");
    assert_eq!(
        (reordered.source, reordered.status, &reordered.value),
        (
            Some(Source::Agent),
            None,
            &json!(["schema_correction", "query_shape"])
        ),
        "in the agent's own order"
    );

    let data_only = crate::agent_turn::tests::hub(|config| {
        config.know_how.mode = ProgramsMode::On;
        config.know_how.kinds = vec![KnowHowKind::DataLocation, KnowHowKind::QueryShape];
    })
    .await;
    configured(
        &data_only,
        "[know_how]\nkinds = [\"code_location\", \"query_shape\"]\n",
    )
    .await;
    assert_eq!(
        data_only.service.know_how().unwrap().kinds,
        [KnowHowKind::QueryShape]
    );
    let rows = all(&data_only, &member(ALICE)).await;
    let narrowed = row(&rows, "know_how.kinds");
    assert_eq!(
        (narrowed.source, &narrowed.value),
        (Some(Source::Agent), &json!(["query_shape"])),
        "the install's kinds bound the agent's"
    );
    assert_eq!(
        reason(&rows, "know_how.kinds"),
        "resolved from [\"code_location\",\"query_shape\"]"
    );
}

#[tokio::test]
async fn shadow_is_reported_and_a_bounded_override_is_inert() {
    let hub = hub(|config| config.know_how.mode = ProgramsMode::On).await;
    configured(
        &hub,
        "[know_how]\nmin_users = 1\n\n[attachments.video]\ndetail_long_edge = 600\n",
    )
    .await;
    let rows = all(&hub, &member(ALICE)).await;
    let detail = row(&rows, "attachments.video.detail_long_edge");
    assert_eq!(
        (detail.status, &detail.value, detail.source),
        (Some(Status::Inert), &json!(1024), Some(Source::Agent)),
        "a closer look is never coarser than a sampled frame"
    );
    assert_eq!(
        reason(&rows, "attachments.video.detail_long_edge"),
        "the agent's config.toml sets 600, which is out of bounds: 1024 is in effect"
    );
    let state = row(&rows, "know_how.state");
    assert_eq!(
        (state.status, &state.value),
        (Some(Status::Shadow), &json!("shadow"))
    );
    assert!(reason(&rows, "know_how.state").contains("not shown to the model"));
    let bounded = row(&rows, "know_how.min_users");
    assert_eq!(
        (bounded.status, &bounded.value, bounded.source),
        (Some(Status::Inert), &json!(2), Some(Source::Default))
    );
    assert_eq!(
        reason(&rows, "know_how.min_users"),
        "the agent's config.toml sets 1, which is not in effect: the built-in default value holds"
    );
    let summary = report(
        &hub.service,
        &member(ALICE),
        &session(&hub),
        &Scope::Summary,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        row(&summary.rows, "know_how.min_users").status,
        Some(Status::Inert),
        "a summary keeps a setting that is not in effect, whichever layer holds"
    );

    configured(&hub, "[know_how]\ninject = \"on\"\n").await;
    let rows = all(&hub, &member(ALICE)).await;
    assert_eq!(row(&rows, "know_how.state").status, Some(Status::Active));
    assert_eq!(row(&rows, "know_how.inject").source, Some(Source::Agent));
}

#[tokio::test]
async fn source_agent_on_an_agent_that_is_no_copy_is_reported_inert() {
    let hub = hub(|config| config.know_how.mode = ProgramsMode::On).await;
    let source = "[know_how]\nsource_agent = \"analyst\"\n";
    configured(&hub, source).await;
    let rows = all(&hub, &member(ALICE)).await;
    let ignored = row(&rows, "know_how.source_agent");
    assert_eq!(
        (ignored.status, &ignored.value, ignored.source),
        (Some(Status::Inert), &json!("analyst"), Some(Source::Agent))
    );
    assert!(reason(&rows, "know_how.source_agent").contains("only a test copy"));

    configured(&hub, &format!("[evals]\ncopy = true\n{source}")).await;
    let rows = all(&hub, &member(ALICE)).await;
    let read = row(&rows, "know_how.source_agent");
    assert_eq!(
        (read.status, &read.value, read.source),
        (None, &json!("analyst"), Some(Source::Agent))
    );
}

#[tokio::test]
async fn a_misconfigured_describer_is_reported_with_its_reason() {
    let hub = hub(|config| {
        config.llm.vision = Some(toml::from_str("model = \"nonesuch:gpt-5.5\"").unwrap());
    })
    .await;
    let rows = all(&hub, &member(ALICE)).await;
    let state = row(&rows, "llm.vision.state");
    assert_eq!(
        (state.status, &state.value),
        (Some(Status::Inert), &json!("misconfigured"))
    );
    let why = reason(&rows, "llm.vision.state");
    assert!(
        why.starts_with("vision is misconfigured: ") && why.contains("names no known provider"),
        "{why}"
    );
    let configured = row(&rows, "llm.vision.model");
    assert_eq!(
        (configured.source, &configured.value),
        (Some(Source::Install), &json!("nonesuch:gpt-5.5"))
    );
    assert_eq!(
        row(&rows, "llm.transcription.state").status,
        Some(Status::Off)
    );
    assert!(rows.iter().all(|row| row.key != "vision.pending"));
}

#[tokio::test]
async fn an_llm_edit_the_running_agent_cannot_apply_waits_for_a_restart() {
    let hub = hub(|_| {}).await;
    let rows = all(&hub, &member(ALICE)).await;
    assert_eq!(row(&rows, "llm.vision.state").value, json!("unconfigured"));
    let waiting = |rows: &[Row]| {
        rows.iter()
            .filter(|row| row.section == "llm" && row.status == Some(Status::Inert))
            .map(|row| row.key.clone())
            .collect::<Vec<_>>()
    };
    assert!(
        waiting(&rows).is_empty(),
        "a fresh agent waits for nothing: {:?}",
        waiting(&rows)
    );
    let started_with = row(&rows, "llm.max_tokens").value.clone();

    configured(
        &hub,
        "[llm]\nmax_tokens = 9999\n\n[llm.retry]\nmax_retries = 7\n\n\
         [llm.vision]\nmodel = \"openrouter:anthropic:cheap\"\n",
    )
    .await;
    let rows = all(&hub, &member(ALICE)).await;
    let edited = row(&rows, "llm.max_tokens");
    assert_eq!(
        (edited.status, &edited.value, edited.source),
        (Some(Status::Inert), &started_with, Some(Source::Default))
    );
    assert_eq!(
        reason(&rows, "llm.max_tokens"),
        "the config now gives 9999; the running agent keeps this value until it restarts"
    );
    let hot = row(&rows, "llm.retry.max_retries");
    assert_eq!(
        (hot.status, &hot.value, hot.source),
        (None, &json!(7), Some(Source::Agent)),
        "a reload applies retry to the running agent"
    );

    assert_eq!(row(&rows, "llm.vision.state").value, json!("unconfigured"));
    let model = row(&rows, "llm.vision.model");
    assert_eq!(
        (model.status, &model.value, model.source),
        (
            Some(Status::Inert),
            &json!("openrouter:anthropic:cheap"),
            Some(Source::Agent)
        )
    );
    assert_eq!(
        reason(&rows, "llm.vision.model"),
        "set since the agent started; it applies after a restart"
    );
    let pending = row(&rows, "llm.vision.pending");
    assert_eq!(pending.status, Some(Status::Inert));
    assert!(reason(&rows, "llm.vision.pending").contains("applies after a restart"));
}

#[tokio::test]
async fn a_credential_shows_the_level_that_holds_it_for_the_person_asking() {
    let hub = hub(|_| {}).await;
    let (agent, global) = (
        hub.service.vault.clone().unwrap(),
        hub.service.global_vault.clone().unwrap(),
    );
    let path = format!("credentials.{}", JEV_API_KEY.path);
    let held = |rows: &[Row]| {
        let row = row(rows, &path);
        (row.value.clone(), row.source, row.reason.clone().unwrap())
    };

    let (value, source, why) = held(&all(&hub, &member(ALICE)).await);
    assert_eq!((value, source), (json!("not set"), None));
    assert!(why.contains("not set at any level") && why.contains("Do not ask for the key"));

    for (vault, key, level) in [
        (&global, JEV_API_KEY.path.to_string(), "global level"),
        (&agent, JEV_API_KEY.path.to_string(), "agent level"),
        (
            &agent,
            format!("users/{ALICE}/{}", JEV_API_KEY.path),
            "your own key for this agent",
        ),
        (
            &global,
            format!("users/{ALICE}/{}", JEV_API_KEY.path),
            "your own key",
        ),
    ] {
        vault.set(&key, &format!("jev-secret-{level}")).unwrap();
        let (value, source, why) = held(&all(&hub, &member(ALICE)).await);
        assert_eq!(
            (value, source),
            (json!(SET), Some(Source::Vault)),
            "{level}"
        );
        assert!(why.ends_with(&format!("set ({level})")), "{why}");
    }
    let (_, _, why) = held(&all(&hub, &member(BOB)).await);
    assert!(
        why.ends_with("set (agent level)"),
        "another person's key: {why}"
    );
}

#[test]
fn a_credential_with_no_vault_key_says_what_its_reader_falls_back_on() {
    let env = Some(LastResort::Environment("BRAVE_API_KEY"));
    let from_env = credential_row(&BRAVE_API_KEY, None, env);
    assert_eq!(
        (from_env.value.clone(), from_env.source),
        (json!(SET), Some(Source::Environment))
    );
    assert!(from_env
        .reason
        .unwrap()
        .contains("the hub's BRAVE_API_KEY is used"));
    assert_eq!(
        credential_row(&BRAVE_API_KEY, Some(VaultSource::Global), env).source,
        Some(Source::Vault),
        "a vault key wins over the environment"
    );
    assert_eq!(
        credential_row(&BRAVE_API_KEY, None, None).value,
        json!("not set")
    );
    let in_config = credential_row(
        &crate::credentials::MEMORY_EMBEDDINGS_API_KEY,
        None,
        Some(LastResort::Config("[memory] embedding_api_key")),
    );
    assert_eq!(in_config.source, Some(Source::Install));
    assert!(in_config
        .reason
        .unwrap()
        .contains("the install config's [memory] embedding_api_key is used"));
    let account = credential_row(&BRAVE_API_KEY, None, Some(LastResort::Account));
    assert_eq!(account.source, Some(Source::Vault));
    assert!(account
        .reason
        .unwrap()
        .contains("the provider's LLM account in the vault is used"));
}

#[tokio::test]
async fn a_modality_shows_the_key_of_the_provider_it_is_routed_to() {
    let hub = hub(|config| {
        config.llm.transcription = Some(toml::from_str(r#"model = "groq:cheap""#).unwrap());
        config.llm.speech = Some(toml::from_str(r#"provider = "fish_audio""#).unwrap());
    })
    .await;
    let global = hub.service.global_vault.clone().unwrap();
    let groq = "credentials.transcription/groq/api_key";
    let fish = "credentials.speech/fish_audio/api_key";
    global
        .set(
            &format!("users/{ALICE}/transcription/groq/api_key"),
            "gsk-alice-secret",
        )
        .unwrap();
    global
        .set("speech/fish_audio/api_key", "fish-shared-secret")
        .unwrap();

    let rows = all(&hub, &member(ALICE)).await;
    assert_eq!(row(&rows, groq).source, Some(Source::Vault));
    assert!(reason(&rows, groq).ends_with("set (your own key)"));
    assert!(reason(&rows, fish).ends_with("set (global level)"));
    assert!(
        rows.iter().all(|row| ![
            "credentials.transcription/openai/api_key",
            "credentials.speech/elevenlabs/api_key",
            "credentials.vision/openrouter/api_key",
            "credentials.image/gemini/api_key",
        ]
        .contains(&row.path().as_str())),
        "a provider the agent does not route to has no row"
    );
    let shown = serde_json::to_string(&rows).unwrap();
    assert!(
        !shown.contains("gsk-") && !shown.contains("fish-shared"),
        "{shown}"
    );

    global
        .set(
            "llm/accounts/groq",
            r#"{"provider":"groq","api_key":"gsk-account-secret"}"#,
        )
        .unwrap();
    let rows = all(&hub, &member(BOB)).await;
    assert!(
        row(&rows, groq).source.is_some(),
        "with no key of his own, the install's still serves him"
    );
    assert!(!serde_json::to_string(&rows).unwrap().contains("gsk-"));
}

#[tokio::test]
async fn decide_first_is_reported_as_far_as_the_install_lets_an_agent_take_it() {
    let mode = |rows: &[Row]| {
        let row = row(rows, "decision.uses.decide_first.mode");
        (row.value.clone(), row.source, row.status)
    };
    let stock = hub(|_| {}).await;
    assert_eq!(
        mode(&all(&stock, &member(ALICE)).await),
        (json!("off"), Some(Source::Default), None)
    );
    configured(&stock, "[decision.uses.decide_first]\nmode = \"on\"\n").await;
    let rows = all(&stock, &member(ALICE)).await;
    assert_eq!(
        mode(&rows),
        (
            json!("off"),
            Some(Source::Default),
            Some(Status::KillSwitched)
        ),
        "an agent cannot take it past the install"
    );
    assert_eq!(
        reason(&rows, "decision.uses.decide_first.mode"),
        "the install asks no workflow step's outcome before its turn"
    );

    let allowed = hub(|config| config.decision.uses.decide_first.mode = DecisionUse::On).await;
    configured(
        &allowed,
        "[decision.uses.decide_first]\nmode = \"shadow\"\n",
    )
    .await;
    let rows = all(&allowed, &member(ALICE)).await;
    let (value, source, _) = mode(&rows);
    assert_eq!((value, source), (json!("shadow"), Some(Source::Agent)));
    assert_eq!(
        layers(row(&rows, "decision.uses.decide_first.mode")),
        [Source::Default, Source::Install, Source::Agent]
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.path() == "decision.uses.decide_first.mode")
            .count(),
        1,
        "one row, the agent-layered one, and not the install-wide copy"
    );
}

#[test]
fn a_decide_first_row_says_how_far_the_install_takes_it() {
    let at = |value: DecisionUse, source| Row {
        source: Some(source),
        ..Row::live("decision", "uses.decide_first.mode", json!(value))
    };
    let status = |row: Row, allowed| decide_first_row(row, allowed).status;
    assert_eq!(status(at(DecisionUse::Off, Source::Default), None), None);
    let tried = Row {
        status: Some(Status::Inert),
        ..at(DecisionUse::Off, Source::Default)
    };
    assert_eq!(status(tried, None), Some(Status::KillSwitched));
    for allowed in [None, Some(DecisionUse::Off)] {
        assert_eq!(
            status(at(DecisionUse::On, Source::Agent), allowed),
            Some(Status::KillSwitched)
        );
    }
    let on = Some(DecisionUse::On);
    assert_eq!(
        status(at(DecisionUse::Shadow, Source::Agent), on),
        Some(Status::Shadow)
    );
    assert_eq!(
        status(at(DecisionUse::On, Source::Install), on),
        Some(Status::Active)
    );
    assert_eq!(
        status(at(DecisionUse::Off, Source::Agent), on),
        Some(Status::Off)
    );
}

#[test]
fn decision_backends_are_available_to_a_caller_by_the_key_they_can_use() {
    let dir = tempfile::tempdir().unwrap();
    let (agent, global) = (vault(dir.path(), "a.enc"), vault(dir.path(), "g.enc"));
    let caller = |person: &str| {
        Caller::new(
            Some(person.to_string()),
            Some(Arc::clone(&agent)),
            Some(Arc::clone(&global)),
        )
    };
    let config: DecisionConfig = toml::from_str(
        "enabled = true\nprimary = \"jev\"\nfallback = \"kev\"\n\
         uses = { scope_gate = { mode = \"shadow\" }, decide_first = { mode = \"on\" } }\n\
         [[backends]]\nname = \"jev\"\nurl = \"https://jev.example\"\n\
         [[backends]]\nname = \"kev\"\nurl = \"https://kev.example\"\napi_key = \"kev-config-key\"\n",
    )
    .unwrap();
    let decisions = crate::decide::build(&config).unwrap().unwrap();

    let rows = decision_rows(Some(&decisions), &caller(ALICE));
    assert_eq!(row(&rows, "decision.state").status, Some(Status::Active));
    let jev = row(&rows, "decision.backends.jev");
    assert_eq!(
        (jev.status, &jev.value),
        (Some(Status::Inert), &json!("unavailable"))
    );
    assert!(
        reason(&rows, "decision.backends.jev").starts_with("primary; key: not set at any level")
    );
    let kev = row(&rows, "decision.backends.kev");
    assert_eq!(
        (kev.source, &kev.value),
        (Some(Source::Install), &json!("available"))
    );
    assert_eq!(
        row(&rows, "decision.uses.scope_gate").status,
        Some(Status::Shadow)
    );
    assert_eq!(
        row(&rows, "decision.uses.routing").status,
        Some(Status::Off)
    );
    assert_eq!(
        row(&rows, "decision.uses.decide_first").status,
        Some(Status::Active)
    );

    global
        .set(
            &format!("users/{ALICE}/{}", JEV_API_KEY.path),
            "alice-jev-key",
        )
        .unwrap();
    let rows = decision_rows(Some(&decisions), &caller(ALICE));
    assert_eq!(
        row(&rows, "decision.backends.jev").source,
        Some(Source::Vault)
    );
    assert_eq!(
        reason(&rows, "decision.backends.jev"),
        "primary; key: set (your own key)"
    );
    let rows = decision_rows(Some(&decisions), &caller(BOB));
    assert_eq!(
        row(&rows, "decision.backends.jev").value,
        json!("unavailable")
    );
    assert!(!serde_json::to_string(&rows)
        .unwrap()
        .contains("kev-config-key"));

    let off = decision_rows(None, &caller(ALICE));
    assert_eq!(row(&off, "decision.state").status, Some(Status::Off));
}

#[tokio::test]
async fn no_secret_reaches_any_viewer_in_any_row() {
    const SECRETS: [&str; 8] = [
        concat!(
            "sk-ant",
            "-api03-ZXhhbXBsZS1rZXktZm9yLXRlc3RzLW9ubHktMDEyMzQ1Njc4OWFiY2RlZg"
        ),
        "embedding-key-7f3a9c2e5b8d4f1a6c0e",
        "jev-install-key-91c4e7a2d5f80b3e6a1d",
        "vision-key-2b6e9d1f4a7c0e3b5d8f",
        "hunter2-url-password",
        concat!("ghp_01", "23456789abcdefghijklmnopqrstuvwxyzAB"),
        "q-tok-123",
        "frag-456",
    ];
    let hub = hub(|config| {
        config.llm.api_key = Some(SECRETS[0].into());
        config.memory.embedding_api_key = Some(SECRETS[1].into());
        config.decision.backends = vec![toml::from_str(&format!(
            "name = \"jev\"\nurl = \"https://jev.example\"\napi_key = {:?}",
            SECRETS[2]
        ))
        .unwrap()];
        config.llm.vision = Some(
            toml::from_str(&format!(
                "model = \"nonesuch:gpt-5.5\"\napi_key = {:?}",
                SECRETS[3]
            ))
            .unwrap(),
        );
        config.embeddings.rerank_url = Some(format!("https://hub:{}@rerank.internal", SECRETS[4]));
        config.embeddings.url = Some(format!(
            "https://embed.internal/v1?sig={}#{}",
            SECRETS[6], SECRETS[7]
        ));
        config
            .llm
            .aliases
            .insert("leaked".into(), SECRETS[5].into());
    })
    .await;
    let (agent, global) = (
        hub.service.vault.clone().unwrap(),
        hub.service.global_vault.clone().unwrap(),
    );
    let vaulted = [
        "vault-global-key-5d8f2b6e9d1f4a7c",
        "vault-agent-key-0e3b5d8f2b6e9d1f",
        "vault-alice-key-4a7c0e3b5d8f2b6e",
    ];
    global.set(BRAVE_API_KEY.path, vaulted[0]).unwrap();
    agent.set(JEV_API_KEY.path, vaulted[1]).unwrap();
    global
        .set(&format!("users/{ALICE}/{}", JEV_API_KEY.path), vaulted[2])
        .unwrap();

    for viewer in [admin(), member(ALICE), member(BOB)] {
        let rows = all(&hub, &viewer).await;
        let shown = serde_json::to_string(&rows).unwrap();
        for secret in SECRETS.iter().chain(&vaulted) {
            assert!(!shown.contains(secret), "{secret} reached {viewer:?}");
        }
        for row in &rows {
            let text = serde_json::to_string(row).unwrap();
            assert!(!contains_secret(&text), "a secret pattern in {text}");
        }
        assert_eq!(row(&rows, "llm.api_key").value, json!(SET));
        assert_eq!(row(&rows, "memory.embedding_api_key").value, json!(SET));
        assert_eq!(row(&rows, "llm.vision.api_key").value, json!(SET));
    }
    let rows = all(&hub, &admin()).await;
    assert_eq!(
        row(&rows, "embeddings.rerank_url").value,
        json!("https://rerank.internal")
    );
    assert_eq!(
        row(&rows, "embeddings.url").value,
        json!("https://embed.internal/v1")
    );
    assert_eq!(
        row(&rows, "decision.backends").value[0]["api_key"],
        json!(SET),
        "{:?}",
        row(&rows, "decision.backends")
    );
}

#[tokio::test]
async fn install_wide_rows_are_an_admins_and_nobody_unidentified_sees_anything() {
    let hub = hub(|_| {}).await;
    let report = |viewer: Viewer| {
        let service = Arc::clone(&hub.service);
        let session = session(&hub);
        async move { report(&service, &viewer, &session, &Scope::All, false).await }
    };

    let seen = report(member(ALICE)).await.unwrap();
    assert_eq!(seen.viewer, "member");
    assert!(seen.rows.iter().all(|row| !row.install_wide));
    for hidden in ["embeddings", "spill", "aws_s3", "telemetry", "approvals"] {
        assert!(!seen.sections.contains(&hidden), "{hidden}");
    }
    assert!(seen
        .rows
        .iter()
        .all(|row| row.path() != "decision.backends"));
    assert!(seen
        .rows
        .iter()
        .all(|row| !row.path().starts_with("tools.loading.agents")));
    row(&seen.rows, "budget.you.daily_usd");
    row(&seen.rows, "know_how.state");
    row(&seen.rows, "tools.loading.state");
    row(&seen.rows, "decision.records.enabled");
    assert!(
        seen.rows
            .iter()
            .all(|row| row.path() != "decision.records.retention_days"),
        "how long the install keeps records is the install's alone"
    );

    let seen = report(admin()).await.unwrap();
    assert_eq!(seen.viewer, "admin");
    row(&seen.rows, "decision.records.retention_days");
    for shown in ["embeddings", "spill", "aws_s3", "telemetry", "approvals"] {
        assert!(seen.sections.contains(&shown), "{shown}");
    }
    row(&seen.rows, "telemetry.activity_retention_days");
    row(&seen.rows, "telemetry.cost_retention_days");
    row(&seen.rows, "approvals.retention_days");
    row(&seen.rows, "budget.hub_daily_usd");
    row(&seen.rows, "tools.loading.mode");

    let owner = report(Viewer {
        person: None,
        admin: true,
    })
    .await
    .unwrap();
    assert!(owner.sections.contains(&"embeddings"));
    assert!(
        owner.rows.iter().all(|row| !row.key.starts_with("you.")),
        "an admin with no identity has no per-user rows"
    );

    assert_eq!(report(Viewer::default()).await, Err(Refusal::NoIdentity));
}

#[tokio::test]
async fn explain_shows_the_chain_and_a_summary_leaves_out_defaults() {
    let hub = hub(|config| {
        config.know_how.mode = ProgramsMode::On;
        config.know_how.max_entries = 9;
    })
    .await;
    configured(&hub, "[know_how]\nmax_entries = 3\n").await;
    let ask = |scope: Scope, chain: bool| {
        let service = Arc::clone(&hub.service);
        let session = session(&hub);
        async move { report(&service, &member(ALICE), &session, &scope, chain).await }
    };

    let explained = ask(Scope::Key("know_how.max_entries".into()), true)
        .await
        .unwrap();
    assert_eq!(explained.rows.len(), 1);
    assert_eq!(
        serde_json::to_value(&explained.rows[0]).unwrap(),
        json!({
            "section": "know_how",
            "key": "max_entries",
            "value": 3,
            "source": "agent",
            "chain": [
                {"source": "default", "value": 5},
                {"source": "install", "value": 9},
                {"source": "agent", "value": 3},
            ],
        })
    );
    let under = ask(Scope::Key("llm.vision".into()), true).await.unwrap();
    assert!(under.rows.len() > 1 && under.rows.iter().all(|row| row.key.starts_with("vision.")));
    assert!(matches!(
        ask(Scope::Key("know_how.nonesuch".into()), true).await,
        Err(Refusal::UnknownKey { nearest, .. }) if nearest.is_empty()
    ));
    let misplaced = ask(Scope::Key("knowhow.max_entries".into()), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        misplaced.starts_with(
            "no setting \"knowhow.max_entries\" (did you mean know_how.max_entries?): a key is"
        ),
        "{misplaced}"
    );
    assert!(!misplaced.contains(". "), "one sentence: {misplaced}");
    assert!(
        matches!(
            ask(Scope::Section("embeddings".into()), false).await,
            Err(Refusal::UnknownSection { .. })
        ),
        "a member is not told an install-wide section exists"
    );

    let section = ask(Scope::Section("know_how".into()), false).await.unwrap();
    assert!(section
        .rows
        .iter()
        .all(|row| row.section == "know_how" && row.chain.is_empty()));
    let summary = ask(Scope::Summary, false).await.unwrap();
    let everything = ask(Scope::All, false).await.unwrap();
    assert!(summary
        .rows
        .iter()
        .all(|row| row.source != Some(Source::Default)));
    assert_eq!(
        summary.rows.len() + summary.defaults_omitted,
        everything.rows.len()
    );
    assert!(summary.defaults_omitted > 0);
    row(&summary.rows, "know_how.max_entries");
    row(&summary.rows, "llm.vision.state");
}

#[tokio::test]
async fn the_tool_answers_a_member_an_admin_and_refuses_nobody() {
    let hub = hub(|_| {}).await;
    async fn ask(
        tool: SettingsTool,
        person: Option<&str>,
        input: serde_json::Value,
    ) -> workforce_agent::tools::ToolResult {
        let run = tool.execute(input);
        let result = match person {
            Some(person) => CURRENT_USER_ID.scope(person.to_string(), run).await,
            None => run.await,
        };
        result.unwrap()
    }
    let tool = || SettingsTool::new(Arc::clone(&hub.service));

    let seen = ask(tool(), Some(ALICE), json!({"action": "show"})).await;
    assert!(seen.success, "{:?}", seen.error);
    assert_eq!(seen.output["viewer"], "member");
    assert!(seen.output["sections"]
        .as_array()
        .unwrap()
        .contains(&json!("llm")));
    assert!(!seen.output["sections"]
        .as_array()
        .unwrap()
        .contains(&json!("embeddings")));

    let seen = ask(
        tool().with_admin_policy(crate::authz::always_admin()),
        Some(ALICE),
        json!({"action": "show", "section": "embeddings"}),
    )
    .await;
    assert_eq!(seen.output["viewer"], "admin");
    assert!(seen.output["rows"]
        .as_array()
        .is_some_and(|rows| !rows.is_empty()));

    let explained = ask(
        tool(),
        Some(ALICE),
        json!({"action": "explain", "key": "llm.default_model"}),
    )
    .await;
    assert!(
        explained.output["rows"][0]["chain"].is_array(),
        "{explained:?}"
    );

    let refused = ask(tool(), None, json!({"action": "show"})).await;
    assert!(!refused.success);
    assert!(refused.error.unwrap().contains("nobody is identified"));
    let unknown = ask(tool(), Some(ALICE), json!({"action": "set"})).await;
    assert!(!unknown.success);
}

#[tokio::test]
async fn every_section_reports_and_the_tool_is_an_action_dispatch_tool() {
    let hub = hub(|_| {}).await;
    let rows = all(&hub, &admin()).await;
    for section in names() {
        assert!(
            rows.iter().any(|row| row.section == section),
            "{section} has no rows"
        );
    }
    let tool = SettingsTool::new(Arc::clone(&hub.service));
    assert!(ACTION_DISPATCH_TOOLS.contains(&tool.name()));
    assert_eq!(
        tool.input_schema()["properties"]["action"]["enum"],
        json!(["show", "explain"])
    );
    assert!(
        hub.service.tools.get("settings").is_some(),
        "registered on the agent"
    );
}

#[test]
fn every_install_table_is_a_section_or_is_left_out_on_purpose() {
    let install = serde_json::to_value(WorkforceConfig::default()).unwrap();
    let tables: BTreeSet<&str> = install
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let sections: BTreeSet<&str> = names().collect();
    let left_out: BTreeSet<&str> = NOT_REPORTED
        .iter()
        .flat_map(|(_why, tables)| tables.iter().copied())
        .collect();
    assert_eq!(
        left_out.len(),
        NOT_REPORTED
            .iter()
            .map(|(_, tables)| tables.len())
            .sum::<usize>(),
        "a table is left out for one reason"
    );

    let undescribed: Vec<&&str> = tables
        .iter()
        .filter(|table| !sections.contains(*table) && !left_out.contains(*table))
        .collect();
    assert!(
        undescribed.is_empty(),
        "install tables with no settings section: {undescribed:?}. Give each a table in \
         `sections::tables` (or a section of its own), or list it in NOT_REPORTED"
    );
    let stale: Vec<&&str> = left_out
        .iter()
        .filter(|table| !tables.contains(*table) || sections.contains(*table))
        .collect();
    assert!(
        stale.is_empty(),
        "NOT_REPORTED names {stale:?}, which is not left out"
    );
}

#[tokio::test]
async fn handoff_reports_the_policy_the_kill_switch_and_a_bounded_override() {
    let (allowed, forbidden) = (
        hub(|_| {}).await,
        hub(|config| config.handoff.allow_verify = false).await,
    );
    let rows = all(&allowed, &member(ALICE)).await;
    for (key, value) in [
        ("handoff.allow_verify", json!(true)),
        ("handoff.verify", json!(false)),
        ("handoff.max_corrections", json!(1)),
    ] {
        let default = row(&rows, key);
        assert_eq!(
            (default.source, &default.value, default.status),
            (Some(Source::Default), &value, None),
            "{key}"
        );
    }

    let own = "[handoff]\nverify = true\nmax_corrections = 9\nmodel = \"cheap\"\n";
    configured(&allowed, own).await;
    let rows = all(&allowed, &member(ALICE)).await;
    let verify = row(&rows, "handoff.verify");
    assert_eq!(
        (verify.source, &verify.value, verify.status),
        (Some(Source::Agent), &json!(true), None)
    );
    assert_eq!(row(&rows, "handoff.model").value, json!("cheap"));
    let bounded = row(&rows, "handoff.max_corrections");
    assert_eq!(
        (bounded.source, &bounded.value, bounded.status),
        (Some(Source::Agent), &json!(3), Some(Status::Inert))
    );
    assert_eq!(
        reason(&rows, "handoff.max_corrections"),
        "the agent's config.toml sets 9, which is out of bounds: 3 is in effect"
    );

    configured(&forbidden, own).await;
    let rows = all(&forbidden, &member(ALICE)).await;
    let switch = row(&rows, "handoff.allow_verify");
    assert_eq!(
        (switch.source, &switch.value, switch.status),
        (
            Some(Source::Install),
            &json!(false),
            Some(Status::KillSwitched)
        )
    );
    assert_eq!(row(&rows, "handoff.verify").status, Some(Status::Inert));
    assert!(reason(&rows, "handoff.verify").contains("the install forbids it"));
}

#[tokio::test]
async fn a_member_is_not_shown_where_the_install_runs_or_which_other_agents_it_serves() {
    let hub = hub(|config| {
        config.tools.programs.mode = ProgramsMode::On;
        config.tools.programs.except = vec!["grace".into()];
        config.llm.accounts = vec!["ANTHROPIC_API_KEY_2".into()];
        config.advisor.agent_id = Some("grace".into());
        config.llm.vision = Some(
            toml::from_str("model = \"nonesuch:gpt-5.5\"\nurl = \"http://10.20.0.6:9000\"")
                .unwrap(),
        );
    })
    .await;
    const INTERNAL: [&str; 6] = [
        "memory.db_path",
        "self_learning.db_path",
        "llm.vision.url",
        "llm.accounts",
        "tools.programs.except",
        "advisor.agent_id",
    ];

    let rows = all(&hub, &member(ALICE)).await;
    for hidden in INTERNAL {
        assert!(rows.iter().all(|row| row.path() != hidden), "{hidden}");
    }
    let shown = serde_json::to_string(&rows).unwrap();
    for internal in ["grace", "10.20.0.6", "ANTHROPIC_API_KEY_2"] {
        assert!(!shown.contains(internal), "{internal} reached a member");
    }
    assert_eq!(
        row(&rows, "tools.programs.state").status,
        Some(Status::Active)
    );
    row(&rows, "llm.vision.model");

    let rows = all(&hub, &admin()).await;
    for shown in INTERNAL {
        assert_eq!(
            rows.iter().filter(|row| row.path() == shown).count(),
            1,
            "{shown}"
        );
    }
}

#[tokio::test]
async fn a_consolidation_edit_takes_effect_without_a_restart() {
    let hub = hub(|_| {}).await;
    let started = row(&all(&hub, &member(ALICE)).await, "memory.consolidation").clone();
    assert_eq!(
        (started.source, started.status),
        (Some(Source::Default), None)
    );

    // Consolidation re-arms its job when the config changes, so the edit
    // shows as live at once rather than waiting for a restart.
    configured(
        &hub,
        &format!(
            "[memory]\nconsolidation = {}\npassive = {}\n",
            started.value == json!(false),
            hub.service.resolved_at_start.memory.passive
        ),
    )
    .await;
    let rows = all(&hub, &member(ALICE)).await;
    let held = row(&rows, "memory.consolidation");
    assert_eq!(held.status, None, "{held:?}");
    assert_eq!(held.value, json!(started.value == json!(false)));
    assert_eq!(row(&rows, "memory.passive").status, None);
}

#[tokio::test]
async fn a_refused_config_toml_is_reported_without_quoting_it() {
    let hub = hub(|_| {}).await;
    let state = |rows: &[Row]| {
        let row = row(rows, "config.state");
        (row.value.clone(), row.status)
    };
    assert_eq!(
        state(&all(&hub, &member(ALICE)).await),
        (json!("none"), Some(Status::Off))
    );
    configured(&hub, "[know_how]\nmax_entries = 3\n").await;
    assert_eq!(
        state(&all(&hub, &member(ALICE)).await),
        (json!("applied"), Some(Status::Active))
    );

    let file = hub.service.workspace.join("config.toml");
    std::fs::write(
        &file,
        "[know_how]\nmax_entries = 4\n[llm]\napi_key = \"tiny-key\" oops\n",
    )
    .unwrap();
    assert!(hub.service.reload_config().await.is_err());
    let rows = all(&hub, &member(ALICE)).await;
    assert_eq!(state(&rows), (json!("rejected"), Some(Status::Inert)));
    let why = reason(&rows, "config.state");
    assert!(why.contains("TOML parse error at line 4"), "{why}");
    assert!(
        !why.contains("tiny-key") && !why.contains(hub.service.workspace.to_str().unwrap()),
        "{why}"
    );
    assert_eq!(
        row(&rows, "know_how.max_entries").value,
        json!(3),
        "the values are the last accepted config's"
    );
    let summary = report(
        &hub.service,
        &member(ALICE),
        &session(&hub),
        &Scope::Summary,
        false,
    )
    .await
    .unwrap();
    row(&summary.rows, "config.state");

    configured(&hub, "[know_how]\nmax_entries = 4\n").await;
    assert_eq!(
        state(&all(&hub, &member(ALICE)).await),
        (json!("applied"), Some(Status::Active))
    );
}

#[tokio::test]
async fn a_provider_the_config_does_not_name_is_reported_as_the_one_serving() {
    use workforce_agent::llm::provider::ProviderKind;
    let hub = hub(|_| {}).await;
    let fresh = row(&all(&hub, &member(ALICE)).await, "llm.provider").clone();
    assert_eq!(fresh.status, None, "{fresh:?}");

    let other = [ProviderKind::Grok, ProviderKind::Gemini]
        .into_iter()
        .find(|kind| *kind != *hub.service.provider_kind.read())
        .unwrap();
    *hub.service.provider_kind.write() = other;
    let rows = all(&hub, &member(ALICE)).await;
    let serving = row(&rows, "llm.provider");
    assert_eq!(
        (&serving.value, serving.status),
        (&json!(other.canonical_name()), Some(Status::Inert))
    );
    assert_eq!(
        reason(&rows, "llm.provider"),
        format!(
            "the config now gives {}; the running agent keeps this value until it restarts",
            fresh.value
        )
    );
}

#[tokio::test]
async fn a_hub_wide_credential_is_read_from_the_global_vault_alone() {
    let hub = hub(|_| {}).await;
    let embeddings = &crate::credentials::EMBEDDINGS_OPENAI_API_KEY;
    let path = format!("credentials.{}", embeddings.path);
    hub.service
        .vault
        .as_ref()
        .unwrap()
        .set(embeddings.path, "agent-embeddings-key")
        .unwrap();
    let unread = all(&hub, &admin()).await;
    assert_ne!(
        row(&unread, &path).source,
        Some(Source::Vault),
        "the hub's embedder never reads an agent's vault"
    );
    assert!(
        row(&unread, &path).source.is_some()
            || reason(&unread, &path).contains(&format!(
                "An admin sets it in the global vault at {}",
                embeddings.path
            )),
        "{}",
        reason(&unread, &path)
    );
    hub.service
        .global_vault
        .as_ref()
        .unwrap()
        .set(embeddings.path, "global-embeddings-key")
        .unwrap();
    assert!(
        all(&hub, &member(ALICE))
            .await
            .iter()
            .all(|row| row.path() != path),
        "whether the hub has a key is an admin's to see"
    );
    let rows = all(&hub, &admin()).await;
    assert_eq!(row(&rows, &path).source, Some(Source::Vault));
    assert!(reason(&rows, &path).ends_with("set (global level)"));
}

#[tokio::test]
async fn a_capped_report_drops_defaults_first_and_says_what_it_left_out() {
    let hub = hub(|config| config.know_how.max_tokens = 600).await;
    let everything = report(&hub.service, &admin(), &session(&hub), &Scope::All, false)
        .await
        .unwrap();
    let all = everything.rows.len();
    assert_eq!(everything.clone().capped(all), everything);

    let capped = everything.capped(12);
    assert_eq!(capped.rows.len(), 12);
    assert!(capped
        .rows
        .iter()
        .all(|row| row.source != Some(Source::Default) || row.status.is_some()));
    assert_eq!(
        capped.note.as_deref(),
        Some(
            format!(
                "{} of {all} rows are left out to keep this short; show one section or explain \
                 one key to see them",
                all - 12
            )
            .as_str()
        )
    );
}

#[tokio::test]
async fn an_override_within_bounds_is_in_effect_for_every_numeric_knob() {
    let hub = hub(|config| {
        config.know_how.mode = ProgramsMode::On;
        config.tools.programs.mode = ProgramsMode::On;
        config.tools.compile_gate.mode = ProgramsMode::On;
        config.tools.refactor.mode = ProgramsMode::On;
    })
    .await;
    configured(
        &hub,
        r#"
[know_how]
min_users = 4
max_entries = 3
max_tokens = 500
min_score = 0.3

[research]
low_confidence_below = 55

[handoff]
max_corrections = 2

[attachments.video]
max_frames = 7
min_interval_secs = 2.5
max_minutes = 9
max_long_edge = 900
detail_long_edge = 1200
jpeg_quality = 70
scene_threshold = 0.3

[compaction]
max_context_tokens = 90000
compaction_threshold = 0.7
keep_recent_messages = 11
max_compaction_passes = 2
urgent_compaction_threshold = 0.9
soft_eviction_threshold = 0.6

[memory]
passive_min_turn_tokens = 123
relationship_min_turn_tokens = 321
consolidation_lookback_secs = 7200
lesson_inject_limit = 4
lesson_inject_min_confidence = 0.3
lesson_inject_min_importance = 0.7

[advisor]
payload_max_tokens = 9000
per_turn_invocation_limit = 2
max_rounds = 6

[tools.programs]
max_calls = 17
max_concurrency = 3
timeout_secs = 45
max_steps = 5000
max_return_bytes = 20000
max_sql_rows = 150
nudge_after = 4

[tools.compile_gate]
max_fix_rounds = 2
timeout_secs = 90

[session_digest]
enabled = true
max_sessions = 4
window_hours = 30
max_chars = 3000
"#,
    )
    .await;
    let rows = all(&hub, &admin()).await;
    let own: Vec<&Row> = rows
        .iter()
        .filter(|row| row.chain.last().map(|layer| layer.source) == Some(Source::Agent))
        .collect();
    assert!(own.len() >= 40, "{} rows set by the agent", own.len());
    for row in own {
        assert_eq!(
            (row.source, row.status, &row.reason),
            (Some(Source::Agent), None, &None),
            "{} = {}",
            row.path(),
            row.value
        );
        assert_eq!(row.value, row.chain.last().unwrap().value, "{}", row.path());
    }
}
