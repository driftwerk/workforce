use std::sync::Arc;

use chrono::{Duration, Utc};
use eyre::Result;
use tracing::{info, warn};
use uuid::Uuid;

use crate::code::generate_pairing_code;
use crate::store::PairingStore;
use crate::types::*;

/// Result of checking whether a sender is allowed.
#[derive(Debug)]
pub enum SenderCheckResult {
    /// Sender is allowed to interact.
    Allowed,
    /// Sender is blocked (disabled policy or not on allowlist).
    Blocked,
    /// Sender needs pairing approval; code has been generated.
    PairingRequired { code: String, message: String },
}

/// Service for managing channel/DM pairing.
pub struct ChannelPairingService {
    store: Arc<dyn PairingStore>,
    config: PairingConfig,
}

impl ChannelPairingService {
    pub fn new(store: Arc<dyn PairingStore>, config: PairingConfig) -> Self {
        Self { store, config }
    }

    /// Check if a sender is authorized on a given platform/channel.
    pub fn check_sender(
        &self,
        platform: &str,
        channel_id: &str,
        sender_id: &str,
        sender_name: &str,
    ) -> Result<SenderCheckResult> {
        let policy = self
            .config
            .platform_policies
            .get(platform)
            .unwrap_or(&self.config.channel_policy);

        match policy {
            PairingPolicy::Open => Ok(SenderCheckResult::Allowed),
            PairingPolicy::Disabled => Ok(SenderCheckResult::Blocked),
            PairingPolicy::Allowlist => {
                if self.store.is_sender_allowed(platform, sender_id)? {
                    Ok(SenderCheckResult::Allowed)
                } else {
                    Ok(SenderCheckResult::Blocked)
                }
            }
            PairingPolicy::Pairing => {
                if self.store.is_sender_allowed(platform, sender_id)? {
                    return Ok(SenderCheckResult::Allowed);
                }
                // Generate or reuse a pending pairing code for this sender.
                let code = self.get_or_create_code(platform, channel_id, sender_id, sender_name)?;
                let message = format!(
                    "I don't recognise you yet. To pair with me, share this code with my owner:\n\n\
                     `{code}`\n\n\
                     They can approve it with: `workforce pair approve {code}`"
                );
                Ok(SenderCheckResult::PairingRequired { code, message })
            }
        }
    }

    /// Approve a pairing code, adding the sender to the allowlist.
    pub fn approve_code(&self, code: &str, approved_by: &str) -> Result<ChannelPairingRequest> {
        let mut req = self
            .store
            .get_channel_request_by_code(code)?
            .ok_or_else(|| eyre::eyre!("Unknown pairing code: {code}"))?;

        if req.status != PairingStatus::Pending {
            return Err(eyre::eyre!(
                "Pairing request is already {}",
                req.status.as_str()
            ));
        }

        if req.expires_at < Utc::now() {
            self.store
                .update_channel_request_status(&req.id, PairingStatus::Expired, None)?;
            return Err(eyre::eyre!("Pairing code has expired"));
        }

        // Mark request as approved
        self.store.update_channel_request_status(
            &req.id,
            PairingStatus::Approved,
            Some(approved_by),
        )?;

        // Add sender to allowlist
        let entry = ChannelAllowlistEntry {
            id: Uuid::new_v4().to_string(),
            platform: req.platform.clone(),
            channel_id: req.channel_id.clone(),
            sender_id: req.sender_id.clone(),
            sender_name: req.sender_name.clone(),
            added_at: Utc::now(),
            added_by: format!("pairing:{code}"),
        };
        self.store.add_to_allowlist(&entry)?;

        info!(
            platform = req.platform,
            sender_id = req.sender_id,
            sender_name = req.sender_name,
            "Approved channel pairing"
        );

        req.status = PairingStatus::Approved;
        req.approved_by = Some(approved_by.to_string());
        req.approved_at = Some(Utc::now());
        Ok(req)
    }

    /// Reject a pairing code.
    pub fn reject_code(&self, code: &str) -> Result<()> {
        let req = self
            .store
            .get_channel_request_by_code(code)?
            .ok_or_else(|| eyre::eyre!("Unknown pairing code: {code}"))?;

        if req.status != PairingStatus::Pending {
            return Err(eyre::eyre!(
                "Pairing request is already {}",
                req.status.as_str()
            ));
        }

        self.store
            .update_channel_request_status(&req.id, PairingStatus::Rejected, None)?;
        info!(code, "Rejected channel pairing");
        Ok(())
    }

    /// Expire stale pending requests. Call periodically.
    pub fn expire_stale(&self) -> Result<usize> {
        self.store.expire_stale_requests(Utc::now())
    }

    /// List pending channel pairing requests.
    pub fn list_pending(&self) -> Result<Vec<ChannelPairingRequest>> {
        self.store.list_pending_channel_requests()
    }

    /// List the channel allowlist, optionally filtered by platform.
    /// Take a sender off the allowlist; `true` when they were on it.
    pub fn remove_sender(&self, platform: &str, sender_id: &str) -> Result<bool> {
        self.store.remove_from_allowlist(platform, sender_id)
    }

    pub fn list_allowlist(&self, platform: Option<&str>) -> Result<Vec<ChannelAllowlistEntry>> {
        self.store.list_allowlist(platform)
    }

    // ── Async wrappers (tokio-safe via spawn_blocking) ──────────────

    /// Async-safe version of [`check_sender`].
    pub async fn check_sender_async(
        &self,
        platform: String,
        channel_id: String,
        sender_id: String,
        sender_name: String,
    ) -> Result<SenderCheckResult> {
        let store = Arc::clone(&self.store);
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            let policy = config
                .platform_policies
                .get(&platform)
                .unwrap_or(&config.channel_policy);
            match policy {
                PairingPolicy::Open => Ok(SenderCheckResult::Allowed),
                PairingPolicy::Disabled => Ok(SenderCheckResult::Blocked),
                PairingPolicy::Allowlist => {
                    if store.is_sender_allowed(&platform, &sender_id)? {
                        Ok(SenderCheckResult::Allowed)
                    } else {
                        Ok(SenderCheckResult::Blocked)
                    }
                }
                PairingPolicy::Pairing => {
                    if store.is_sender_allowed(&platform, &sender_id)? {
                        return Ok(SenderCheckResult::Allowed);
                    }
                    // Check for existing pending code or create new one
                    let pending = store.list_pending_channel_requests()?;
                    let code = if let Some(existing) = pending
                        .iter()
                        .find(|r| r.platform == platform && r.sender_id == sender_id)
                    {
                        if existing.expires_at > chrono::Utc::now() {
                            existing.code.clone()
                        } else {
                            store.update_channel_request_status(
                                &existing.id,
                                PairingStatus::Expired,
                                None,
                            )?;
                            let total_pending = store.count_pending()?;
                            if total_pending >= config.max_pending {
                                return Err(eyre::eyre!("Too many pending pairing requests"));
                            }
                            let new_code = crate::code::generate_pairing_code();
                            let now = chrono::Utc::now();
                            let req = ChannelPairingRequest {
                                id: uuid::Uuid::new_v4().to_string(),
                                code: new_code.clone(),
                                platform: platform.clone(),
                                channel_id: channel_id.clone(),
                                sender_id: sender_id.clone(),
                                sender_name: sender_name.clone(),
                                status: PairingStatus::Pending,
                                created_at: now,
                                expires_at: now
                                    + chrono::Duration::seconds(config.code_ttl_secs as i64),
                                approved_by: None,
                                approved_at: None,
                            };
                            store.create_channel_request(&req)?;
                            new_code
                        }
                    } else {
                        let total_pending = store.count_pending()?;
                        if total_pending >= config.max_pending {
                            return Err(eyre::eyre!("Too many pending pairing requests"));
                        }
                        let new_code = crate::code::generate_pairing_code();
                        let now = chrono::Utc::now();
                        let req = ChannelPairingRequest {
                            id: uuid::Uuid::new_v4().to_string(),
                            code: new_code.clone(),
                            platform: platform.clone(),
                            channel_id,
                            sender_id: sender_id.clone(),
                            sender_name,
                            status: PairingStatus::Pending,
                            created_at: now,
                            expires_at: now
                                + chrono::Duration::seconds(config.code_ttl_secs as i64),
                            approved_by: None,
                            approved_at: None,
                        };
                        store.create_channel_request(&req)?;
                        new_code
                    };
                    let message = format!(
                        "I don't recognise you yet. To pair with me, share this code with my owner:\n\n\
                         `{code}`\n\n\
                         They can approve it with: `workforce pair approve {code}`"
                    );
                    Ok(SenderCheckResult::PairingRequired { code, message })
                }
            }
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Async-safe version of [`approve_code`].
    pub async fn approve_code_async(
        &self,
        code: String,
        approved_by: String,
    ) -> Result<ChannelPairingRequest> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let mut req = store
                .get_channel_request_by_code(&code)?
                .ok_or_else(|| eyre::eyre!("Unknown pairing code: {code}"))?;
            if req.status != PairingStatus::Pending {
                return Err(eyre::eyre!(
                    "Pairing request is already {}",
                    req.status.as_str()
                ));
            }
            if req.expires_at < chrono::Utc::now() {
                store.update_channel_request_status(&req.id, PairingStatus::Expired, None)?;
                return Err(eyre::eyre!("Pairing code has expired"));
            }
            store.update_channel_request_status(
                &req.id,
                PairingStatus::Approved,
                Some(&approved_by),
            )?;
            let entry = ChannelAllowlistEntry {
                id: uuid::Uuid::new_v4().to_string(),
                platform: req.platform.clone(),
                channel_id: req.channel_id.clone(),
                sender_id: req.sender_id.clone(),
                sender_name: req.sender_name.clone(),
                added_at: chrono::Utc::now(),
                added_by: format!("pairing:{code}"),
            };
            store.add_to_allowlist(&entry)?;
            req.status = PairingStatus::Approved;
            req.approved_by = Some(approved_by);
            req.approved_at = Some(chrono::Utc::now());
            Ok(req)
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Async-safe version of [`reject_code`].
    pub async fn reject_code_async(&self, code: String) -> Result<()> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let req = store
                .get_channel_request_by_code(&code)?
                .ok_or_else(|| eyre::eyre!("Unknown pairing code: {code}"))?;
            if req.status != PairingStatus::Pending {
                return Err(eyre::eyre!(
                    "Pairing request is already {}",
                    req.status.as_str()
                ));
            }
            store.update_channel_request_status(&req.id, PairingStatus::Rejected, None)?;
            Ok(())
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Async-safe version of [`list_pending`].
    pub async fn list_pending_async(&self) -> Result<Vec<ChannelPairingRequest>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.list_pending_channel_requests())
            .await
            .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Async-safe version of [`list_allowlist`].
    pub async fn list_allowlist_async(
        &self,
        platform: Option<String>,
    ) -> Result<Vec<ChannelAllowlistEntry>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.list_allowlist(platform.as_deref()))
            .await
            .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Get or create a pairing code for a sender.
    fn get_or_create_code(
        &self,
        platform: &str,
        channel_id: &str,
        sender_id: &str,
        sender_name: &str,
    ) -> Result<String> {
        // Check for an existing pending request for this sender
        let pending = self.store.list_pending_channel_requests()?;
        if let Some(existing) = pending
            .iter()
            .find(|r| r.platform == platform && r.sender_id == sender_id)
        {
            if existing.expires_at > Utc::now() {
                return Ok(existing.code.clone());
            }
            // Expired — mark it and create a new one
            self.store
                .update_channel_request_status(&existing.id, PairingStatus::Expired, None)?;
        }

        // Check max pending limit
        let total_pending = self.store.count_pending()?;
        if total_pending >= self.config.max_pending {
            warn!(
                total_pending,
                max = self.config.max_pending,
                "Too many pending pairing requests"
            );
            return Err(eyre::eyre!("Too many pending pairing requests"));
        }

        let code = generate_pairing_code();
        let now = Utc::now();
        let req = ChannelPairingRequest {
            id: Uuid::new_v4().to_string(),
            code: code.clone(),
            platform: platform.to_string(),
            channel_id: channel_id.to_string(),
            sender_id: sender_id.to_string(),
            sender_name: sender_name.to_string(),
            status: PairingStatus::Pending,
            created_at: now,
            expires_at: now + Duration::seconds(self.config.code_ttl_secs as i64),
            approved_by: None,
            approved_at: None,
        };
        self.store.create_channel_request(&req)?;

        info!(
            platform,
            sender_id,
            code = code,
            "Created channel pairing request"
        );
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqlitePairingStore;
    use std::collections::HashMap;

    fn pairing_config() -> PairingConfig {
        PairingConfig {
            channel_policy: PairingPolicy::Pairing,
            platform_policies: HashMap::new(),
            code_ttl_secs: 900,
            max_pending: 50,
            device_pairing_enabled: false,
            node_pairing_enabled: false,
            device_token_ttl_secs: 2_592_000,
        }
    }

    #[test]
    fn open_policy_allows_all() {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let config = PairingConfig {
            channel_policy: PairingPolicy::Open,
            ..pairing_config()
        };
        let svc = ChannelPairingService::new(store, config);
        match svc.check_sender("slack", "C1", "U1", "Test").unwrap() {
            SenderCheckResult::Allowed => {}
            other => panic!("Expected Allowed, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn pairing_flow() {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let svc = ChannelPairingService::new(store, pairing_config());

        // First check: should require pairing
        let result = svc.check_sender("slack", "C1", "U1", "Alice").unwrap();
        let code = match result {
            SenderCheckResult::PairingRequired { code, .. } => code,
            other => panic!(
                "Expected PairingRequired, got {:?}",
                std::mem::discriminant(&other)
            ),
        };

        // Same sender again: should return same code
        let result2 = svc.check_sender("slack", "C1", "U1", "Alice").unwrap();
        match result2 {
            SenderCheckResult::PairingRequired { code: c2, .. } => assert_eq!(c2, code),
            _ => panic!("Expected same code"),
        }

        // Approve
        svc.approve_code(&code, "admin").unwrap();

        // Now allowed
        match svc.check_sender("slack", "C1", "U1", "Alice").unwrap() {
            SenderCheckResult::Allowed => {}
            other => panic!("Expected Allowed, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn reject_flow() {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let svc = ChannelPairingService::new(store, pairing_config());

        let result = svc.check_sender("slack", "C1", "U2", "Bob").unwrap();
        let code = match result {
            SenderCheckResult::PairingRequired { code, .. } => code,
            _ => panic!("Expected PairingRequired"),
        };

        svc.reject_code(&code).unwrap();

        // Rejecting again should fail
        assert!(svc.reject_code(&code).is_err());
    }

    #[test]
    fn disabled_blocks_all() {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let config = PairingConfig {
            channel_policy: PairingPolicy::Disabled,
            ..pairing_config()
        };
        let svc = ChannelPairingService::new(store, config);
        match svc.check_sender("slack", "C1", "U1", "Test").unwrap() {
            SenderCheckResult::Blocked => {}
            other => panic!("Expected Blocked, got {:?}", std::mem::discriminant(&other)),
        }
    }
}
