//! Plugin distribution — install, update, and manage plugins from various sources.
//!
//! Plugins can be distributed as container images, binaries, or source packages.

use serde::{Deserialize, Serialize};

/// Source from which a plugin can be installed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginSource {
    /// Install from the official plugin registry.
    Registry {
        name: String,
        version: Option<String>,
    },
    /// Install from a git repository.
    Git { url: String, branch: Option<String> },
    /// Install from a local directory.
    Local { path: String },
    /// Install from a container image.
    Container { image: String, tag: Option<String> },
}

/// Distribution format for a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistributionFormat {
    /// Docker/OCI container image (preferred — fully isolated).
    Container,
    /// Standalone binary executable.
    Binary,
    /// Source code (compiled on install).
    Source,
}

/// An installed plugin's metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPlugin {
    /// Plugin name.
    pub name: String,
    /// Installed version.
    pub version: String,
    /// Where it was installed from.
    pub source: PluginSource,
    /// Distribution format.
    pub format: DistributionFormat,
    /// Install path on disk.
    pub install_path: String,
    /// When it was installed.
    pub installed_at: String,
    /// Whether it's currently enabled.
    pub enabled: bool,
}

/// Plugin installation manager.
pub struct PluginInstaller {
    install_dir: String,
    installed: Vec<InstalledPlugin>,
}

impl PluginInstaller {
    pub fn new(install_dir: &str) -> Self {
        Self {
            install_dir: install_dir.to_string(),
            installed: Vec::new(),
        }
    }

    /// Install a plugin from a source.
    pub fn install(&mut self, source: PluginSource) -> eyre::Result<InstalledPlugin> {
        let (name, version, format) = match &source {
            PluginSource::Registry { name, version } => (
                name.clone(),
                version.clone().unwrap_or_else(|| "latest".to_string()),
                DistributionFormat::Container,
            ),
            PluginSource::Git { url, .. } => {
                let name = url
                    .rsplit('/')
                    .next()
                    .unwrap_or("unknown")
                    .trim_end_matches(".git")
                    .to_string();
                (name, "git".to_string(), DistributionFormat::Source)
            }
            PluginSource::Local { path } => {
                let name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("local")
                    .to_string();
                (name, "local".to_string(), DistributionFormat::Source)
            }
            PluginSource::Container { image, tag } => {
                let name = image.rsplit('/').next().unwrap_or(image).to_string();
                (
                    name,
                    tag.clone().unwrap_or_else(|| "latest".to_string()),
                    DistributionFormat::Container,
                )
            }
        };

        // Check if already installed
        if self.is_installed(&name) {
            return Err(eyre::eyre!(
                "Plugin '{}' is already installed. Use update or uninstall first.",
                name
            ));
        }

        let install_path = format!("{}/{}", self.install_dir, name);

        let plugin = InstalledPlugin {
            name: name.clone(),
            version,
            source,
            format,
            install_path,
            installed_at: chrono::Utc::now().to_rfc3339(),
            enabled: true,
        };

        self.installed.push(plugin.clone());
        Ok(plugin)
    }

    /// Uninstall a plugin.
    pub fn uninstall(&mut self, name: &str) -> bool {
        let len = self.installed.len();
        self.installed.retain(|p| p.name != name);
        self.installed.len() < len
    }

    /// Check if a plugin is installed.
    pub fn is_installed(&self, name: &str) -> bool {
        self.installed.iter().any(|p| p.name == name)
    }

    /// Get an installed plugin.
    pub fn get(&self, name: &str) -> Option<&InstalledPlugin> {
        self.installed.iter().find(|p| p.name == name)
    }

    /// List installed plugins.
    pub fn list(&self) -> &[InstalledPlugin] {
        &self.installed
    }

    /// Enable a plugin.
    pub fn enable(&mut self, name: &str) -> bool {
        if let Some(p) = self.installed.iter_mut().find(|p| p.name == name) {
            p.enabled = true;
            true
        } else {
            false
        }
    }

    /// Disable a plugin.
    pub fn disable(&mut self, name: &str) -> bool {
        if let Some(p) = self.installed.iter_mut().find(|p| p.name == name) {
            p.enabled = false;
            true
        } else {
            false
        }
    }

    /// Count installed plugins.
    pub fn count(&self) -> usize {
        self.installed.len()
    }

    /// Count enabled plugins.
    pub fn enabled_count(&self) -> usize {
        self.installed.iter().filter(|p| p.enabled).count()
    }
}

/// Generate the docker run command for a container-based plugin.
pub fn container_run_command(image: &str, tag: &str, nats_url: &str) -> String {
    format!(
        "docker run -d --name workforce-plugin-{} -e NATS_URL={} {}:{}",
        image.rsplit('/').next().unwrap_or(image),
        nats_url,
        image,
        tag
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_from_registry() {
        let mut installer = PluginInstaller::new("/opt/workforce/plugins");
        let result = installer.install(PluginSource::Registry {
            name: "jira".to_string(),
            version: Some("1.0.0".to_string()),
        });

        assert!(result.is_ok());
        let plugin = result.unwrap();
        assert_eq!(plugin.name, "jira");
        assert_eq!(plugin.version, "1.0.0");
        assert!(plugin.enabled);
        assert!(installer.is_installed("jira"));
    }

    #[test]
    fn install_from_git() {
        let mut installer = PluginInstaller::new("/opt/plugins");
        let result = installer.install(PluginSource::Git {
            url: "https://github.com/workforce-plugins/jira.git".to_string(),
            branch: None,
        });

        assert!(result.is_ok());
        assert_eq!(result.unwrap().name, "jira");
    }

    #[test]
    fn duplicate_install_fails() {
        let mut installer = PluginInstaller::new("/opt/plugins");
        installer
            .install(PluginSource::Registry {
                name: "jira".to_string(),
                version: None,
            })
            .unwrap();

        let result = installer.install(PluginSource::Registry {
            name: "jira".to_string(),
            version: None,
        });
        assert!(result.is_err());
    }

    #[test]
    fn uninstall() {
        let mut installer = PluginInstaller::new("/opt/plugins");
        installer
            .install(PluginSource::Registry {
                name: "jira".to_string(),
                version: None,
            })
            .unwrap();

        assert!(installer.uninstall("jira"));
        assert!(!installer.is_installed("jira"));
        assert!(!installer.uninstall("jira")); // Already gone
    }

    #[test]
    fn enable_disable() {
        let mut installer = PluginInstaller::new("/opt/plugins");
        installer
            .install(PluginSource::Registry {
                name: "test".to_string(),
                version: None,
            })
            .unwrap();

        assert_eq!(installer.enabled_count(), 1);
        installer.disable("test");
        assert_eq!(installer.enabled_count(), 0);
        installer.enable("test");
        assert_eq!(installer.enabled_count(), 1);
    }

    #[test]
    fn container_command() {
        let cmd = container_run_command("workforce/jira-plugin", "1.0", "nats://hub:4222");
        assert!(cmd.contains("docker run"));
        assert!(cmd.contains("nats://hub:4222"));
        assert!(cmd.contains("workforce/jira-plugin:1.0"));
    }

    #[test]
    fn source_serialization() {
        let source = PluginSource::Git {
            url: "https://github.com/test/plugin.git".to_string(),
            branch: Some("main".to_string()),
        };
        let json = serde_json::to_string(&source).unwrap();
        let parsed: PluginSource = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, PluginSource::Git { .. }));
    }
}
