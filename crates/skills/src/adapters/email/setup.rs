//! Email setup wizard tool.
//!
//! Provides an interactive `email.setup` tool that guides users through
//! configuring email accounts. Supports preset providers (auto-filling
//! IMAP/SMTP server details) and custom domains (manual entry).
//!
//! The tool works *before* any email accounts are configured — it tests
//! connections with temporary clients and returns vault.set commands
//! for the agent to execute.

use async_trait::async_trait;
use eyre::Result;
use tracing::debug;

use workforce_agent::tools::{Tool, ToolResult};

use super::config::EmailAccountConfig;
use super::presets::KnownProvider;

use crate::adapters::utils::coerce_to_u64;

/// Stateless setup wizard tool — no connector needed.
pub struct EmailSetupTool;

#[async_trait]
impl Tool for EmailSetupTool {
    fn name(&self) -> &str {
        "email.setup"
    }

    fn description(&self) -> &str {
        "Email account setup wizard with provider presets"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list_providers", "test_connection", "configure"],
                    "description": "Action to perform"
                },
                "provider": {
                    "type": "string",
                    "description": "Provider name (gmail, outlook, icloud)"
                },
                "email": {
                    "type": "string",
                    "description": "Email address / username for authentication"
                },
                "password": {
                    "type": "string",
                    "description": "Password or app-specific password"
                },
                "imap_host": {
                    "type": "string",
                    "description": "IMAP server hostname (auto-filled from provider preset if"
                },
                "imap_port": {
                    "type": "integer",
                    "description": "IMAP server port (default: 993)"
                },
                "smtp_host": {
                    "type": "string",
                    "description": "SMTP server hostname (auto-filled from provider preset if"
                },
                "smtp_port": {
                    "type": "integer",
                    "description": "SMTP server port (default: 587)"
                },
                "tls": {
                    "type": "boolean",
                    "description": "Use TLS (default: true, false for ProtonMail Bridge)"
                },
                "account_name": {
                    "type": "string",
                    "description": "Account name for vault storage (default"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = std::time::Instant::now();
        let action = input["action"].as_str().unwrap_or("");

        let result = match action {
            "list_providers" => self.list_providers(),
            "test_connection" => self.test_connection(&input).await,
            "configure" => self.configure(&input).await,
            _ => serde_json::json!({
                "success": false,
                "error": format!(
                    "Unknown action: '{}'. Valid actions: list_providers, test_connection, configure",
                    action
                )
            }),
        };

        let duration = start.elapsed().as_millis() as u64;
        if result["success"].as_bool().unwrap_or(false) {
            Ok(ToolResult::success(result, duration))
        } else {
            Ok(ToolResult::failure(
                result["error"]
                    .as_str()
                    .unwrap_or("Unknown error")
                    .to_string(),
                duration,
            ))
        }
    }
}

impl EmailSetupTool {
    fn list_providers(&self) -> serde_json::Value {
        let providers: Vec<serde_json::Value> = KnownProvider::all()
            .iter()
            .map(|p| {
                let preset = p.preset();
                serde_json::json!({
                    "provider": p.as_str(),
                    "name": preset.name,
                    "imap_host": preset.imap_host,
                    "imap_port": preset.imap_port,
                    "smtp_host": preset.smtp_host,
                    "smtp_port": preset.smtp_port,
                    "tls": preset.tls,
                    "setup_notes": preset.setup_notes,
                    "setup_guidance": p.setup_guidance(),
                })
            })
            .collect();

        serde_json::json!({
            "success": true,
            "providers": providers,
            "custom_note": KnownProvider::Custom.setup_guidance(),
            "usage": "Use action='test_connection' or action='configure' with provider='<name>', email, and password to set up an account."
        })
    }

    async fn test_connection(&self, input: &serde_json::Value) -> serde_json::Value {
        let resolved = match self.resolve_config(input) {
            Ok(r) => r,
            Err(e) => return e,
        };

        let (imap_ok, imap_error) = self.test_imap(&resolved.config).await;
        let (smtp_ok, smtp_error) = self.test_smtp(&resolved.config).await;

        let provider_guidance = resolved.provider.setup_guidance();

        serde_json::json!({
            "success": imap_ok && smtp_ok,
            "imap_ok": imap_ok,
            "smtp_ok": smtp_ok,
            "imap_error": imap_error,
            "smtp_error": smtp_error,
            "provider": resolved.provider.as_str(),
            "imap_host": resolved.config.imap_host,
            "imap_port": resolved.config.imap_port,
            "smtp_host": resolved.config.smtp_host,
            "smtp_port": resolved.config.smtp_port,
            "tls": resolved.config.tls,
            "provider_guidance": if !imap_ok || !smtp_ok { provider_guidance } else { "Connection successful!" },
        })
    }

    async fn configure(&self, input: &serde_json::Value) -> serde_json::Value {
        let resolved = match self.resolve_config(input) {
            Ok(r) => r,
            Err(e) => return e,
        };

        let account_name = input["account_name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if resolved.provider == KnownProvider::Custom {
                    "default"
                } else {
                    resolved.provider.as_str()
                }
            })
            .to_string();

        // Test connection first
        let (imap_ok, imap_error) = self.test_imap(&resolved.config).await;
        let (smtp_ok, smtp_error) = self.test_smtp(&resolved.config).await;

        if !imap_ok || !smtp_ok {
            return serde_json::json!({
                "success": false,
                "error": "Connection test failed — credentials were NOT saved.",
                "imap_ok": imap_ok,
                "smtp_ok": smtp_ok,
                "imap_error": imap_error,
                "smtp_error": smtp_error,
                "provider_guidance": resolved.provider.setup_guidance(),
            });
        }

        // Build vault.set commands
        let prefix = format!("skills/email/accounts/{account_name}");
        let vault_commands: Vec<serde_json::Value> = vec![
            serde_json::json!({
                "path": format!("{prefix}/username"),
                "value": resolved.config.username
            }),
            serde_json::json!({
                "path": format!("{prefix}/password"),
                "value": resolved.config.password
            }),
            serde_json::json!({
                "path": format!("{prefix}/imap_host"),
                "value": resolved.config.imap_host
            }),
            serde_json::json!({
                "path": format!("{prefix}/smtp_host"),
                "value": resolved.config.smtp_host
            }),
            serde_json::json!({
                "path": format!("{prefix}/imap_port"),
                "value": resolved.config.imap_port.to_string()
            }),
            serde_json::json!({
                "path": format!("{prefix}/smtp_port"),
                "value": resolved.config.smtp_port.to_string()
            }),
            serde_json::json!({
                "path": format!("{prefix}/tls"),
                "value": resolved.config.tls.to_string()
            }),
        ];

        serde_json::json!({
            "success": true,
            "message": format!("Connection test passed! Run the vault.set commands below, then call system.enable_skill('email') to activate."),
            "account_name": account_name,
            "vault_commands": vault_commands,
            "next_steps": [
                "Run each vault.set command above to store credentials securely",
                "Call system.enable_skill('email') to load the email skill"
            ],
            "provider": resolved.provider.as_str(),
            "imap_host": resolved.config.imap_host,
            "smtp_host": resolved.config.smtp_host,
        })
    }

    /// Resolve the email config from input, merging preset defaults with overrides.
    fn resolve_config(
        &self,
        input: &serde_json::Value,
    ) -> std::result::Result<ResolvedConfig, serde_json::Value> {
        let email = match input["email"].as_str().filter(|s| !s.is_empty()) {
            Some(e) => e.to_string(),
            None => {
                return Err(serde_json::json!({
                    "success": false,
                    "error": "Missing required parameter 'email'. Provide the email address for authentication."
                }));
            }
        };

        let password = match input["password"].as_str().filter(|s| !s.is_empty()) {
            Some(p) => p.to_string(),
            None => {
                return Err(serde_json::json!({
                    "success": false,
                    "error": "Missing required parameter 'password'. Provide the password or app-specific password."
                }));
            }
        };

        let provider_name = input["provider"].as_str().unwrap_or("custom");
        let provider = KnownProvider::from_name(provider_name);

        // Start with preset defaults (or sensible defaults for Custom)
        let (imap_host, imap_port, smtp_host, smtp_port, tls) = if provider != KnownProvider::Custom
        {
            let preset = provider.preset();
            (
                preset.imap_host.to_string(),
                preset.imap_port,
                preset.smtp_host.to_string(),
                preset.smtp_port,
                preset.tls,
            )
        } else {
            // Custom — require explicit server details
            let imap_host = match input["imap_host"].as_str().filter(|s| !s.is_empty()) {
                Some(h) => h.to_string(),
                None => {
                    return Err(serde_json::json!({
                        "success": false,
                        "error": "Missing 'imap_host' — required for custom providers. Use provider='gmail' etc. for auto-fill, or supply imap_host manually.",
                        "hint": "Set provider to a known name (gmail, icloud, outlook, protonmail, yahoo, fastmail) to auto-fill server details, or provide imap_host and smtp_host for custom domains."
                    }));
                }
            };
            let smtp_host = match input["smtp_host"].as_str().filter(|s| !s.is_empty()) {
                Some(h) => h.to_string(),
                None => {
                    return Err(serde_json::json!({
                        "success": false,
                        "error": "Missing 'smtp_host' — required for custom providers.",
                        "hint": "Provide the SMTP server hostname for your email provider."
                    }));
                }
            };
            let imap_port = coerce_to_u64(&input["imap_port"]).unwrap_or(993) as u16;
            let smtp_port = coerce_to_u64(&input["smtp_port"]).unwrap_or(587) as u16;
            let tls = input["tls"].as_bool().unwrap_or(true);
            (imap_host, imap_port, smtp_host, smtp_port, tls)
        };

        // Allow overrides even for known providers
        let imap_host = input["imap_host"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or(imap_host);
        let smtp_host = input["smtp_host"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or(smtp_host);
        let imap_port = coerce_to_u64(&input["imap_port"])
            .map(|p| p as u16)
            .unwrap_or(imap_port);
        let smtp_port = coerce_to_u64(&input["smtp_port"])
            .map(|p| p as u16)
            .unwrap_or(smtp_port);
        let tls = input["tls"].as_bool().unwrap_or(tls);

        let config = EmailAccountConfig {
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            username: email,
            password,
            display_name: None,
            tls,
            connect_timeout_secs: 30,
            command_timeout_secs: 60,
        };

        Ok(ResolvedConfig { config, provider })
    }

    /// Test IMAP connectivity with a temporary client.
    async fn test_imap(&self, config: &EmailAccountConfig) -> (bool, Option<String>) {
        use super::client::ImapClient;

        debug!(
            host = %config.imap_host,
            port = config.imap_port,
            "Testing IMAP connection"
        );

        match ImapClient::connect(config.clone()).await {
            Ok(client) => {
                // Connection + login succeeded, drop the client
                drop(client);
                (true, None)
            }
            Err(e) => (false, Some(e.to_string())),
        }
    }

    /// Test SMTP connectivity with a temporary transport.
    async fn test_smtp(&self, config: &EmailAccountConfig) -> (bool, Option<String>) {
        use super::tools::build_smtp_transport;

        debug!(
            host = %config.smtp_host,
            port = config.smtp_port,
            "Testing SMTP connection"
        );

        match build_smtp_transport(config).await {
            Ok(transport) => match transport.test_connection().await {
                Ok(true) => (true, None),
                Ok(false) => (
                    false,
                    Some(
                        "SMTP test_connection returned false — server rejected the connection"
                            .to_string(),
                    ),
                ),
                Err(e) => (false, Some(format!("SMTP connection test failed: {e}"))),
            },
            Err(e) => (false, Some(e.to_string())),
        }
    }
}

/// Internal struct for resolved configuration.
#[derive(Debug)]
struct ResolvedConfig {
    config: EmailAccountConfig,
    provider: KnownProvider,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_providers_returns_all() {
        let tool = EmailSetupTool;
        let result = tool.list_providers();
        assert!(result["success"].as_bool().unwrap());

        let providers = result["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 6);

        // Check each provider has the required fields
        for p in providers {
            assert!(p["provider"].is_string());
            assert!(p["name"].is_string());
            assert!(p["imap_host"].is_string());
            assert!(p["imap_port"].is_number());
            assert!(p["smtp_host"].is_string());
            assert!(p["smtp_port"].is_number());
            assert!(p["setup_guidance"].is_string());
        }
    }

    #[test]
    fn list_providers_includes_gmail() {
        let tool = EmailSetupTool;
        let result = tool.list_providers();
        let providers = result["providers"].as_array().unwrap();

        let gmail = providers
            .iter()
            .find(|p| p["provider"].as_str() == Some("gmail"))
            .expect("Gmail should be in providers list");

        assert_eq!(gmail["imap_host"].as_str().unwrap(), "imap.gmail.com");
        assert_eq!(gmail["imap_port"].as_u64().unwrap(), 993);
        assert_eq!(gmail["smtp_host"].as_str().unwrap(), "smtp.gmail.com");
        assert_eq!(gmail["smtp_port"].as_u64().unwrap(), 587);
    }

    #[test]
    fn resolve_config_missing_email() {
        let tool = EmailSetupTool;
        let input = serde_json::json!({"password": "test"});
        let result = tool.resolve_config(&input);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err["error"]
            .as_str()
            .unwrap()
            .contains("Missing required parameter 'email'"));
    }

    #[test]
    fn resolve_config_missing_password() {
        let tool = EmailSetupTool;
        let input = serde_json::json!({"email": "test@gmail.com"});
        let result = tool.resolve_config(&input);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err["error"]
            .as_str()
            .unwrap()
            .contains("Missing required parameter 'password'"));
    }

    #[test]
    fn resolve_config_gmail_autofill() {
        let tool = EmailSetupTool;
        let input = serde_json::json!({
            "provider": "gmail",
            "email": "test@gmail.com",
            "password": "app-password"
        });
        let resolved = tool.resolve_config(&input).unwrap();
        assert_eq!(resolved.config.imap_host, "imap.gmail.com");
        assert_eq!(resolved.config.imap_port, 993);
        assert_eq!(resolved.config.smtp_host, "smtp.gmail.com");
        assert_eq!(resolved.config.smtp_port, 587);
        assert!(resolved.config.tls);
        assert_eq!(resolved.provider, KnownProvider::Gmail);
    }

    #[test]
    fn resolve_config_custom_requires_servers() {
        let tool = EmailSetupTool;
        let input = serde_json::json!({
            "email": "test@custom.com",
            "password": "pass"
        });
        let result = tool.resolve_config(&input);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err["error"]
            .as_str()
            .unwrap()
            .contains("Missing 'imap_host'"));
    }

    #[test]
    fn resolve_config_custom_with_servers() {
        let tool = EmailSetupTool;
        let input = serde_json::json!({
            "email": "test@custom.com",
            "password": "pass",
            "imap_host": "imap.custom.com",
            "smtp_host": "smtp.custom.com",
            "imap_port": 995,
            "smtp_port": 465
        });
        let resolved = tool.resolve_config(&input).unwrap();
        assert_eq!(resolved.config.imap_host, "imap.custom.com");
        assert_eq!(resolved.config.imap_port, 995);
        assert_eq!(resolved.config.smtp_host, "smtp.custom.com");
        assert_eq!(resolved.config.smtp_port, 465);
        assert_eq!(resolved.provider, KnownProvider::Custom);
    }

    #[test]
    fn resolve_config_override_preset() {
        let tool = EmailSetupTool;
        let input = serde_json::json!({
            "provider": "gmail",
            "email": "test@gmail.com",
            "password": "pass",
            "imap_port": 995  // Override the default 993
        });
        let resolved = tool.resolve_config(&input).unwrap();
        assert_eq!(resolved.config.imap_host, "imap.gmail.com"); // From preset
        assert_eq!(resolved.config.imap_port, 995); // Overridden
    }

    #[test]
    fn resolve_config_protonmail_no_tls() {
        let tool = EmailSetupTool;
        let input = serde_json::json!({
            "provider": "protonmail",
            "email": "test@protonmail.com",
            "password": "bridge-password"
        });
        let resolved = tool.resolve_config(&input).unwrap();
        assert_eq!(resolved.config.imap_host, "127.0.0.1");
        assert_eq!(resolved.config.imap_port, 1143);
        assert!(!resolved.config.tls);
    }
}
