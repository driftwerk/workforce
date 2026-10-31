//! A person's own computer as their Desk presents it to a remote hub: the
//! node a Desk becomes on its hub link, and what a turn sees of it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Macos,
    Linux,
    Windows,
}

/// Where Desk's backend runs on the computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Runtime {
    Native,
    Wsl { distro: String },
}

/// A Desk as it introduces itself on its link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskNode {
    /// What the person calls the computer, e.g. "Sarah's MacBook Pro".
    pub name: String,
    pub platform: Platform,
    pub runtime: Runtime,
    /// This workspace's switch on the computer, its kill switch included.
    pub enabled: bool,
    /// The folders agents may work in, as the person reads them
    /// ("~/dev/acme-web").
    #[serde(default)]
    pub folders: Vec<String>,
    /// When the person last used Desk on this computer.
    pub active_at: DateTime<Utc>,
}

/// One of the person's Desks, online: what a turn sees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnlineDesk {
    pub key: String,
    pub device_id: String,
    pub node: DeskNode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_desk_node_reads_as_desk_sends_it() {
        let node: DeskNode = serde_json::from_value(serde_json::json!({
            "name": "Sarah's MacBook Pro",
            "platform": "windows",
            "runtime": {"kind": "wsl", "distro": "Ubuntu"},
            "enabled": true,
            "active_at": "2026-10-30T09:00:00Z",
        }))
        .unwrap();
        assert_eq!(
            node.runtime,
            Runtime::Wsl {
                distro: "Ubuntu".into()
            }
        );
        assert!(node.folders.is_empty());
        let back = serde_json::to_value(&node).unwrap();
        assert_eq!(back["platform"], "windows");
        assert_eq!(serde_json::from_value::<DeskNode>(back).unwrap(), node);
    }
}
