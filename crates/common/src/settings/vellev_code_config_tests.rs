//! Where code vectors are kept and how they are embedded (`[vellev.code]`),
//! and whether code questions search them (`[knowledge]
//! code_search_enabled`).

use super::*;

const SMALL: &str = "BAAI/bge-small-en-v1.5";
const BGE_PREFIX: &str = "Represent this sentence for searching relevant passages: ";
const CODE_ON_VELLEV: &str = "[knowledge]\nvector_backend = \"vellev\"\n";

/// `toml`, with `pairs` as its environment and nothing else.
fn config(toml: &str, pairs: &[(&str, &str)]) -> WorkforceConfig {
    let mut config = WorkforceConfig::from_toml(toml).unwrap();
    config.apply_env_overrides_from(&|name| {
        pairs
            .iter()
            .find(|(set, _)| *set == name)
            .map(|(_, value)| value.to_string())
            .ok_or(std::env::VarError::NotPresent)
    });
    config
}

/// Each field of a node as `(value, table)`, the key's as present or not.
fn fields(node: VellevNode<'_>) -> [(Option<&str>, &'static str); 3] {
    [
        (Some(node.url.value), node.url.table),
        (
            node.api_key.map(|key| key.value),
            node.api_key.map_or("", |key| key.table),
        ),
        (Some(node.tenant.value), node.tenant.table),
    ]
}

fn refused(toml: &str, pairs: &[(&str, &str)]) -> String {
    config(toml, pairs)
        .validate()
        .expect_err("refused")
        .to_string()
}

#[test]
fn code_vectors_are_kept_where_vellev_code_says_and_in_vellev_otherwise() {
    let vellev = "[vellev]\nurl = \"http://memories:18080\"\napi_key = \"k\"\ntenant = \"acme\"\n";
    let plain = config(vellev, &[]);
    assert!(
        plain.vellev.code_node()
            == VellevNode {
                keys: &[CODE_API_KEY, VELLEV_API_KEY],
                ..plain.vellev.node()
            }
    );
    assert_eq!(
        fields(plain.vellev.code_node()),
        [
            (Some("http://memories:18080"), "[vellev]"),
            (Some("k"), "[vellev]"),
            (Some("acme"), "[vellev]"),
        ]
    );
    assert_eq!(plain.vellev.code_model().unwrap(), None);

    let by_file = |key: &str, value: &str| {
        config(
            &format!("{vellev}[vellev.code]\n{key} = \"{value}\"\n"),
            &[],
        )
    };
    for (at, (key, variable)) in [
        ("url", "WORKFORCE_VELLEV_CODE_URL"),
        ("api_key", "WORKFORCE_VELLEV_CODE_API_KEY"),
        ("tenant", "WORKFORCE_VELLEV_CODE_TENANT"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut wanted = fields(plain.vellev.node());
        wanted[at] = (Some("http://code:18080"), "[vellev.code]");
        let from_file = by_file(key, "http://code:18080");
        assert_eq!(fields(from_file.vellev.code_node()), wanted, "{key}");
        let from_env = config(vellev, &[(variable, "http://code:18080")]);
        assert_eq!(fields(from_env.vellev.code_node()), wanted, "{variable}");
        for moved in [&from_file, &from_env] {
            assert!(
                moved.vellev.node() == plain.vellev.node(),
                "{key} moves code alone"
            );
        }
        let empty = config(vellev, &[(variable, " ")]);
        assert_eq!(
            fields(empty.vellev.code_node()),
            fields(plain.vellev.node()),
            "an empty {variable} is unset"
        );
    }

    let prefixed = |prefix: &str, wanted: QueryPrefix<'_>| {
        let toml = format!("[vellev.code]\nmodel = \"{SMALL}\"\ndimensions = 384\n{prefix}");
        let config = config(&toml, &[]);
        let resolved = config.vellev.code_model().unwrap();
        assert_eq!(
            resolved.map(|model| model.query_prefix),
            Some(wanted),
            "{prefix}"
        );
    };
    prefixed("query_prefix = \"\"\n", QueryPrefix::None);
    prefixed("", QueryPrefix::Unset);
    prefixed(
        &format!("query_prefix = \"{BGE_PREFIX}\"\n"),
        QueryPrefix::Given(BGE_PREFIX),
    );
    let from_env = config(
        "",
        &[
            ("WORKFORCE_VELLEV_CODE_MODEL", SMALL),
            ("WORKFORCE_VELLEV_CODE_DIMENSIONS", "384"),
            ("WORKFORCE_VELLEV_CODE_QUERY_PREFIX", "query: "),
        ],
    );
    assert_eq!(
        from_env.vellev.code_model().unwrap(),
        Some(CodeModel {
            model: SMALL,
            dimensions: 384,
            query_prefix: QueryPrefix::Given("query: "),
        })
    );

    let unvalidated = config(
        &format!("[vellev.code]\nmodel = \"{SMALL}\"\ndimensions = 384\n"),
        &[("WORKFORCE_VELLEV_CODE_DIMENSIONS", "384x")],
    );
    let refused = unvalidated.vellev.code_model().unwrap_err().to_string();
    assert!(
        refused.contains("WORKFORCE_VELLEV_CODE_DIMENSIONS") && refused.contains("\"384x\""),
        "{refused}"
    );

    let secret = config(
        &format!("[vellev.code]\napi_key = \"code-secret\"\nmodel = \"{SMALL}\"\n"),
        &[],
    );
    let shown = format!("{:?}", secret.vellev);
    assert!(
        shown.contains("<redacted>") && !shown.contains("code-secret"),
        "{shown}"
    );
}

#[test]
fn code_on_vellev_needs_a_reachable_node_and_a_whole_model() {
    let vellev = "[vellev]\napi_key = \"k\"\ntenant = \"acme\"\n";
    let code = |keys: &str| format!("{CODE_ON_VELLEV}{vellev}[vellev.code]\n{keys}");
    for (toml, said) in [
        (
            code("url = \"vellev-code:18080\"\n"),
            "[vellev.code] url must be an http:// or https:// address of a vellev node, not \"vellev-code:18080\"",
        ),
        (
            code("tenant = \" \"\n"),
            "[knowledge] vector_backend = \"vellev\" needs [vellev.code] tenant (or WORKFORCE_VELLEV_CODE_TENANT)",
        ),
        (
            format!("{CODE_ON_VELLEV}[vellev.code]\ntenant = \"code\"\n"),
            "[knowledge] vector_backend = \"vellev\" needs [vellev.code] api_key or [vellev] api_key (or WORKFORCE_VELLEV_CODE_API_KEY or WORKFORCE_VELLEV_API_KEY)",
        ),
        (
            code(&format!("model = \"{SMALL}\"\n")),
            "[vellev.code] model needs [vellev.code] dimensions",
        ),
        (
            code(&format!("model = \"{SMALL}\"\ndimensions = 0\n")),
            "[vellev.code] dimensions must be more than 0",
        ),
        (
            code("query_prefix = \"query: \"\n"),
            "[vellev.code] query_prefix is read only with a model",
        ),
    ] {
        let refused = refused(&toml, &[]);
        assert!(refused.contains(said), "{toml}: {refused}");
    }
    let refused = refused(
        &code("dimensions = 768\n"),
        &[
            ("WORKFORCE_VELLEV_CODE_MODEL", SMALL),
            ("WORKFORCE_VELLEV_CODE_DIMENSIONS", "384x"),
        ],
    );
    assert!(
        refused.contains("WORKFORCE_VELLEV_CODE_DIMENSIONS") && refused.contains("\"384x\""),
        "{refused}"
    );

    let accepted = |toml: &str, pairs| {
        let config = config(toml, pairs);
        config.validate().unwrap_or_else(|e| panic!("{toml}: {e}"));
        config
    };
    accepted(
        &format!("{CODE_ON_VELLEV}[vellev]\napi_key = \"k\"\n[vellev.code]\ntenant = \"code\"\n"),
        &[],
    );
    accepted(
        &format!(
            "{CODE_ON_VELLEV}[memory]\nbackend = \"vellev\"\n[vellev]\nurl = \"http://one:18080\"\napi_key = \"k\"\ntenant = \"acme\"\n[vellev.code]\nurl = \"http://two:18080\"\n"
        ),
        &[],
    );
    let sized = accepted(
        &code(&format!("model = \"{SMALL}\"\ndimensions = 768\n")),
        &[("WORKFORCE_VELLEV_CODE_DIMENSIONS", "384")],
    );
    assert_eq!(
        sized.vellev.code_model().unwrap().map(|m| m.dimensions),
        Some(384)
    );

    let mistyped = config(
        "[vellev.shadow]\nsampel = 1\n[vellev.code]\nmodle = \"x\"\n",
        &[],
    );
    assert_eq!(
        mistyped.vellev.unknown_keys(),
        [
            ("[vellev.shadow]", "sampel".to_string()),
            ("[vellev.code]", "modle".to_string()),
        ]
    );
}

#[test]
fn code_search_is_on_unless_turned_off() {
    assert!(KnowledgeConfig::default().code_search_enabled);
    assert!(config("[knowledge]\n", &[]).knowledge.code_search_enabled);
    let off = config("[knowledge]\ncode_search_enabled = false\n", &[]);
    assert!(!off.knowledge.code_search_enabled);
    let agent = AgentConfig {
        knowledge: Some(Default::default()),
        ..Default::default()
    };
    assert!(
        !agent
            .effective_knowledge(&off.knowledge)
            .code_search_enabled
    );
}
