use serde::{Deserialize, Serialize};

/// Configuration for a NATS leaf node connection.
///
/// Leaf nodes connect local machines (laptops, workstations) to the
/// hub's NATS cluster, enabling distributed agent communication
/// without full cluster membership.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeafNodeConfig {
    /// Hub NATS server address.
    pub hub_url: String,
    /// Local node ID.
    pub node_id: String,
    /// TLS certificate path (for mTLS).
    #[serde(default)]
    pub tls_cert: Option<String>,
    /// TLS key path.
    #[serde(default)]
    pub tls_key: Option<String>,
    /// CA certificate path.
    #[serde(default)]
    pub tls_ca: Option<String>,
    /// Credentials file path.
    #[serde(default)]
    pub credentials: Option<String>,
    /// Local NATS listen port.
    #[serde(default = "default_port")]
    pub local_port: u16,
    /// Heartbeat interval in seconds.
    #[serde(default = "default_heartbeat")]
    pub heartbeat_interval_secs: u64,
    /// Reconnect attempts before giving up.
    #[serde(default = "default_reconnect")]
    pub max_reconnect_attempts: u32,
}

impl LeafNodeConfig {
    /// Generate the NATS leaf node configuration file content.
    pub fn to_nats_config(&self) -> String {
        let mut config = String::new();

        config.push_str(&format!("# Leaf node config for {}\n", self.node_id));
        config.push_str(&format!("server_name: {}\n\n", self.node_id));

        config.push_str(&format!("port: {}\n\n", self.local_port));

        config.push_str("leafnodes {\n");
        config.push_str("  remotes [\n");
        config.push_str("    {\n");
        config.push_str(&format!("      url: \"{}\"\n", self.hub_url));

        if let Some(ref creds) = self.credentials {
            config.push_str(&format!("      credentials: \"{}\"\n", creds));
        }

        if self.tls_cert.is_some() {
            config.push_str("      tls {\n");
            if let Some(ref cert) = self.tls_cert {
                config.push_str(&format!("        cert_file: \"{}\"\n", cert));
            }
            if let Some(ref key) = self.tls_key {
                config.push_str(&format!("        key_file: \"{}\"\n", key));
            }
            if let Some(ref ca) = self.tls_ca {
                config.push_str(&format!("        ca_file: \"{}\"\n", ca));
            }
            config.push_str("      }\n");
        }

        config.push_str("    }\n");
        config.push_str("  ]\n");
        config.push_str("}\n");

        config
    }

    /// Parse from YAML.
    pub fn from_yaml(yaml: &str) -> eyre::Result<Self> {
        serde_yaml::from_str(yaml)
            .map_err(|e| eyre::eyre!("Failed to parse leaf node config: {}", e))
    }
}

impl Default for LeafNodeConfig {
    fn default() -> Self {
        Self {
            hub_url: "nats-leaf://hub.example.com:7422".to_string(),
            node_id: "local-node".to_string(),
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            credentials: None,
            local_port: 4222,
            heartbeat_interval_secs: 30,
            max_reconnect_attempts: 10,
        }
    }
}

fn default_port() -> u16 {
    4222
}

fn default_heartbeat() -> u64 {
    30
}

fn default_reconnect() -> u32 {
    10
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_nats_config() {
        let config = LeafNodeConfig {
            hub_url: "nats-leaf://hub.contoso.example:7422".to_string(),
            node_id: "kings-macbook".to_string(),
            credentials: Some("/home/kings/.nats/creds".to_string()),
            ..Default::default()
        };

        let nats_config = config.to_nats_config();
        assert!(nats_config.contains("kings-macbook"));
        assert!(nats_config.contains("nats-leaf://hub.contoso.example:7422"));
        assert!(nats_config.contains("credentials"));
        assert!(nats_config.contains("leafnodes"));
    }

    #[test]
    fn generate_nats_config_with_tls() {
        let config = LeafNodeConfig {
            hub_url: "nats-leaf://hub.contoso.example:7422".to_string(),
            node_id: "secure-node".to_string(),
            tls_cert: Some("/etc/nats/cert.pem".to_string()),
            tls_key: Some("/etc/nats/key.pem".to_string()),
            tls_ca: Some("/etc/nats/ca.pem".to_string()),
            ..Default::default()
        };

        let nats_config = config.to_nats_config();
        assert!(nats_config.contains("tls"));
        assert!(nats_config.contains("cert_file"));
        assert!(nats_config.contains("key_file"));
        assert!(nats_config.contains("ca_file"));
    }

    #[test]
    fn parse_from_yaml() {
        let yaml = r#"
hub_url: "nats-leaf://hub.example.com:7422"
node_id: test-node
local_port: 4223
heartbeat_interval_secs: 15
max_reconnect_attempts: 5
"#;
        let config = LeafNodeConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.node_id, "test-node");
        assert_eq!(config.local_port, 4223);
        assert_eq!(config.heartbeat_interval_secs, 15);
    }
}
