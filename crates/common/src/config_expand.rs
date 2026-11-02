//! Pre-deserialisation expansion for TOML config values.
//!
//! Supports `${VAR}` / `${VAR:-default}` (process env) and `${vault:path}`
//! (via a [`VaultResolver`]). The main `workforce.toml` loader applies env
//! expansion as a blanket pass; vault references are resolved per-field at
//! access time because the vault is itself configured by the same file.
//! The per-skill `config.toml` loader in `workforce-skills::config_loader`
//! shares these routines.
//!
//! The secrets a supervisor hands the hub on a descriptor
//! ([`crate::inherited_secrets`]) are never part of that blanket pass. Only
//! the few base-file fields in [`INHERITED_SECRET_FIELDS`] read them, each
//! its own one ([`expand_inherited_secret_fields`]), before the overlay is
//! merged. The overlay's agent entries, which agents write, are never
//! expanded at all, so an agent can't read a secret or the hub's
//! environment back through an agent it made.

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
    expand_vars_with(input, &|name| std::env::var(name).ok())
}

/// Expand `${VAR}` / `${VAR:-default}`, reading each variable through
/// `lookup`.
fn expand_vars_with(input: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    ENV_VAR_RE
        .replace_all(input, |caps: &regex::Captures| match lookup(&caps[1]) {
            Some(val) => val,
            None => caps
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
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
    expand_vars_in_table_with(table, &|name| std::env::var(name).ok());
}

/// The base-config fields (`section.key`) that may read a secret inherited
/// on a descriptor, each with the one variable it may read: the ones a
/// supervisor (Desk Helper) fills with them. A field reads no other
/// inherited secret, so a rewritten base file can't send the vault key, or
/// one secret, anywhere else.
pub const INHERITED_SECRET_FIELDS: &[(&str, &str, &str)] = &[
    ("postgres", "password", "DESK_POSTGRES_PASSWORD"),
    ("vellev", "api_key", "DESK_MEMORY_KEY"),
    ("desk", "pty_holder_token", "WORKFORCE_GATEWAY_TOKEN"),
];

/// Expand, in the base file's [`INHERITED_SECRET_FIELDS`], each field's own
/// `${VAR}`, reading the inherited secrets first, then the environment
/// ([`crate::inherited_secrets::var`]). Any other placeholder there is left
/// for the env-only pass. Run it on the base file alone, before the overlay
/// is merged over it.
pub fn expand_inherited_secret_fields(base: &mut toml::Table) {
    expand_fields_with(base, &crate::inherited_secrets::var);
}

fn expand_fields_with(table: &mut toml::Table, lookup: &dyn Fn(&str) -> Option<String>) {
    for (section, key, var) in INHERITED_SECRET_FIELDS {
        if let Some(toml::Value::String(s)) = table
            .get_mut(*section)
            .and_then(toml::Value::as_table_mut)
            .and_then(|t| t.get_mut(*key))
        {
            *s = ENV_VAR_RE
                .replace_all(s, |caps: &regex::Captures| {
                    if &caps[1] != *var {
                        return caps[0].to_string();
                    }
                    lookup(var).unwrap_or_else(|| {
                        caps.get(2)
                            .map(|m| m.as_str().to_string())
                            .unwrap_or_default()
                    })
                })
                .to_string();
        }
    }
}

/// [`expand_env_vars_in_table`] on every top-level key of `table` but
/// `literal`, whose values are left as written.
pub fn expand_env_vars_in_table_except(table: &mut toml::Table, literal: &[&str]) {
    for (key, value) in table.iter_mut() {
        if !literal.contains(&key.as_str()) {
            expand_vars_in_value_with(value, &|name| std::env::var(name).ok());
        }
    }
}

fn expand_vars_in_table_with(table: &mut toml::Table, lookup: &dyn Fn(&str) -> Option<String>) {
    for (_key, value) in table.iter_mut() {
        expand_vars_in_value_with(value, lookup);
    }
}

fn expand_vars_in_value_with(value: &mut toml::Value, lookup: &dyn Fn(&str) -> Option<String>) {
    match value {
        toml::Value::String(s) => *s = expand_vars_with(s, lookup),
        toml::Value::Table(t) => expand_vars_in_table_with(t, lookup),
        toml::Value::Array(arr) => {
            for item in arr.iter_mut() {
                expand_vars_in_value_with(item, lookup);
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

    /// Each secret field takes its own `${VAR}` from the secrets inherited
    /// on a descriptor first, then the environment; any other placeholder in
    /// it, and every other field, is left for the env-only pass.
    #[cfg(unix)]
    #[test]
    fn only_the_secret_fields_expand_from_the_inherited_secrets() {
        use std::io::Write;
        let (reader, mut writer) = std::io::pipe().unwrap();
        let payload =
            br#"{"DESK_POSTGRES_PASSWORD":"pw-fixture","WORKFORCE_VAULT_KEY":"vk-fixture"}"#;
        writer
            .write_all(&(payload.len() as u32).to_be_bytes())
            .unwrap();
        writer.write_all(payload).unwrap();
        drop(writer);
        let secrets = crate::inherited_secrets::InheritedSecrets::read(reader.into()).unwrap();
        let mut table: toml::Table = r#"
            [postgres]
            password = "${DESK_POSTGRES_PASSWORD}"
            other = "${DESK_POSTGRES_PASSWORD}"
            [vellev]
            api_key = "${WORKFORCE_VAULT_KEY}"
            [desk]
            pty_holder_token = "${CFG_EXPAND_T_FD_UNSET:-x}${WORKFORCE_GATEWAY_TOKEN:-fallback}"
            [agent]
            prompt = "${DESK_POSTGRES_PASSWORD}"
        "#
        .parse()
        .unwrap();
        expand_fields_with(&mut table, &|name| {
            crate::inherited_secrets::var_in(Some(&secrets), name)
        });
        assert_eq!(table["postgres"]["password"].as_str(), Some("pw-fixture"));
        // Not its own variable: left for the env-only pass.
        assert_eq!(
            table["vellev"]["api_key"].as_str(),
            Some("${WORKFORCE_VAULT_KEY}")
        );
        assert_eq!(
            table["desk"]["pty_holder_token"].as_str(),
            Some("${CFG_EXPAND_T_FD_UNSET:-x}fallback")
        );
        assert_eq!(
            table["postgres"]["other"].as_str(),
            Some("${DESK_POSTGRES_PASSWORD}")
        );
        assert_eq!(
            table["agent"]["prompt"].as_str(),
            Some("${DESK_POSTGRES_PASSWORD}")
        );
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
