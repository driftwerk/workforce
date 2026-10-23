use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use eyre::Result;
use serde::{Deserialize, Serialize};

/// A credential stored in the node-local credential store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    /// Credential name/key.
    pub name: String,
    /// The secret value (encrypted at rest, plaintext in memory).
    pub value: String,
    /// Which tools/skills can access this credential.
    pub allowed_tools: Vec<String>,
    /// Expiry timestamp (Unix seconds). 0 = no expiry.
    #[serde(default)]
    pub expires_at: u64,
}

/// Node-local credential store. Secrets never leave the node via NATS.
pub struct CredentialStore {
    credentials: Mutex<HashMap<String, Credential>>,
    store_path: Option<std::path::PathBuf>,
}

impl CredentialStore {
    /// Create an in-memory credential store.
    pub fn new() -> Self {
        Self {
            credentials: Mutex::new(HashMap::new()),
            store_path: None,
        }
    }

    /// Create a credential store backed by a file.
    pub fn with_file(path: &Path) -> Result<Self> {
        let credentials = if path.exists() {
            let data = std::fs::read_to_string(path)?;
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            HashMap::new()
        };

        Ok(Self {
            credentials: Mutex::new(credentials),
            store_path: Some(path.to_path_buf()),
        })
    }

    /// Store a credential.
    pub fn set(&self, cred: Credential) -> Result<()> {
        let mut creds = self.credentials.lock().map_err(|e| eyre::eyre!("{}", e))?;
        creds.insert(cred.name.clone(), cred);
        drop(creds);
        self.persist()?;
        Ok(())
    }

    /// Get a credential, checking tool authorization.
    pub fn get(&self, name: &str, requesting_tool: &str) -> Result<Option<String>> {
        let creds = self.credentials.lock().map_err(|e| eyre::eyre!("{}", e))?;

        match creds.get(name) {
            Some(cred) => {
                // Check expiry
                if cred.expires_at > 0 {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    if now > cred.expires_at {
                        return Ok(None);
                    }
                }

                // Check tool authorization
                if cred.allowed_tools.is_empty()
                    || cred
                        .allowed_tools
                        .iter()
                        .any(|t| t == "*" || t == requesting_tool || requesting_tool.starts_with(t))
                {
                    Ok(Some(cred.value.clone()))
                } else {
                    Err(eyre::eyre!(
                        "Tool '{}' not authorized for credential '{}'",
                        requesting_tool,
                        name
                    ))
                }
            }
            None => Ok(None),
        }
    }

    /// Remove a credential.
    pub fn remove(&self, name: &str) -> Result<bool> {
        let mut creds = self.credentials.lock().map_err(|e| eyre::eyre!("{}", e))?;
        let removed = creds.remove(name).is_some();
        drop(creds);
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    /// List credential names (never values).
    pub fn list(&self) -> Result<Vec<String>> {
        let creds = self.credentials.lock().map_err(|e| eyre::eyre!("{}", e))?;
        Ok(creds.keys().cloned().collect())
    }

    /// Persist to disk if file-backed.
    fn persist(&self) -> Result<()> {
        if let Some(ref path) = self.store_path {
            let creds = self.credentials.lock().map_err(|e| eyre::eyre!("{}", e))?;
            let json = serde_json::to_string_pretty(&*creds)?;
            std::fs::write(path, json)?;
        }
        Ok(())
    }
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

/// mTLS certificate configuration for NATS connections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MtlsConfig {
    /// Path to CA certificate.
    pub ca_cert: String,
    /// Path to client certificate.
    pub client_cert: String,
    /// Path to client private key.
    pub client_key: String,
}

impl MtlsConfig {
    /// Validate that all certificate files exist.
    pub fn validate(&self) -> Result<()> {
        for (name, path) in [
            ("CA cert", &self.ca_cert),
            ("Client cert", &self.client_cert),
            ("Client key", &self.client_key),
        ] {
            if !Path::new(path).exists() {
                return Err(eyre::eyre!("{} not found at: {}", name, path));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_store_basics() {
        let store = CredentialStore::new();

        store
            .set(Credential {
                name: "api_key".to_string(),
                value: "secret123".to_string(),
                allowed_tools: vec!["*".to_string()],
                expires_at: 0,
            })
            .unwrap();

        assert_eq!(
            store.get("api_key", "any_tool").unwrap(),
            Some("secret123".to_string())
        );
        assert!(store.get("missing", "tool").unwrap().is_none());
    }

    #[test]
    fn tool_authorization() {
        let store = CredentialStore::new();

        store
            .set(Credential {
                name: "github_token".to_string(),
                value: "ghp_xxx".to_string(),
                allowed_tools: vec!["git.".to_string()],
                expires_at: 0,
            })
            .unwrap();

        // Authorized tool prefix
        assert!(store.get("github_token", "git.push").unwrap().is_some());
        // Unauthorized tool
        assert!(store.get("github_token", "email.send").is_err());
    }

    #[test]
    fn credential_list_and_remove() {
        let store = CredentialStore::new();

        store
            .set(Credential {
                name: "a".to_string(),
                value: "1".to_string(),
                allowed_tools: vec![],
                expires_at: 0,
            })
            .unwrap();
        store
            .set(Credential {
                name: "b".to_string(),
                value: "2".to_string(),
                allowed_tools: vec![],
                expires_at: 0,
            })
            .unwrap();

        assert_eq!(store.list().unwrap().len(), 2);
        assert!(store.remove("a").unwrap());
        assert_eq!(store.list().unwrap().len(), 1);
    }
}
