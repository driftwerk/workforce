//! Skill manifest parser — reads `Skill.toml` from WASM skill repositories.

use std::collections::HashSet;
use std::path::Path;

use eyre::{bail, Result, WrapErr};
use serde::{Deserialize, Serialize};

use crate::types::{Capability, CredentialScope};

/// The top-level manifest for a WASM skill, parsed from `Skill.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillManifest {
    /// Unique skill name (kebab-case, e.g., `weather-lookup`).
    pub name: String,
    /// Semantic version string (e.g., `0.2.0`).
    pub version: String,
    /// Human-readable description.
    pub description: String,
    /// Author or organisation.
    #[serde(default)]
    pub author: String,
    /// License identifier (e.g., `MIT`).
    #[serde(default)]
    pub license: String,
    /// Minimum workforce version this skill is compatible with.
    #[serde(default)]
    pub min_workforce_version: Option<String>,
    /// Capabilities the skill requires (http, vault, memory, nats, config).
    #[serde(default)]
    pub capabilities: HashSet<Capability>,
    /// Tools this skill provides.
    #[serde(default)]
    pub tools: Vec<ManifestTool>,
    /// Credentials the skill needs (stored in vault).
    #[serde(default)]
    pub credentials: Vec<ManifestCredential>,
    /// Configuration values the skill accepts.
    #[serde(default)]
    pub config_schema: Vec<ManifestConfigEntry>,
    /// Hostnames the skill is allowed to reach via `wf_http_fetch`. Each
    /// entry is either a literal hostname (`api.example.com`) or a
    /// `*.example.com` wildcard matching exactly one leading label. Empty
    /// means no outbound HTTP is permitted.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Platform tools the skill may invoke via `wf_tool_invoke`. Requires the
    /// `tools` capability; each invocation is additionally gated by the
    /// calling user's capabilities and the policy engine.
    #[serde(default)]
    pub permissions: Vec<ManifestPermission>,
}

/// A platform-tool permission declared in the manifest (`[[permissions]]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestPermission {
    /// Exact tool name (dot notation, e.g. `slack.post_message`). No
    /// wildcards — the user consents to a precise list.
    pub tool: String,
    /// Why the skill needs this tool; shown at grant time.
    pub description: String,
    /// When non-empty, every string argument that parses as an absolute URL
    /// must resolve to one of these hosts (same pattern grammar as
    /// `[network] allowed_hosts`).
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

/// Tool families no `[[permissions]]` declaration may name — enforced at
/// manifest parse time and re-checked at invoke time. The skill's own locker,
/// self-modification, admin/config surface, host execution, and orchestration
/// recursion stay out of reach regardless of consent.
pub const DENIED_TOOL_PREFIXES: &[&str] = &[
    "wasm_skill.",
    "vault.",
    "secret.",
    "system.",
    "settings",
    "mcp.",
    "agent.",
    "role.",
    "pairing.",
    "exec.",
    "code.",
    "coding.",
    "workflow",
    "subagent",
    "jobs",
];

/// Whether `tool` falls in a family that can never be granted to a skill.
pub fn is_denied_tool(tool: &str) -> bool {
    DENIED_TOOL_PREFIXES
        .iter()
        .any(|p| tool == p.trim_end_matches('.') || tool.starts_with(p))
}

/// A tool declared in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestTool {
    /// Tool name within the skill (e.g., `get_forecast`).
    /// Registered as `{skill_name}.{tool_name}` at runtime.
    pub name: String,
    /// Description shown to the LLM.
    pub description: String,
    /// Whether a human must approve every call before it executes. A skill
    /// may only ever ask for *more* approval than the hub's policy requires,
    /// never less, so setting this can add a prompt but can never remove one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub requires_approval: bool,
    /// Template the host renders into the approval prompt so the approver
    /// reads this call's scope rather than a parameter shape. See
    /// [`crate::approval`] for the grammar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_summary: Option<String>,
    /// JSON Schema for the tool's input parameters.
    #[serde(default = "default_object_schema")]
    pub input_schema: serde_json::Value,
    /// WASM handler function name. Defaults to the tool name.
    #[serde(default)]
    pub handler: Option<String>,
}

/// A credential requirement declared in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestCredential {
    /// Credential key (e.g., `api_key`). The host computes the full vault
    /// path from this name plus the skill name and (for `User` scope) the
    /// active user identity.
    pub name: String,
    /// What the credential is for.
    pub description: String,
    /// Whether the skill can function without it.
    #[serde(default = "default_true")]
    pub required: bool,
    /// Identity scope — `User` (default, per-caller) or `Shared`
    /// (cross-user). See `CredentialScope` docs.
    #[serde(default)]
    pub scope: CredentialScope,
}

/// A configuration entry declared in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestConfigEntry {
    /// Config key name.
    pub name: String,
    /// Value type (string, int, bool).
    #[serde(default = "default_string_type")]
    pub r#type: String,
    /// Description.
    pub description: String,
    /// Whether this config is required.
    #[serde(default)]
    pub required: bool,
    /// Default value.
    #[serde(default)]
    pub default: Option<String>,
}

fn default_object_schema() -> serde_json::Value {
    serde_json::json!({"type": "object", "properties": {}})
}

fn default_true() -> bool {
    true
}

fn default_string_type() -> String {
    "string".to_string()
}

/// Whether `host` matches any pattern in `allowed`. A pattern is either a
/// literal hostname (case-insensitive exact match) or `*.domain` matching
/// exactly one leading label — so `*.example.com` matches
/// `api.example.com` but not `deep.api.example.com` and not the apex
/// `example.com`. Empty `allowed` means nothing is allowed.
///
/// Used at runtime by `wf_http_fetch` and at parse time by
/// [`SkillManifest::host_allowed`] — a single canonical impl.
pub fn host_matches_allowlist(allowed: &[String], host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    allowed.iter().any(|pattern| {
        let pat = pattern.to_ascii_lowercase();
        match pat.strip_prefix("*.") {
            Some(suffix) => {
                let needle = format!(".{suffix}");
                host.find('.')
                    .map(|i| &host[i..])
                    .is_some_and(|rest| rest == needle && !host.starts_with('.'))
            }
            None => host == pat,
        }
    })
}

/// True if an IP must never be reachable from a skill's `wf_http_fetch`:
/// loopback, RFC1918 / CGNAT private space, link-local (incl. the cloud
/// metadata endpoint `169.254.169.254`), unspecified, broadcast, and the IPv6
/// equivalents. Used to block SSRF into the host / internal network (audit W1).
pub(crate) fn ip_is_disallowed(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.octets()[0] == 0
                // CGNAT 100.64.0.0/10
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // unique-local fc00::/7
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // link-local fe80::/10
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped (::ffff:a.b.c.d) — re-check the embedded v4
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|m| ip_is_disallowed(IpAddr::V4(m)))
        }
    }
}

/// Reject malformed `allowed_hosts` entries at parse time so misconfiguration
/// doesn't silently produce a no-match-everywhere rule.
fn validate_allowed_host(host: &str) -> Result<()> {
    if host.is_empty() {
        bail!("allowed_hosts entry cannot be empty");
    }
    // Defense-in-depth (audit W1): a literal internal/metadata IP must not be
    // allowlistable; the runtime resolve-check in `wf_http_fetch` is the real
    // boundary, but rejecting it here gives a clearer install-time error.
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if ip_is_disallowed(ip) {
            bail!(
                "allowed_hosts entry '{}' is an internal/metadata address and is not permitted",
                host
            );
        }
    }
    if host.contains("://") || host.contains('/') {
        bail!(
            "allowed_hosts entry '{}' must be a hostname only (no scheme or path)",
            host
        );
    }
    let star_count = host.matches('*').count();
    if star_count > 1 {
        bail!("allowed_hosts entry '{}' has more than one wildcard", host);
    }
    if star_count == 1 && !host.starts_with("*.") {
        bail!(
            "allowed_hosts entry '{}' wildcard must be a leading '*.' (e.g. '*.example.com')",
            host
        );
    }
    Ok(())
}

/// Raw TOML structure for deserialization. The `[skill]` table wraps the
/// core fields, and `[[tools]]` is a top-level array.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    skill: RawSkillSection,
    #[serde(default)]
    capabilities: Option<RawCapabilities>,
    #[serde(default)]
    network: Option<RawNetwork>,
    #[serde(default)]
    tools: Vec<ManifestTool>,
    #[serde(default)]
    credentials: Vec<ManifestCredential>,
    #[serde(default)]
    permissions: Vec<ManifestPermission>,
    #[serde(default)]
    config: Vec<ManifestConfigEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSkillSection {
    name: String,
    version: String,
    description: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    license: String,
    #[serde(default)]
    min_workforce_version: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCapabilities {
    #[serde(default)]
    http: bool,
    #[serde(default)]
    vault: bool,
    #[serde(default)]
    memory: bool,
    #[serde(default)]
    nats: bool,
    #[serde(default)]
    config: bool,
    #[serde(default)]
    artifacts: bool,
    #[serde(default)]
    tools: bool,
    #[serde(default)]
    store: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetwork {
    #[serde(default)]
    allowed_hosts: Vec<String>,
}

impl SkillManifest {
    /// Parse a manifest from TOML string content.
    pub fn from_toml(toml_str: &str) -> Result<Self> {
        let raw: RawManifest = toml::from_str(toml_str).wrap_err("Failed to parse Skill.toml")?;

        let caps = raw.capabilities.unwrap_or_default();
        let capabilities: HashSet<Capability> = [
            (caps.http, Capability::Http),
            (caps.vault, Capability::Vault),
            (caps.memory, Capability::Memory),
            (caps.nats, Capability::Nats),
            (caps.config, Capability::Config),
            (caps.artifacts, Capability::Artifacts),
            (caps.tools, Capability::Tools),
            (caps.store, Capability::Store),
        ]
        .into_iter()
        .filter(|(enabled, _)| *enabled)
        .map(|(_, cap)| cap)
        .collect();

        let allowed_hosts = raw.network.map(|n| n.allowed_hosts).unwrap_or_default();

        Ok(Self {
            name: raw.skill.name,
            version: raw.skill.version,
            description: raw.skill.description,
            author: raw.skill.author,
            license: raw.skill.license,
            min_workforce_version: raw.skill.min_workforce_version,
            capabilities,
            tools: raw.tools,
            credentials: raw.credentials,
            config_schema: raw.config,
            allowed_hosts,
            permissions: raw.permissions,
        })
    }

    /// Load a manifest from a `Skill.toml` file on disk.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("Failed to read {}", path.display()))?;
        Self::from_toml(&content)
    }

    /// Validate the manifest for correctness.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            bail!("Skill name cannot be empty");
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            bail!(
                "Skill name '{}' must be kebab-case (lowercase letters, digits, hyphens)",
                self.name
            );
        }
        if self.version.is_empty() {
            bail!("Skill version cannot be empty");
        }
        if semver::Version::parse(&self.version).is_err() {
            bail!("Skill version '{}' is not valid semver", self.version);
        }
        if self.tools.is_empty() {
            bail!("Skill must declare at least one tool");
        }
        for tool in &self.tools {
            if tool.name.is_empty() {
                bail!("Tool name cannot be empty");
            }
            if tool.description.is_empty() {
                bail!("Tool '{}' must have a description", tool.name);
            }
            if tool.requires_approval && tool.approval_summary.is_none() {
                bail!(
                    "Tool '{}' sets requires_approval but declares no approval_summary — \
                     an approver has to be told what the call would do",
                    tool.name
                );
            }
            if let Some(template) = &tool.approval_summary {
                crate::approval::validate_summary_template(template).wrap_err_with(|| {
                    format!("Tool '{}' has an invalid approval_summary", tool.name)
                })?;
            }
        }
        if let Some(ref min_ver) = self.min_workforce_version {
            if semver::Version::parse(min_ver).is_err() {
                bail!("min_workforce_version '{}' is not valid semver", min_ver);
            }
        }
        // A credential name doubles as the user's `cmd: secret` name and as a
        // single vault path segment in the consent flow, so it must satisfy the
        // named-secret grammar (lowercase letter first, then lowercase
        // letters/digits/underscores).
        for cred in &self.credentials {
            if let Err(e) = workforce_common::secret_marker::MarkerId::named(&cred.name) {
                bail!(
                    "Credential name '{}' is invalid: {e}. Users provision it via \
                     `cmd: secret {0} <value>` + `cmd: skill grant {1} {0}`, so it must \
                     be a lowercase identifier",
                    cred.name,
                    self.name
                );
            }
            if cred.description.is_empty() {
                bail!("Credential '{}' must have a description", cred.name);
            }
            // `cmd: skill grant <skill> tools` grants tool access; a credential
            // with that name would make the command ambiguous.
            if cred.name == "tools" {
                bail!("Credential name 'tools' is reserved for the tools grant");
            }
        }
        for host in &self.allowed_hosts {
            validate_allowed_host(host)?;
        }
        if !self.permissions.is_empty() && !self.capabilities.contains(&Capability::Tools) {
            bail!(
                "[[permissions]] declared without the `tools` capability — add \
                 `tools = true` under [capabilities]"
            );
        }
        for perm in &self.permissions {
            let valid_name = perm
                .tool
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase())
                && perm
                    .tool
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c));
            if !valid_name {
                bail!(
                    "Permission tool '{}' must be an exact dot-notation tool name \
                     (lowercase letters, digits, '.', '_', '-'); wildcards are not allowed",
                    perm.tool
                );
            }
            if is_denied_tool(&perm.tool) {
                bail!(
                    "Permission tool '{}' is in a family that can never be granted to a \
                     skill (denied prefixes: {})",
                    perm.tool,
                    DENIED_TOOL_PREFIXES.join(", ")
                );
            }
            if perm.description.is_empty() {
                bail!("Permission '{}' must have a description", perm.tool);
            }
            for host in &perm.allowed_hosts {
                validate_allowed_host(host)?;
            }
        }
        Ok(())
    }

    /// Check if the skill declares a specific capability.
    pub fn has_capability(&self, cap: Capability) -> bool {
        self.capabilities.contains(&cap)
    }

    /// Whether `host` matches any pattern in this manifest's
    /// `allowed_hosts`. See [`host_matches_allowlist`] for pattern
    /// semantics.
    pub fn host_allowed(&self, host: &str) -> bool {
        host_matches_allowlist(&self.allowed_hosts, host)
    }

    /// Get the handler function name for a tool.
    pub fn handler_for_tool(&self, tool_name: &str) -> Option<String> {
        self.tools
            .iter()
            .find(|t| t.name == tool_name)
            .map(|t| t.handler.as_deref().unwrap_or(tool_name).to_string())
    }

    /// Serialise the manifest to TOML in the canonical `Skill.toml` format.
    pub fn to_toml(&self) -> Result<String> {
        use serde::Serialize;

        #[derive(Serialize)]
        struct RawOut<'a> {
            skill: RawSkillOut<'a>,
            #[serde(skip_serializing_if = "Option::is_none")]
            capabilities: Option<RawCapsOut>,
            #[serde(skip_serializing_if = "Option::is_none")]
            network: Option<RawNetworkOut<'a>>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            tools: &'a Vec<ManifestTool>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            credentials: &'a Vec<ManifestCredential>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            permissions: &'a Vec<ManifestPermission>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            config: &'a Vec<ManifestConfigEntry>,
        }

        #[derive(Serialize)]
        struct RawNetworkOut<'a> {
            allowed_hosts: &'a Vec<String>,
        }

        #[derive(Serialize)]
        struct RawSkillOut<'a> {
            name: &'a str,
            version: &'a str,
            description: &'a str,
            #[serde(skip_serializing_if = "str::is_empty")]
            author: &'a str,
            #[serde(skip_serializing_if = "str::is_empty")]
            license: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            min_workforce_version: &'a Option<String>,
        }

        #[derive(Serialize)]
        struct RawCapsOut {
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            http: bool,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            vault: bool,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            memory: bool,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            nats: bool,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            config: bool,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            artifacts: bool,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            tools: bool,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            store: bool,
        }

        let caps = if self.capabilities.is_empty() {
            None
        } else {
            Some(RawCapsOut {
                http: self.capabilities.contains(&Capability::Http),
                vault: self.capabilities.contains(&Capability::Vault),
                memory: self.capabilities.contains(&Capability::Memory),
                nats: self.capabilities.contains(&Capability::Nats),
                config: self.capabilities.contains(&Capability::Config),
                artifacts: self.capabilities.contains(&Capability::Artifacts),
                tools: self.capabilities.contains(&Capability::Tools),
                store: self.capabilities.contains(&Capability::Store),
            })
        };

        let network = if self.allowed_hosts.is_empty() {
            None
        } else {
            Some(RawNetworkOut {
                allowed_hosts: &self.allowed_hosts,
            })
        };

        let raw = RawOut {
            skill: RawSkillOut {
                name: &self.name,
                version: &self.version,
                description: &self.description,
                author: &self.author,
                license: &self.license,
                min_workforce_version: &self.min_workforce_version,
            },
            capabilities: caps,
            network,
            tools: &self.tools,
            credentials: &self.credentials,
            permissions: &self.permissions,
            config: &self.config_schema,
        };

        toml::to_string_pretty(&raw).wrap_err("Failed to serialise manifest to TOML")
    }

    /// Get the fully-qualified tool names (prefixed with skill name).
    pub fn qualified_tool_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|t| format!("{}.{}", self.name, t.name))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_is_disallowed_blocks_internal_ranges() {
        use std::net::IpAddr;
        for s in [
            "127.0.0.1",
            "10.0.0.5",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata
            "100.64.0.1",      // CGNAT
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:169.254.169.254", // IPv4-mapped metadata
        ] {
            assert!(
                ip_is_disallowed(s.parse::<IpAddr>().unwrap()),
                "{s} must be blocked"
            );
        }
        for s in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(
                !ip_is_disallowed(s.parse::<IpAddr>().unwrap()),
                "{s} must be allowed"
            );
        }
    }

    const WEATHER_SKILL: &str = r#"
[skill]
name = "weather-lookup"
version = "0.2.0"
description = "Look up current weather for a location"
author = "driftwerk"
license = "MIT"
min_workforce_version = "0.1.0"

[capabilities]
http = true

[[tools]]
name = "get_forecast"
description = "Get the current weather forecast for a city"
input_schema = { type = "object", properties = { city = { type = "string", description = "City name" } }, required = ["city"] }

[[tools]]
name = "get_alerts"
description = "Get active weather alerts for a region"
handler = "fetch_alerts"
input_schema = { type = "object", properties = { region = { type = "string" } }, required = ["region"] }

[[credentials]]
name = "api_key"
description = "OpenWeatherMap API key"
required = true

[[config]]
name = "units"
type = "string"
description = "Temperature units (metric or imperial)"
required = false
default = "metric"
"#;

    const PERMISSIONED_SKILL: &str = r#"
[skill]
name = "enabler"
version = "0.1.0"
description = "Enable users"

[capabilities]
tools = true

[[tools]]
name = "enable"
description = "Enable a batch of users"

[[permissions]]
tool = "slack.post_message"
description = "Post the enablement summary"
allowed_hosts = []
"#;

    #[test]
    fn permissions_parse_and_round_trip_stably() {
        let manifest = SkillManifest::from_toml(PERMISSIONED_SKILL).unwrap();
        manifest.validate().unwrap();
        assert!(manifest.has_capability(Capability::Tools));
        assert_eq!(manifest.permissions.len(), 1);
        assert_eq!(manifest.permissions[0].tool, "slack.post_message");

        let canonical = manifest.to_toml().unwrap();
        let reparsed = SkillManifest::from_toml(&canonical).unwrap();
        assert_eq!(reparsed.to_toml().unwrap(), canonical);
        assert_eq!(reparsed.permissions[0].tool, "slack.post_message");
    }

    #[test]
    fn permissions_without_tools_capability_are_rejected() {
        let toml = PERMISSIONED_SKILL.replace("tools = true", "http = true");
        let err = SkillManifest::from_toml(&toml)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("tools"));
    }

    #[test]
    fn denied_tool_families_are_rejected_at_parse() {
        for denied in [
            "vault.set",
            "wasm_skill.build",
            "exec.run",
            "workflow",
            "system.logs",
        ] {
            let toml = PERMISSIONED_SKILL.replace("slack.post_message", denied);
            let err = SkillManifest::from_toml(&toml)
                .unwrap()
                .validate()
                .unwrap_err();
            assert!(
                err.to_string().contains("never be granted"),
                "{denied} must be rejected: {err}"
            );
        }
    }

    #[test]
    fn wildcard_permission_tools_are_rejected() {
        let toml = PERMISSIONED_SKILL.replace("slack.post_message", "slack.*");
        assert!(SkillManifest::from_toml(&toml).unwrap().validate().is_err());
    }

    #[test]
    fn credential_named_tools_is_reserved() {
        let toml = WEATHER_SKILL.replace("name = \"api_key\"", "name = \"tools\"");
        let err = SkillManifest::from_toml(&toml)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("reserved"));
    }

    #[test]
    fn is_denied_tool_covers_families_and_bare_names() {
        assert!(is_denied_tool("vault.set"));
        assert!(is_denied_tool("vault"));
        assert!(is_denied_tool("workflow"));
        assert!(is_denied_tool("settings"));
        assert!(is_denied_tool("wasm_skill.install"));
        assert!(!is_denied_tool("slack.post_message"));
        assert!(!is_denied_tool("http.request"));
    }

    #[test]
    fn parse_weather_skill() {
        let manifest = SkillManifest::from_toml(WEATHER_SKILL).unwrap();
        assert_eq!(manifest.name, "weather-lookup");
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(
            manifest.description,
            "Look up current weather for a location"
        );
        assert_eq!(manifest.author, "driftwerk");
        assert_eq!(manifest.license, "MIT");
        assert_eq!(manifest.min_workforce_version.as_deref(), Some("0.1.0"));
        assert!(manifest.has_capability(Capability::Http));
        assert!(!manifest.has_capability(Capability::Vault));
        assert_eq!(manifest.tools.len(), 2);
        assert_eq!(manifest.credentials.len(), 1);
        assert_eq!(manifest.config_schema.len(), 1);
    }

    #[test]
    fn handler_resolution() {
        let manifest = SkillManifest::from_toml(WEATHER_SKILL).unwrap();
        // Default handler = tool name
        assert_eq!(
            manifest.handler_for_tool("get_forecast"),
            Some("get_forecast".into())
        );
        // Explicit handler override
        assert_eq!(
            manifest.handler_for_tool("get_alerts"),
            Some("fetch_alerts".into())
        );
        // Missing tool
        assert_eq!(manifest.handler_for_tool("nonexistent"), None);
    }

    #[test]
    fn qualified_tool_names() {
        let manifest = SkillManifest::from_toml(WEATHER_SKILL).unwrap();
        let names = manifest.qualified_tool_names();
        assert_eq!(
            names,
            vec!["weather-lookup.get_forecast", "weather-lookup.get_alerts"]
        );
    }

    #[test]
    fn validation_passes() {
        let manifest = SkillManifest::from_toml(WEATHER_SKILL).unwrap();
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn validation_empty_name() {
        let toml = r#"
[skill]
name = ""
version = "1.0.0"
description = "bad"

[[tools]]
name = "test"
description = "test"
"#;
        let manifest = SkillManifest::from_toml(toml).unwrap();
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn validation_bad_version() {
        let toml = r#"
[skill]
name = "test"
version = "not-semver"
description = "bad"

[[tools]]
name = "test"
description = "test"
"#;
        let manifest = SkillManifest::from_toml(toml).unwrap();
        let err = manifest.validate().unwrap_err();
        assert!(err.to_string().contains("semver"));
    }

    #[test]
    fn validation_no_tools() {
        let toml = r#"
[skill]
name = "empty"
version = "1.0.0"
description = "no tools"
"#;
        let manifest = SkillManifest::from_toml(toml).unwrap();
        let err = manifest.validate().unwrap_err();
        assert!(err.to_string().contains("at least one tool"));
    }

    #[test]
    fn validation_non_kebab_name() {
        let toml = r#"
[skill]
name = "BadName"
version = "1.0.0"
description = "bad"

[[tools]]
name = "test"
description = "test"
"#;
        let manifest = SkillManifest::from_toml(toml).unwrap();
        let err = manifest.validate().unwrap_err();
        assert!(err.to_string().contains("kebab-case"));
    }

    #[test]
    fn minimal_manifest() {
        let toml = r#"
[skill]
name = "minimal"
version = "0.1.0"
description = "Bare minimum"

[[tools]]
name = "ping"
description = "Respond with pong"
"#;
        let manifest = SkillManifest::from_toml(toml).unwrap();
        assert!(manifest.validate().is_ok());
        assert!(manifest.capabilities.is_empty());
        assert!(manifest.credentials.is_empty());
        assert!(manifest.config_schema.is_empty());
        // Default input schema
        assert_eq!(manifest.tools[0].input_schema["type"], "object");
    }

    #[test]
    fn all_capabilities() {
        let toml = r#"
[skill]
name = "full"
version = "1.0.0"
description = "All caps"

[capabilities]
http = true
vault = true
memory = true
nats = true
config = true
artifacts = true
tools = true
store = true

[[tools]]
name = "test"
description = "test"
"#;
        let manifest = SkillManifest::from_toml(toml).unwrap();
        assert_eq!(manifest.capabilities.len(), Capability::ALL.len());
        for cap in Capability::ALL {
            assert!(manifest.has_capability(*cap));
        }
    }

    #[test]
    fn artifacts_capability_round_trips() {
        let toml = r#"
[skill]
name = "reshaper"
version = "1.0.0"
description = "Reshape an uploaded spreadsheet"

[capabilities]
artifacts = true

[[tools]]
name = "reshape"
description = "reshape"
"#;
        let manifest = SkillManifest::from_toml(toml).unwrap();
        assert!(manifest.has_capability(Capability::Artifacts));
        assert!(!manifest.has_capability(Capability::Http));
        let reparsed = SkillManifest::from_toml(&manifest.to_toml().unwrap()).unwrap();
        assert_eq!(reparsed.capabilities, manifest.capabilities);
    }

    #[test]
    fn unknown_top_level_field_rejected() {
        let toml = r#"
[skill]
name = "typo"
version = "1.0.0"
description = "typo guard"

[capabilites]
http = true

[[tools]]
name = "ping"
description = "ping"
"#;
        let err = format!("{:?}", SkillManifest::from_toml(toml).unwrap_err());
        assert!(
            err.contains("unknown") || err.contains("capabilites"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn unknown_credential_field_rejected() {
        let toml = r#"
[skill]
name = "typo"
version = "1.0.0"
description = "typo guard"

[[tools]]
name = "ping"
description = "ping"

[[credentials]]
name = "api_key"
description = "the key"
scoped = "user"
"#;
        let err = format!("{:?}", SkillManifest::from_toml(toml).unwrap_err());
        assert!(
            err.contains("unknown") || err.contains("scoped"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn credential_scope_defaults_to_user() {
        let toml = r#"
[skill]
name = "creds"
version = "1.0.0"
description = "scope defaults"

[[tools]]
name = "ping"
description = "ping"

[[credentials]]
name = "api_key"
description = "default scope"

[[credentials]]
name = "shared_token"
description = "shared bot token"
scope = "shared"
"#;
        let manifest = SkillManifest::from_toml(toml).unwrap();
        assert_eq!(manifest.credentials.len(), 2);
        assert_eq!(manifest.credentials[0].scope, CredentialScope::User);
        assert_eq!(manifest.credentials[1].scope, CredentialScope::Shared);
    }

    #[test]
    fn allowed_hosts_parses_and_validates() {
        let toml = r#"
[skill]
name = "net"
version = "1.0.0"
description = "network"

[network]
allowed_hosts = ["api.example.com", "*.orbit.example.com"]

[[tools]]
name = "ping"
description = "ping"
"#;
        let manifest = SkillManifest::from_toml(toml).unwrap();
        assert!(manifest.validate().is_ok());
        assert_eq!(manifest.allowed_hosts.len(), 2);
        assert!(manifest.host_allowed("api.example.com"));
        assert!(manifest.host_allowed("API.example.com"));
        assert!(manifest.host_allowed("foo.orbit.example.com"));
        assert!(!manifest.host_allowed("orbit.example.com")); // wildcard requires one label
        assert!(!manifest.host_allowed("deep.foo.orbit.example.com"));
        assert!(!manifest.host_allowed("attacker.com"));
    }

    #[test]
    fn allowed_hosts_rejects_scheme() {
        let toml = r#"
[skill]
name = "net"
version = "1.0.0"
description = "network"

[network]
allowed_hosts = ["https://api.example.com"]

[[tools]]
name = "ping"
description = "ping"
"#;
        let manifest = SkillManifest::from_toml(toml).unwrap();
        let err = manifest.validate().unwrap_err().to_string();
        assert!(err.contains("hostname only"), "got: {err}");
    }

    #[test]
    fn allowed_hosts_rejects_non_leading_wildcard() {
        let toml = r#"
[skill]
name = "net"
version = "1.0.0"
description = "network"

[network]
allowed_hosts = ["api.*.example.com"]

[[tools]]
name = "ping"
description = "ping"
"#;
        let manifest = SkillManifest::from_toml(toml).unwrap();
        let err = manifest.validate().unwrap_err().to_string();
        assert!(err.contains("leading"), "got: {err}");
    }

    #[test]
    fn manifest_round_trips_through_toml_with_new_fields() {
        let original = r#"
[skill]
name = "rt"
version = "1.0.0"
description = "round-trip"

[capabilities]
http = true

[network]
allowed_hosts = ["*.example.com"]

[[tools]]
name = "ping"
description = "ping"

[[credentials]]
name = "api_key"
description = "user key"
required = true

[[credentials]]
name = "shared_token"
description = "shared"
required = false
scope = "shared"
"#;
        let m = SkillManifest::from_toml(original).unwrap();
        let serialised = m.to_toml().unwrap();
        let m2 = SkillManifest::from_toml(&serialised).unwrap();
        assert_eq!(m2.allowed_hosts, vec!["*.example.com"]);
        assert_eq!(m2.credentials[0].scope, CredentialScope::User);
        assert_eq!(m2.credentials[1].scope, CredentialScope::Shared);
    }

    const GATED_SKILL: &str = r#"
[skill]
name = "record-keeper"
version = "0.1.0"
description = "Apply bulk changes to records"

[[tools]]
name = "apply_change"
description = "Apply an action to a batch of recipients"
requires_approval = true
approval_summary = "apply `{{action}}` to {{recipients|count}} recipient(s)"
"#;

    /// `grant_fingerprint` hashes `to_toml()`, and a legacy grant record is
    /// validated by exact build match. If the canonical TOML of an ungated
    /// skill gained a `requires_approval = false` line, every legacy grant on
    /// every hub would go stale at once.
    #[test]
    fn to_toml_omits_the_approval_fields_when_unset() {
        let manifest = SkillManifest::from_toml(WEATHER_SKILL).unwrap();
        let canonical = manifest.to_toml().unwrap();
        assert!(!canonical.contains("requires_approval"), "{canonical}");
        assert!(!canonical.contains("approval_summary"), "{canonical}");
    }

    #[test]
    fn a_gated_tool_round_trips_through_toml_stably() {
        let manifest = SkillManifest::from_toml(GATED_SKILL).unwrap();
        manifest.validate().unwrap();
        assert!(manifest.tools[0].requires_approval);

        let canonical = manifest.to_toml().unwrap();
        let reparsed = SkillManifest::from_toml(&canonical).unwrap();
        assert_eq!(reparsed.to_toml().unwrap(), canonical);
        assert!(reparsed.tools[0].requires_approval);
        assert_eq!(
            reparsed.tools[0].approval_summary.as_deref(),
            Some("apply `{{action}}` to {{recipients|count}} recipient(s)")
        );
    }

    #[test]
    fn requires_approval_without_a_summary_is_rejected() {
        let toml = GATED_SKILL.replace(
            "approval_summary = \"apply `{{action}}` to {{recipients|count}} recipient(s)\"\n",
            "",
        );
        let err = SkillManifest::from_toml(&toml)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("approval_summary"), "{err}");
    }

    #[test]
    fn an_unrenderable_approval_summary_is_rejected_at_install() {
        let toml = GATED_SKILL.replace("{{recipients|count}}", "{{recipients|upper}}");
        let err = SkillManifest::from_toml(&toml)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(format!("{err:#}").contains("apply_change"), "{err:#}");
    }

    /// A summary without the gate is allowed: it improves the prompt an
    /// operator rule raises without the skill asking for one.
    #[test]
    fn an_approval_summary_without_the_gate_is_allowed() {
        let toml = GATED_SKILL.replace("requires_approval = true\n", "");
        let manifest = SkillManifest::from_toml(&toml).unwrap();
        manifest.validate().unwrap();
        assert!(!manifest.tools[0].requires_approval);
        assert!(manifest.tools[0].approval_summary.is_some());
    }

    /// An old manifest parses unchanged on a new hub, and a new manifest is a
    /// hard parse error on an old hub — the fail-closed direction.
    #[test]
    fn the_approval_fields_default_off_and_unknown_tool_fields_still_fail() {
        let manifest = SkillManifest::from_toml(WEATHER_SKILL).unwrap();
        assert!(!manifest.tools[0].requires_approval);
        assert!(manifest.tools[0].approval_summary.is_none());

        let err = SkillManifest::from_toml(
            &GATED_SKILL.replace("requires_approval = true", "requires_approval_later = true"),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("requires_approval_later"),
            "{err:#}"
        );
    }
}
