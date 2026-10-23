//! Pre-deserialisation expansion for TOML config values.
//!
//! Supports `${VAR}` / `${VAR:-default}` (process env) and `${vault:path}`
//! (via a [`VaultResolver`]). The main `workforce.toml` loader applies env
//! expansion as a blanket pass; vault references are resolved per-field at
//! access time because the vault is itself configured by the same file.
//! The per-skill `config.toml` loader in `workforce-skills::config_loader`
//! shares these routines.

use std::sync::LazyLock;

use regex::Regex;

use crate::settings::VaultResolver;

static ENV_VAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}").unwrap());

static VAULT_REF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{vault:([a-zA-Z0-9_.]+)\}").unwrap());

/// Expand `${VAR}` / `${VAR:-default}` from the process env. Missing
/// variables with no default produce an empty string.
pub fn expand_env_vars(input: &str) -> String {
    ENV_VAR_RE
        .replace_all(input, |caps: &regex::Captures| {
            match std::env::var(&caps[1]) {
                Ok(val) => val,
                Err(_) => caps
                    .get(2)
                    .map(|m| m.as_str().to_string())
                    .unwrap_or_default(),
            }
        })
        .to_string()
}

/// Expand `${vault:path}` patterns. Unresolved paths produce an empty string.
pub fn expand_vault_refs(input: &str, vault: &dyn VaultResolver) -> String {
    VAULT_REF_RE
        .replace_all(input, |caps: &regex::Captures| {
            vault.resolve(&caps[1]).unwrap_or_default()
        })
        .to_string()
}

/// Recursively expand `${VAR}` in every string value of a TOML table.
/// Non-string scalars are untouched.
pub fn expand_env_vars_in_table(table: &mut toml::Table) {
    for (_key, value) in table.iter_mut() {
        expand_env_vars_in_value(value);
    }
}

fn expand_env_vars_in_value(value: &mut toml::Value) {
    match value {
        toml::Value::String(s) => *s = expand_env_vars(s),
        toml::Value::Table(t) => expand_env_vars_in_table(t),
        toml::Value::Array(arr) => {
            for item in arr.iter_mut() {
                expand_env_vars_in_value(item);
            }
        }
        _ => {}
    }
}

/// Recursively expand `${vault:path}` in every string value of a TOML table.
pub fn expand_vault_refs_in_table(table: &mut toml::Table, vault: &dyn VaultResolver) {
    for (_key, value) in table.iter_mut() {
        expand_vault_refs_in_value(value, vault);
    }
}

fn expand_vault_refs_in_value(value: &mut toml::Value, vault: &dyn VaultResolver) {
    match value {
        toml::Value::String(s) => *s = expand_vault_refs(s, vault),
        toml::Value::Table(t) => expand_vault_refs_in_table(t, vault),
        toml::Value::Array(arr) => {
            for item in arr.iter_mut() {
                expand_vault_refs_in_value(item, vault);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_simple_var() {
        std::env::set_var("CFG_EXPAND_T_SIMPLE", "hello");
        assert_eq!(expand_env_vars("${CFG_EXPAND_T_SIMPLE}"), "hello");
        std::env::remove_var("CFG_EXPAND_T_SIMPLE");
    }

    #[test]
    fn expand_var_with_default_uses_default_when_unset() {
        std::env::remove_var("CFG_EXPAND_T_DEFAULT_UNSET");
        assert_eq!(
            expand_env_vars("${CFG_EXPAND_T_DEFAULT_UNSET:-fallback}"),
            "fallback"
        );
    }

    #[test]
    fn expand_var_with_default_ignores_default_when_set() {
        std::env::set_var("CFG_EXPAND_T_DEFAULT_SET", "real");
        assert_eq!(
            expand_env_vars("${CFG_EXPAND_T_DEFAULT_SET:-fallback}"),
            "real"
        );
        std::env::remove_var("CFG_EXPAND_T_DEFAULT_SET");
    }

    #[test]
    fn expand_missing_var_no_default_is_empty() {
        std::env::remove_var("CFG_EXPAND_T_MISSING");
        assert_eq!(expand_env_vars("${CFG_EXPAND_T_MISSING}"), "");
    }

    #[test]
    fn expand_with_surrounding_text() {
        std::env::set_var("CFG_EXPAND_T_SURR", "abc");
        assert_eq!(
            expand_env_vars("prefix ${CFG_EXPAND_T_SURR} suffix"),
            "prefix abc suffix"
        );
        std::env::remove_var("CFG_EXPAND_T_SURR");
    }

    #[test]
    fn expand_multiple_in_one_string() {
        std::env::set_var("CFG_EXPAND_T_MULTI_A", "alpha");
        std::env::set_var("CFG_EXPAND_T_MULTI_B", "beta");
        assert_eq!(
            expand_env_vars("${CFG_EXPAND_T_MULTI_A}-${CFG_EXPAND_T_MULTI_B}"),
            "alpha-beta"
        );
        std::env::remove_var("CFG_EXPAND_T_MULTI_A");
        std::env::remove_var("CFG_EXPAND_T_MULTI_B");
    }

    #[test]
    fn expand_passthrough_no_placeholder() {
        assert_eq!(expand_env_vars("plain text"), "plain text");
        assert_eq!(expand_env_vars(""), "");
    }

    #[test]
    fn expand_table_recursive_includes_arrays() {
        std::env::set_var("CFG_EXPAND_T_REC", "filled");
        let mut table: toml::Table = r#"
            top = "${CFG_EXPAND_T_REC}"
            [inner]
            key = "prefix-${CFG_EXPAND_T_REC}"
            list = ["${CFG_EXPAND_T_REC}", "literal"]
        "#
        .parse()
        .unwrap();
        expand_env_vars_in_table(&mut table);
        assert_eq!(table["top"].as_str().unwrap(), "filled");
        assert_eq!(table["inner"]["key"].as_str().unwrap(), "prefix-filled");
        let list = table["inner"]["list"].as_array().unwrap();
        assert_eq!(list[0].as_str().unwrap(), "filled");
        assert_eq!(list[1].as_str().unwrap(), "literal");
        std::env::remove_var("CFG_EXPAND_T_REC");
    }

    #[test]
    fn expand_does_not_touch_non_string_scalars() {
        let mut table: toml::Table = r#"
            int_val = 42
            bool_val = true
            float_val = 1.5
        "#
        .parse()
        .unwrap();
        expand_env_vars_in_table(&mut table);
        assert_eq!(table["int_val"].as_integer().unwrap(), 42);
        assert!(table["bool_val"].as_bool().unwrap());
        assert!((table["float_val"].as_float().unwrap() - 1.5).abs() < f64::EPSILON);
    }

    struct MockVault {
        secrets: std::collections::HashMap<String, String>,
    }

    impl VaultResolver for MockVault {
        fn resolve(&self, path: &str) -> Option<String> {
            self.secrets.get(path).cloned()
        }
    }

    #[test]
    fn vault_expand_simple() {
        let mut s = std::collections::HashMap::new();
        s.insert("a.b".to_string(), "v".to_string());
        let vault = MockVault { secrets: s };
        assert_eq!(expand_vault_refs("${vault:a.b}", &vault), "v");
    }

    #[test]
    fn vault_expand_does_not_match_env_pattern() {
        let vault = MockVault {
            secrets: Default::default(),
        };
        // env-style placeholders are passed through by vault expansion
        assert_eq!(expand_vault_refs("${MY_VAR}", &vault), "${MY_VAR}");
    }
}
