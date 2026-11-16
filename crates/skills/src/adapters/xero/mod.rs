//! Xero skill — agent-callable tools for the Xero Accounting API.
//!
//! Auth is the client-credentials grant of a Xero "Custom Connection" (one per
//! organisation). Multiple entities are supported via an optional `entity`
//! parameter on every tool. Read tools default to `allow`; mutating tools to
//! `require_approval`.

pub mod client;
pub mod config;
pub mod export;
pub mod report;
pub mod tools;

use std::sync::Arc;

use workforce_agent::tools::Tool;

use crate::adapters::utils::{build_instances, InstancePaths, LiveInstances};
use crate::definition::{CredentialRequirement, SkillDefinition, SkillRequirements};

pub use client::XeroClient;
pub use config::{XeroEntityConfig, XeroSkillConfig};

/// The Xero skill — its entities, read as the caller on each call in store
/// mode.
pub struct XeroSkill {
    instances: Arc<LiveInstances<XeroClient>>,
}

impl XeroSkill {
    /// Build from config with static credentials.
    pub fn new(config: XeroSkillConfig) -> eyre::Result<Self> {
        let instances = build_instances(
            "xero",
            &config.resolve(),
            config.default_entity.as_deref(),
            "No Xero entities configured. Set `client_id`+`client_secret` or `[entities.<name>]`.",
            |_, e: &XeroEntityConfig| {
                XeroClient::new(&e.client_id, &e.client_secret, e.tenant_id.clone())
            },
        )?;
        Ok(Self {
            instances: Arc::new(LiveInstances::fixed(instances)),
        })
    }

    /// Build a skill whose entities, credentials and pinned tenant are all
    /// read from `store` per call, as the caller: nothing of whoever loaded
    /// it is kept. `default_entity` is the shared selector.
    pub fn with_store(
        default_entity: Option<String>,
        store: Arc<dyn workforce_common::settings::SecretStore>,
    ) -> eyre::Result<Self> {
        let clients = Arc::clone(&store);
        let instances =
            LiveInstances::live(store, Self::instance_paths(), default_entity, move |name| {
                XeroClient::with_store(Arc::clone(&clients), name)
            });
        Ok(Self {
            instances: Arc::new(instances),
        })
    }

    /// [`Self::with_store`] with every client pointed at a mock server.
    #[cfg(test)]
    pub(crate) fn with_store_for_test(
        default_entity: Option<String>,
        store: Arc<dyn workforce_common::settings::SecretStore>,
        base: &str,
    ) -> Self {
        let clients = Arc::clone(&store);
        let base = base.to_string();
        let instances =
            LiveInstances::live(store, Self::instance_paths(), default_entity, move |name| {
                Ok(XeroClient::with_store_and_endpoints_for_test(
                    Arc::clone(&clients),
                    name,
                    &base,
                ))
            });
        Self {
            instances: Arc::new(instances),
        }
    }

    /// Where Xero's entities live in the vault.
    pub fn instance_paths() -> InstancePaths {
        InstancePaths::new(
            "skills/xero/instances/",
            "client_id",
            "skills/xero/client_id",
        )
    }

    /// The entity a call naming none goes to, for the caller in scope.
    pub fn default_entity(&self) -> String {
        self.instances.for_caller().1
    }

    /// The entities this skill exposes, read as whoever asks.
    pub fn instance_source(&self) -> Arc<dyn workforce_agent::llm::environment::InstanceSource> {
        Arc::clone(&self.instances) as Arc<dyn workforce_agent::llm::environment::InstanceSource>
    }

    /// All agent-callable tools.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        tools::build_tools(Arc::clone(&self.instances))
    }

    /// Tool names this skill provides, in registration order.
    pub fn tool_names() -> Vec<&'static str> {
        tools::tool_names()
    }

    /// Static skill definition (metadata + policies).
    pub fn definition() -> SkillDefinition {
        let (tools, default_policies) = tools::skill_tools_and_policies();
        SkillDefinition {
            name: "xero-integration".to_string(),
            version: "1.0.0".to_string(),
            description: "Query and act on Xero accounting data (P&L, balance sheet, invoices, \
                          contacts, journals) across one or more organisations."
                .to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),
            tools,
            event_sources: Vec::new(),
            default_policies,
            memory_schemas: Vec::new(),
            requirements: SkillRequirements {
                credentials: vec![
                    CredentialRequirement {
                        name: "xero_client_id".to_string(),
                        r#type: "oauth2".to_string(),
                        description: "Xero custom-connection client id".to_string(),
                    },
                    CredentialRequirement {
                        name: "xero_client_secret".to_string(),
                        r#type: "oauth2".to_string(),
                        description: "Xero custom-connection client secret".to_string(),
                    },
                ],
                config: Vec::new(),
            },
            dependencies: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_single() -> XeroSkillConfig {
        XeroSkillConfig {
            client_id: Some("cid".into()),
            client_secret: Some("secret".into()),
            ..XeroSkillConfig::default()
        }
    }

    #[test]
    fn new_errors_without_entities() {
        let err = XeroSkill::new(XeroSkillConfig::default())
            .err()
            .expect("expected error");
        assert!(err.to_string().contains("No Xero entities"));
    }

    #[test]
    fn new_promotes_flat_to_default() {
        let skill = XeroSkill::new(config_single()).unwrap();
        assert_eq!(skill.default_entity(), "default");
        assert!(!skill.tools().is_empty());
    }

    fn named(names: &[&str], default_entity: Option<&str>) -> XeroSkillConfig {
        XeroSkillConfig {
            entities: names
                .iter()
                .map(|name| {
                    (
                        name.to_string(),
                        XeroEntityConfig {
                            client_id: "a".into(),
                            client_secret: "b".into(),
                            tenant_id: None,
                        },
                    )
                })
                .collect(),
            default_entity: default_entity.map(str::to_string),
            ..XeroSkillConfig::default()
        }
    }

    #[test]
    fn a_sole_named_entity_becomes_the_default_even_over_a_stale_selector() {
        assert_eq!(
            XeroSkill::new(named(&["harbor"], None))
                .unwrap()
                .default_entity(),
            "harbor"
        );
        assert_eq!(
            XeroSkill::new(named(&["harbor"], Some("nope")))
                .unwrap()
                .default_entity(),
            "harbor"
        );
        assert_eq!(
            XeroSkill::new(XeroSkillConfig {
                default_entity: Some("nope".into()),
                ..config_single()
            })
            .unwrap()
            .default_entity(),
            "default"
        );
    }

    #[test]
    fn several_entities_without_a_selector_load_and_keep_default_for_the_call_error() {
        let skill = XeroSkill::new(named(&["harbor", "beacon"], None)).unwrap();
        assert_eq!(skill.default_entity(), "default");
        let skill = XeroSkill::new(named(&["harbor", "beacon"], Some("beacon"))).unwrap();
        assert_eq!(skill.default_entity(), "beacon");
    }

    #[test]
    fn definition_lists_all_tools_with_policies() {
        let def = XeroSkill::definition();
        assert_eq!(def.name, "xero-integration");
        assert_eq!(def.default_policies.len(), def.tools.len());
        let names: Vec<&str> = def.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"xero.profit_and_loss"));
        assert!(names.contains(&"xero.manual_journal_create"));
    }

    #[test]
    fn tool_names_matches_registered() {
        let skill = XeroSkill::new(config_single()).unwrap();
        let registered: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        let declared: Vec<String> = XeroSkill::tool_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(registered, declared);
    }
    // ── One loaded skill, two people ──
    //
    // A loads the skill (it is built while A is in scope), B uses it. B must
    // reach only B's entities, keys and tenant, and nothing B sees may name
    // A's.

    use crate::adapters::test_support::People;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use workforce_agent::runner::CURRENT_USER_ID;

    async fn as_person<F: std::future::Future>(person: &str, f: F) -> F::Output {
        CURRENT_USER_ID.scope(person.to_string(), f).await
    }

    async fn mock_xero(discovered: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/connect/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "tok", "token_type": "Bearer", "expires_in": 1800
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/connections"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!([{ "tenantId": discovered, "tenantType": "ORGANISATION" }]),
                ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api.xro/2.0/Organisation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "Organisations": [] })))
            .mount(&server)
            .await;
        server
    }

    /// The tenant headers the Organisation calls went out with, in order.
    async fn tenants_sent(server: &MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().ends_with("/Organisation"))
            .map(|r| {
                r.headers
                    .get("xero-tenant-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    fn organisation(skill: &XeroSkill) -> Arc<dyn workforce_agent::tools::Tool> {
        skill
            .tools()
            .into_iter()
            .find(|t| t.name() == "xero.organisation")
            .expect("xero.organisation")
    }

    #[tokio::test]
    async fn each_persons_own_keys_go_out_with_their_own_tenant() {
        let server = mock_xero("T-DISCOVERED").await;
        let store = People::new(&[]);
        store
            .own("ann", "skills/xero/client_id", "id-ann")
            .own("ann", "skills/xero/client_secret", "secret-ann")
            .own("ann", "skills/xero/tenant_id", "T-ANN")
            .own("bob", "skills/xero/client_id", "id-bob")
            .own("bob", "skills/xero/client_secret", "secret-bob")
            .own("bob", "skills/xero/tenant_id", "T-BOB");
        let skill = as_person("ann", async {
            XeroSkill::with_store_for_test(None, store.clone(), &server.uri())
        })
        .await;
        let tool = organisation(&skill);

        for person in ["ann", "bob", "ann"] {
            let ran = as_person(person, tool.execute(json!({}))).await.unwrap();
            assert!(ran.success, "{person}: {:?}", ran.error);
        }
        assert_eq!(tenants_sent(&server).await, ["T-ANN", "T-BOB", "T-ANN"]);
    }

    #[tokio::test]
    async fn a_shared_connection_never_carries_one_persons_tenant() {
        // The agent's shared connection; Ann pinned a tenant of her own
        // beside it. Bob, using the shared connection, must not send it, and
        // neither does Ann: a pin travels only with keys of one's own.
        let server = mock_xero("T-DISCOVERED").await;
        let store = People::new(&[
            ("skills/xero/client_id", "id-shared"),
            ("skills/xero/client_secret", "secret-shared"),
        ]);
        store.own("ann", "skills/xero/tenant_id", "T-ANN");
        let skill = as_person("ann", async {
            XeroSkill::with_store_for_test(None, store.clone(), &server.uri())
        })
        .await;
        let tool = organisation(&skill);
        for person in ["ann", "bob"] {
            let ran = as_person(person, tool.execute(json!({}))).await.unwrap();
            assert!(ran.success, "{person}: {:?}", ran.error);
        }
        assert_eq!(
            tenants_sent(&server).await,
            ["T-DISCOVERED", "T-DISCOVERED"]
        );
    }

    #[tokio::test]
    async fn a_named_entity_never_takes_the_flat_keys_tenant() {
        // Legacy flat keys with a pinned tenant, beside a named entity of
        // the agent's with none: the entity discovers its own.
        let server = mock_xero("T-DISCOVERED").await;
        let store = People::new(&[
            ("skills/xero/tenant_id", "T-FLAT"),
            ("skills/xero/instances/harbor/client_id", "id-harbor"),
            (
                "skills/xero/instances/harbor/client_secret",
                "secret-harbor",
            ),
        ]);
        let skill = XeroSkill::with_store_for_test(None, store, &server.uri());
        let ran = as_person("ann", organisation(&skill).execute(json!({})))
            .await
            .unwrap();
        assert!(ran.success, "{:?}", ran.error);
        assert_eq!(tenants_sent(&server).await, ["T-DISCOVERED"]);
    }

    #[tokio::test]
    async fn a_persons_own_connection_takes_nothing_shared_with_it() {
        // Bob's own id and secret; the agent's shared tenant pin is not his.
        let server = mock_xero("T-DISCOVERED").await;
        let store = People::new(&[
            ("skills/xero/client_secret", "secret-shared"),
            ("skills/xero/tenant_id", "T-SHARED"),
        ]);
        store
            .own("bob", "skills/xero/client_id", "id-bob")
            .own("bob", "skills/xero/client_secret", "secret-bob")
            .own("cy", "skills/xero/client_id", "id-cy");
        let skill = XeroSkill::with_store_for_test(None, store, &server.uri());
        let tool = organisation(&skill);
        let ran = as_person("bob", tool.execute(json!({}))).await.unwrap();
        assert!(ran.success, "{:?}", ran.error);
        assert_eq!(tenants_sent(&server).await, ["T-DISCOVERED"]);
        // Cy has only an id of her own: the shared secret doesn't complete it.
        let ran = as_person("cy", tool.execute(json!({}))).await.unwrap();
        assert!(!ran.success);
        assert!(
            ran.error
                .unwrap_or_default()
                .contains("client_secret not configured"),
            "the shared secret joined Cy's id"
        );
        assert_eq!(tenants_sent(&server).await.len(), 1);
    }

    #[tokio::test]
    async fn a_person_sees_and_reaches_only_their_own_entities() {
        let server = mock_xero("T-DISCOVERED").await;
        let store = People::new(&[]);
        store
            .own("ann", "skills/xero/instances/harbor/client_id", "id-ann")
            .own(
                "ann",
                "skills/xero/instances/harbor/client_secret",
                "secret-ann",
            )
            .own("ann", "skills/xero/instances/harbor/tenant_id", "T-HARBOR")
            .own(
                "ann",
                "skills/xero/instances/anchorage/client_id",
                "id-ann2",
            )
            .own(
                "ann",
                "skills/xero/instances/anchorage/client_secret",
                "secret-ann2",
            )
            .own("bob", "skills/xero/instances/beacon/client_id", "id-bob")
            .own(
                "bob",
                "skills/xero/instances/beacon/client_secret",
                "secret-bob",
            )
            .own("bob", "skills/xero/instances/beacon/tenant_id", "T-BEACON");
        let skill = as_person("ann", async {
            XeroSkill::with_store_for_test(Some("harbor".into()), store.clone(), &server.uri())
        })
        .await;
        let tools = skill.tools();

        // Nothing built once for everyone names Ann's entities.
        for tool in &tools {
            let schema = tool.input_schema().to_string();
            for name in ["harbor", "anchorage"] {
                assert!(!schema.contains(name), "{}: {schema}", tool.name());
            }
        }

        as_person("bob", async {
            assert_eq!(skill.instances.names(), ["beacon"]);
            assert_eq!(skill.default_entity(), "beacon");
            let tool = organisation(&skill);
            // Bob's sole entity is his default, though the loader's was harbor.
            let ran = tool.execute(json!({})).await.unwrap();
            assert!(ran.success, "{:?}", ran.error);
            // An entity he has none of: the error names only his.
            let ran = tool.execute(json!({ "entity": "nope" })).await.unwrap();
            let err = ran.error.unwrap_or_default();
            assert!(err.contains("beacon"), "{err}");
            assert!(
                !err.contains("harbor") && !err.contains("anchorage"),
                "{err}"
            );
            // Naming Ann's entity reaches nothing of hers.
            let ran = tool.execute(json!({ "entity": "harbor" })).await.unwrap();
            assert!(!ran.success);
            assert!(!ran.error.unwrap_or_default().contains("anchorage"));
            // The export tool resolves the same way.
            let export = skill
                .tools()
                .into_iter()
                .find(|t| t.name() == "xero.export")
                .expect("xero.export");
            let ran = export
                .execute(json!({ "datasets": ["invoices"], "entity": "nope" }))
                .await
                .unwrap();
            let err = ran.error.unwrap_or_default();
            assert!(
                !err.contains("harbor") && !err.contains("anchorage"),
                "{err}"
            );
        })
        .await;
        assert_eq!(tenants_sent(&server).await, ["T-BEACON"]);

        // The environments' view of the instances is the caller's too.
        let variants = |person: &'static str| {
            let source = skill.instance_source();
            async move { as_person(person, async { source.variants() }).await }
        };
        assert!(variants("bob").await.is_none(), "Bob has one entity");
        let ann = variants("ann").await.expect("Ann has two");
        assert_eq!(ann.instances, ["anchorage", "harbor"]);
    }
}
