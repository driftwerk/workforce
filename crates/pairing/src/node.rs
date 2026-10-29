use std::sync::Arc;

use chrono::{Duration, Utc};
use eyre::Result;
use tracing::info;
use uuid::Uuid;

use crate::code::generate_pairing_code;
use crate::store::PairingStore;
use crate::types::*;

/// Service for managing node pairing.
pub struct NodePairingService {
    store: Arc<dyn PairingStore>,
    config: PairingConfig,
}

impl NodePairingService {
    pub fn new(store: Arc<dyn PairingStore>, config: PairingConfig) -> Self {
        Self { store, config }
    }

    /// Whether node pairing is required (vs. auto-accept).
    pub fn is_enabled(&self) -> bool {
        self.config.node_pairing_enabled
    }

    /// A node requests to join the cluster.
    pub fn request_join(
        &self,
        node_id: &str,
        node_name: &str,
        node_type: &str,
        capabilities: Vec<String>,
    ) -> Result<NodePairingRequest> {
        // Check if already approved
        if self.store.is_node_approved(node_id)? {
            return Err(eyre::eyre!("Node {node_id} is already approved"));
        }

        let total_pending = self.store.count_pending()?;
        if total_pending >= self.config.max_pending {
            return Err(eyre::eyre!("Too many pending pairing requests"));
        }

        let code = generate_pairing_code();
        let now = Utc::now();
        let req = NodePairingRequest {
            id: Uuid::new_v4().to_string(),
            code: code.clone(),
            node_id: node_id.to_string(),
            node_name: node_name.to_string(),
            node_type: node_type.to_string(),
            capabilities,
            status: PairingStatus::Pending,
            created_at: now,
            expires_at: now + Duration::seconds(self.config.code_ttl_secs as i64),
            approved_by: None,
            approved_at: None,
        };
        self.store.create_node_request(&req)?;

        info!(node_id, node_name, code, "Created node pairing request");
        Ok(req)
    }

    /// Approve a node pairing request.
    pub fn approve(&self, code: &str, approved_by: &str) -> Result<NodePairingRequest> {
        let mut req = self
            .store
            .get_node_request_by_code(code)?
            .ok_or_else(|| eyre::eyre!("Unknown pairing code: {code}"))?;

        if req.status != PairingStatus::Pending {
            return Err(eyre::eyre!(
                "Node pairing request is already {}",
                req.status.as_str()
            ));
        }

        if req.expires_at < Utc::now() {
            self.store
                .update_node_request_status(&req.id, PairingStatus::Expired, None)?;
            return Err(eyre::eyre!("Pairing code has expired"));
        }

        self.store.update_node_request_status(
            &req.id,
            PairingStatus::Approved,
            Some(approved_by),
        )?;

        info!(
            node_id = req.node_id,
            node_name = req.node_name,
            "Approved node pairing"
        );

        req.status = PairingStatus::Approved;
        req.approved_by = Some(approved_by.to_string());
        req.approved_at = Some(Utc::now());
        Ok(req)
    }

    /// Reject a node pairing request.
    pub fn reject(&self, code: &str) -> Result<()> {
        let req = self
            .store
            .get_node_request_by_code(code)?
            .ok_or_else(|| eyre::eyre!("Unknown pairing code: {code}"))?;

        if req.status != PairingStatus::Pending {
            return Err(eyre::eyre!(
                "Node pairing request is already {}",
                req.status.as_str()
            ));
        }

        self.store
            .update_node_request_status(&req.id, PairingStatus::Rejected, None)?;
        info!(code, "Rejected node pairing");
        Ok(())
    }

    /// Check if a node has been approved.
    pub fn is_node_approved(&self, node_id: &str) -> Result<bool> {
        self.store.is_node_approved(node_id)
    }

    /// List pending node requests.
    pub fn list_pending(&self) -> Result<Vec<NodePairingRequest>> {
        self.store.list_pending_node_requests()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqlitePairingStore;
    use std::collections::HashMap;

    fn node_config() -> PairingConfig {
        PairingConfig {
            channel_policy: PairingPolicy::Open,
            platform_policies: HashMap::new(),
            code_ttl_secs: 900,
            max_pending: 50,
            device_pairing_enabled: false,
            node_pairing_enabled: true,
            device_token_ttl_secs: 2_592_000,
            who_can_connect: WhoCanConnect::default(),
        }
    }

    #[test]
    fn node_pairing_flow() {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let svc = NodePairingService::new(store, node_config());

        // Request join
        let req = svc
            .request_join(
                "node-1",
                "My Laptop",
                "local",
                vec!["agent_execution".into()],
            )
            .unwrap();
        assert!(!svc.is_node_approved("node-1").unwrap());

        // Approve
        let approved = svc.approve(&req.code, "admin").unwrap();
        assert_eq!(approved.node_id, "node-1");
        assert!(svc.is_node_approved("node-1").unwrap());

        // Can't request again (already approved)
        assert!(svc
            .request_join("node-1", "My Laptop", "local", vec![])
            .is_err());
    }

    #[test]
    fn reject_node() {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let svc = NodePairingService::new(store, node_config());

        let req = svc
            .request_join("node-2", "Server", "remote", vec![])
            .unwrap();
        svc.reject(&req.code).unwrap();

        assert!(!svc.is_node_approved("node-2").unwrap());
    }
}
