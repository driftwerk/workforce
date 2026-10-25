//! Shadow mode's config (DW-676, PR 2).

use super::*;

/// What a config serves from, as everything that reads it decides it.
fn serving(
    config: &WorkforceConfig,
) -> (
    Vec<(&'static str, String)>,
    bool,
    bool,
    bool,
    Option<&'static str>,
) {
    (
        config.store_backends().to_vec(),
        config.memory.is_vellev(),
        config.artifacts_indexed_in_vellev(),
        config.code_vectors_in_vellev(),
        config.kept_in_vellev(),
    )
}

fn backends(memories: &str, artifacts: &str, code: &str) -> Vec<(&'static str, String)> {
    vec![
        ("memories", memories.to_string()),
        ("artifact_index", artifacts.to_string()),
        ("code_vectors", code.to_string()),
    ]
}

/// Every config the repo ships, and the defaults, serve from exactly what
/// they served from before shadow mode existed, and validate as before.
#[test]
fn every_shipped_config_serves_from_what_it_did_before() {
    let shipped = [
        (
            "no config",
            String::new(),
            backends("sqlite", "none", "none"),
        ),
        (
            "default_toml",
            WorkforceConfig::default_toml(),
            backends("sqlite", "none", "none"),
        ),
        (
            "workforce.toml",
            include_str!("../../../../workforce.toml").to_string(),
            backends("sqlite", "weaviate", "weaviate"),
        ),
        (
            "config/default.toml",
            include_str!("../../../../config/default.toml").to_string(),
            backends("sqlite", "none", "none"),
        ),
    ];
    for (name, toml, want) in shipped {
        let config = WorkforceConfig::from_toml(&toml).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            serving(&config),
            (want, false, false, false, None),
            "{name}"
        );
        config.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    assert_eq!(
        serving(&WorkforceConfig::default()),
        (
            backends("sqlite", "none", "none"),
            false,
            false,
            false,
            None
        )
    );
}

const WEAVIATE: &str = "[weaviate]\nenabled = true\n";
const VELLEV: &str = "[vellev]\napi_key = \"k\"\ntenant = \"acme\"\n";
const EMBEDDINGS: &str = "[embeddings]\nprovider = \"tei\"\nurl = \"http://embeddings:80\"\n";
const MEMORIES: &str = "[memory]\nbackend = \"shadow\"\n";
const ARTIFACTS: &str = "[artifacts]\nindex_backend = \"shadow\"\n";
const POSTGRES: &str = "[postgres]\nenabled = true\nurl = \"postgres://hub@db:5432/workforce\"\n";

fn refused(toml: &str) -> String {
    WorkforceConfig::from_toml(toml)
        .unwrap()
        .validate()
        .expect_err("refused")
        .to_string()
}

/// Why a config that sets `store` is refused: for artifacts when the
/// config is read; for memories where they are opened, the config itself
/// still loading.
fn refused_for(store: &str, toml: &str) -> String {
    let config = WorkforceConfig::from_toml(toml).unwrap();
    if store == MEMORIES {
        config
            .validate()
            .unwrap_or_else(|e| panic!("a process not opening memories was stopped: {e}"));
        config
            .check_memory_backend()
            .expect_err("refused where memories are opened")
            .to_string()
    } else {
        refused(toml)
    }
}

/// Shadowed, a store is still served by Weaviate: nothing that decides
/// where reads and writes go takes "shadow" for "vellev".
#[test]
fn a_shadowed_store_is_served_by_weaviate() {
    let both = format!("{MEMORIES}{ARTIFACTS}{WEAVIATE}{VELLEV}{EMBEDDINGS}");
    let config = WorkforceConfig::from_toml(&both).unwrap();
    assert!(!config.memory.is_vellev());
    assert!(!config.artifacts_indexed_in_vellev());
    assert!(!config.code_vectors_in_vellev());
    assert_eq!(config.kept_in_vellev(), None);
    assert_eq!(
        config.store_backends()[1],
        ("artifact_index", "weaviate".to_string())
    );
}

/// Weaviate serves while a store is shadowed, so it must be there.
#[test]
fn shadow_without_weaviate_is_refused_naming_the_fix() {
    for store in [MEMORIES, ARTIFACTS] {
        let refused = refused_for(store, &format!("{store}{VELLEV}{EMBEDDINGS}"));
        assert!(
            refused.contains("= \"shadow\" needs [weaviate] enabled = true"),
            "{refused}"
        );
    }
}

/// The mirror is vellev, so it needs the key, tenant and address that
/// "vellev" needs, said as "shadow".
#[test]
fn shadow_without_a_vellev_key_tenant_or_address_is_refused_naming_the_fix() {
    for (store, setting) in [
        (MEMORIES, "[memory] backend"),
        (ARTIFACTS, "[artifacts] index_backend"),
    ] {
        let base = format!("{store}{WEAVIATE}{EMBEDDINGS}");
        let keyless = refused_for(store, &format!("{base}[vellev]\ntenant = \"acme\"\n"));
        assert!(
            keyless.contains(&format!("{setting} = \"shadow\" needs [vellev] api_key")),
            "{keyless}"
        );
        let tenantless = refused_for(store, &format!("{base}[vellev]\napi_key = \"k\"\n"));
        assert!(
            tenantless.contains(&format!("{setting} = \"shadow\" needs [vellev] tenant")),
            "{tenantless}"
        );
        let nowhere = refused_for(store, &format!("{base}{VELLEV}url = \"vellev:18080\"\n"));
        assert!(nowhere.contains("[vellev] url must be"), "{nowhere}");
    }
}

/// Without the hub embedding memories, Weaviate and vellev would each embed
/// with their own model: not the same-model comparison memories are judged
/// by, and no query vector for the shadow to reuse.
#[test]
fn memories_shadowed_without_hub_embeddings_are_refused_naming_the_fix() {
    let refused = refused_for(MEMORIES, &format!("{MEMORIES}{WEAVIATE}{VELLEV}"));
    assert!(
        refused.contains("[memory] backend = \"shadow\" needs [embeddings]"),
        "{refused}"
    );
    // Artifacts are embedded by each side's own model whatever [embeddings] says.
    let artifacts = self::refused(&format!("{ARTIFACTS}{WEAVIATE}{VELLEV}"));
    assert!(!artifacts.contains("[embeddings]"), "{artifacts}");
}

/// The owner's decision of 2026-10-12: code vectors stay on Weaviate,
/// unshadowed, until vellev's memory use for them is cut.
#[test]
fn code_vectors_cannot_be_shadowed() {
    let refused = refused(&format!(
        "[knowledge]\nvector_backend = \"shadow\"\n{WEAVIATE}{VELLEV}"
    ));
    assert!(
        refused.contains("[knowledge] vector_backend cannot be \"shadow\""),
        "{refused}"
    );
    assert!(refused.contains("2026-10-12"), "{refused}");
    assert!(refused.contains("\"weaviate\""), "names the fix: {refused}");
}

/// An unknown value still names what may be chosen.
#[test]
fn an_unknown_backend_names_the_choices() {
    let artifacts = refused("[artifacts]\nindex_backend = \"auto\"");
    assert!(
        artifacts.contains("must be \"weaviate\" or \"vellev\", or \"shadow\""),
        "{artifacts}"
    );
    let memories = refused_for(MEMORIES, "[memory]\nbackend = \"mongodb\"");
    assert!(memories.contains("[memory] backend"), "{memories}");
    assert!(memories.contains("\"shadow\""), "{memories}");
}

/// With everything it needs, each store's shadow is accepted (artifacts
/// built in PR 5, memories in PR 8), and Weaviate still serves it.
#[test]
fn a_shadow_with_everything_it_needs_is_accepted_once_built() {
    let artifacts = WorkforceConfig::from_toml(&format!(
        "{ARTIFACTS}{WEAVIATE}{VELLEV}{EMBEDDINGS}{POSTGRES}"
    ))
    .unwrap();
    artifacts
        .validate()
        .unwrap_or_else(|e| panic!("the artifact shadow, with everything it needs: {e}"));
    assert!(!artifacts.artifacts_indexed_in_vellev(), "served by vellev");
    assert_eq!(
        artifacts.store_backends()[1],
        ("artifact_index", "weaviate".to_string())
    );
    let memories = WorkforceConfig::from_toml(&format!(
        "{MEMORIES}{WEAVIATE}{VELLEV}{EMBEDDINGS}{POSTGRES}"
    ))
    .unwrap();
    memories
        .validate()
        .unwrap_or_else(|e| panic!("the memories' shadow, with everything it needs: {e}"));
    memories
        .check_memory_backend()
        .unwrap_or_else(|e| panic!("the memories' shadow, where memories are opened: {e}"));
    assert!(!memories.memory.is_vellev(), "served by Weaviate");
}

/// An unknown memory backend is told the choices, "shadow" among them as
/// one that is built.
#[test]
fn the_memory_backends_named_are_all_built() {
    let memories = refused_for(MEMORIES, "[memory]\nbackend = \"mongodb\"");
    assert!(memories.contains("\"shadow\""), "{memories}");
    assert!(!memories.contains("not built"), "{memories}");
}

// ---- [vellev.shadow] -------------------------------------------------------

/// Unset, each store's shadow runs as design revision 2 says: every
/// question sampled, four at a time, memories given 2 s and artifacts 5 s,
/// the breaker of DW-620's artifact search (3 in a row, 30 s), compared
/// to depth 10 with p 0.9; and nothing paused.
#[test]
fn unset_shadow_settings_are_the_designs() {
    let config = WorkforceConfig::from_toml("").unwrap();
    assert!(!config.vellev.shadow.paused);
    let want = |deadline_ms| crate::shadow::ShadowSettings {
        sample: 1.0,
        permits: 4,
        deadline: std::time::Duration::from_millis(deadline_ms),
        trip_after: 3,
        rest: std::time::Duration::from_secs(30),
        k: 10,
        rbo_p: 0.9,
    };
    assert_eq!(
        config
            .vellev
            .shadow
            .settings(ShadowedStore::Memories)
            .unwrap(),
        want(2_000)
    );
    assert_eq!(
        config
            .vellev
            .shadow
            .settings(ShadowedStore::Artifacts)
            .unwrap(),
        want(5_000)
    );
    for store in [ShadowedStore::Memories, ShadowedStore::Artifacts] {
        let settings = config.vellev.shadow.settings(store).unwrap();
        crate::shadow::Shadow::new(
            "test",
            settings,
            std::sync::Arc::new(crate::shadow::TracingSink),
        )
        .expect("the kernel takes the defaults");
    }
}

/// Each store's values are its own; an unset one keeps its default.
#[test]
fn shadow_settings_are_read_per_store() {
    let config = WorkforceConfig::from_toml(
        "[vellev.shadow]\npaused = true\n[vellev.shadow.memories]\nsample = 0.5\ndeadline_ms = 750\n[vellev.shadow.artifacts]\npermits = 2\ntrip_after = 5\nrest_secs = 60\nk = 5\nrbo_p = 0.8\n",
    )
    .unwrap();
    assert!(config.vellev.shadow.paused);
    let memories = config
        .vellev
        .shadow
        .settings(ShadowedStore::Memories)
        .unwrap();
    assert_eq!(
        (memories.sample, memories.deadline, memories.permits),
        (0.5, std::time::Duration::from_millis(750), 4)
    );
    let artifacts = config
        .vellev
        .shadow
        .settings(ShadowedStore::Artifacts)
        .unwrap();
    assert_eq!(
        (
            artifacts.permits,
            artifacts.trip_after,
            artifacts.rest,
            artifacts.k,
            artifacts.rbo_p,
            artifacts.deadline
        ),
        (
            2,
            5,
            std::time::Duration::from_secs(60),
            5,
            0.8,
            std::time::Duration::from_secs(5)
        )
    );
    config.validate().unwrap();
}

/// A value the kernel cannot run with is refused when the config is read,
/// naming its table and what is wrong, whether or not a store is shadowed.
#[test]
fn shadow_values_that_cannot_be_used_are_refused_by_name() {
    for (table, value, says) in [
        ("memories", "sample = 1.5", "sample"),
        ("memories", "permits = 0", "permits"),
        ("artifacts", "deadline_ms = 0", "deadline"),
        ("artifacts", "trip_after = 0", "breaker"),
        ("memories", "k = 0", "depth"),
        ("artifacts", "rbo_p = 1.0", "p "),
    ] {
        let refused = refused(&format!("[vellev.shadow.{table}]\n{value}\n"));
        assert!(
            refused.contains(&format!("[vellev.shadow.{table}]")) && refused.contains(says),
            "{value}: {refused}"
        );
    }
}

/// A key this hub does not know (a newer hub's, or mistyped) is warned of
/// by its whole name and otherwise ignored: the config still loads and
/// validates, so a rollback to an older hub still starts.
#[test]
fn an_unknown_shadow_key_is_warned_of_by_name_and_the_config_still_loads() {
    for (toml, table, key) in [
        (
            "[vellev.shadow]\npause = true\n",
            "[vellev.shadow]",
            "pause",
        ),
        (
            "[vellev.shadow.memories]\nsample_rate = 0.5\n",
            "[vellev.shadow.memories]",
            "sample_rate",
        ),
        (
            "[vellev.shadow.artifacts]\nwatch_secs = 30\n",
            "[vellev.shadow.artifacts]",
            "watch_secs",
        ),
        (
            "[vellev.shadow.judge]\nenable = true\n",
            "[vellev.shadow.judge]",
            "enable",
        ),
        (
            "[vellev.shadow.known_items]\nitem = 10\n",
            "[vellev.shadow.known_items]",
            "item",
        ),
        // A misspelled table is as loud as a misspelled key.
        (
            "[vellev.shadow.memory]\nsample = 0.5\n",
            "[vellev.shadow]",
            "memory",
        ),
    ] {
        let config = WorkforceConfig::from_toml(toml).unwrap_or_else(|e| panic!("{toml}: {e}"));
        assert_eq!(
            config.vellev.shadow.unknown_keys(),
            vec![(table, key.to_string())]
        );
        let said = said_while(|| config.validate().unwrap_or_else(|e| panic!("{toml}: {e}")));
        assert!(
            said.contains("WARN") && said.contains(table) && said.contains(&format!("\"{key}\"")),
            "{toml}: {said}"
        );
    }
    // A known key is not warned of.
    let config = WorkforceConfig::from_toml("[vellev.shadow.memories]\nsample = 0.5\n").unwrap();
    assert!(config.vellev.shadow.unknown_keys().is_empty());
    let said = said_while(|| config.validate().unwrap());
    assert!(!said.contains("shadow"), "{said}");
}

/// What is logged while `f` runs on this thread.
fn said_while(f: impl FnOnce()) -> String {
    #[derive(Clone, Default)]
    struct Said(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Said {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let said = Said::default();
    let writer = said.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let bytes = said.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

/// `[memory] backend` stops only what opens memories: a process that loads
/// the config and never opens them (the CLI, a worker, edge, local-mcp)
/// starts with a value it does not use, as before shadow mode; the hub,
/// which opens them, is refused (`resolve_memory_store`, pinned in the
/// hub's own tests).
#[test]
fn a_bad_memory_backend_stops_only_what_opens_memories() {
    for toml in [
        "[memory]\nbackend = \"mongodb\"\n",
        "[memory]\nbackend = \"shadow\"\n",
    ] {
        let config = WorkforceConfig::from_toml(toml).unwrap();
        config.validate().unwrap_or_else(|e| {
            panic!("{toml} stopped a process that does not open memories: {e}")
        });
        assert!(config.check_memory_backend().is_err(), "{toml}");
    }
}

/// Every `[memory] backend` value accepted before shadow mode is accepted
/// still, both when the config is read and where memories are opened.
#[test]
fn every_memory_backend_accepted_before_is_accepted_still() {
    for fine in ["sqlite", "", "weaviate", "vellev", "in_memory"] {
        let mut toml = format!("[memory]\nbackend = \"{fine}\"\n");
        if fine == "vellev" {
            toml.push_str(VELLEV);
        }
        let config = WorkforceConfig::from_toml(&toml).unwrap();
        config
            .validate()
            .unwrap_or_else(|e| panic!("{fine:?} when read: {e}"));
        config
            .check_memory_backend()
            .unwrap_or_else(|e| panic!("{fine:?} where memories are opened: {e}"));
    }
}

/// The two answers' comparisons are kept in Postgres, so a shadow without
/// it is refused, naming the fix.
#[test]
fn shadow_without_postgres_is_refused_naming_the_fix() {
    for (store, setting) in [
        (MEMORIES, "[memory] backend"),
        (ARTIFACTS, "[artifacts] index_backend"),
    ] {
        for postgres in ["", "[postgres]\nenabled = false\n"] {
            let refused = refused_for(
                store,
                &format!("{store}{WEAVIATE}{VELLEV}{EMBEDDINGS}{postgres}"),
            );
            assert!(
                refused.contains(&format!(
                    "{setting} = \"shadow\" needs [postgres] enabled = true"
                )),
                "{postgres:?}: {refused}"
            );
        }
    }
}

/// Memories' replica is reconciled every 15 minutes unless set; a value
/// outside a minute to a day is refused by name, when the config is read.
#[test]
fn the_memory_reconcile_interval_is_15_minutes_unless_set_within_bounds() {
    let read = |toml: &str| {
        WorkforceConfig::from_toml(toml)
            .unwrap()
            .vellev
            .shadow
            .memory_reconcile()
    };
    assert_eq!(read("").unwrap(), std::time::Duration::from_secs(900));
    assert_eq!(
        read("[vellev.shadow]\nmemory_reconcile_secs = 60\n").unwrap(),
        std::time::Duration::from_secs(60)
    );
    for secs in [0, 59, 86_401] {
        let refused = refused(&format!(
            "[vellev.shadow]\nmemory_reconcile_secs = {secs}\n"
        ));
        assert!(
            refused.contains("[vellev.shadow] memory_reconcile_secs")
                && refused.contains("60 to 86400"),
            "{secs}: {refused}"
        );
    }
}

/// The known-item runs draw 1,000 memories a night, and 250 filed under no
/// identity, from 02:00 to 06:00 UTC, a quarter second apart, from seed 0,
/// unless set; 0 items runs none; a value out of bounds, a window that is
/// none, or a run so long that it would be taken for abandoned is refused
/// by name, when the config is read.
#[test]
fn known_item_runs_have_the_plans_defaults_and_refuse_values_out_of_bounds() {
    let read = |toml: &str| {
        WorkforceConfig::from_toml(toml)
            .unwrap()
            .vellev
            .shadow
            .known_items()
    };
    assert_eq!(
        read("").unwrap(),
        KnownItemSettings {
            items: 1000,
            unscoped_items: 250,
            seed: 0,
            hour_utc: 2,
            until_hour_utc: 6,
            pace: std::time::Duration::from_millis(250),
        }
    );
    assert_eq!(
        read("[vellev.shadow.known_items]\nitems = 0\nunscoped_items = 0\nseed = 42\nhour_utc = 22\nuntil_hour_utc = 24\npace_ms = 0\n")
            .unwrap(),
        KnownItemSettings {
            items: 0,
            unscoped_items: 0,
            seed: 42,
            hour_utc: 22,
            until_hour_utc: 24,
            pace: std::time::Duration::ZERO,
        }
    );
    for (toml, named) in [
        ("items = 5001", "items"),
        ("unscoped_items = 5001", "unscoped_items"),
        ("hour_utc = 24", "hour_utc"),
        ("until_hour_utc = 25", "until_hour_utc"),
        ("hour_utc = 6\nuntil_hour_utc = 6", "until_hour_utc"),
        ("pace_ms = 10001", "pace_ms"),
        // 5,000 and 5,000 items, 2.2 s apart, past six hours.
        (
            "items = 5000\nunscoped_items = 5000\npace_ms = 2200",
            "pace_ms",
        ),
    ] {
        let refused = refused(&format!("[vellev.shadow.known_items]\n{toml}\n"));
        assert!(
            refused.contains(&format!("[vellev.shadow.known_items] {named}")),
            "{toml}: {refused}"
        );
    }
    // Just within: 4,000 items 5.3 s apart is 5.9 hours.
    assert!(read(
        "[vellev.shadow.known_items]\nitems = 4000\nunscoped_items = 0\npace_ms = 5300\n"
    )
    .is_ok());
}

/// The judge is off unless set; on, it judges 200 queries a day unless
/// set; a cap outside 1 to 1000 is refused by name, on or off.
#[test]
fn the_judge_is_off_unless_set_and_its_cap_is_bounded() {
    let read = |toml: &str| {
        WorkforceConfig::from_toml(toml)
            .unwrap()
            .vellev
            .shadow
            .judge_cap()
    };
    assert_eq!(read("").unwrap(), None);
    assert_eq!(
        read("[vellev.shadow.judge]\ndaily_cap = 50\n").unwrap(),
        None
    );
    assert_eq!(
        read("[vellev.shadow.judge]\nenabled = true\n").unwrap(),
        Some(200)
    );
    assert_eq!(
        read("[vellev.shadow.judge]\nenabled = true\ndaily_cap = 1000\n").unwrap(),
        Some(1000)
    );
    for cap in [0, 1001] {
        let refused = refused(&format!("[vellev.shadow.judge]\ndaily_cap = {cap}\n"));
        assert!(
            refused.contains("[vellev.shadow.judge] daily_cap") && refused.contains("1 to 1000"),
            "{cap}: {refused}"
        );
    }
}
