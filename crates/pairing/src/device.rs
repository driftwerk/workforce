use std::sync::Arc;

use chrono::{Duration, Utc};
use eyre::Result;
use tracing::info;
use uuid::Uuid;

use crate::code::generate_pairing_code;
use crate::store::PairingStore;
use crate::types::*;

/// Service for managing device/API pairing.
pub struct DevicePairingService {
    store: Arc<dyn PairingStore>,
    config: PairingConfig,
}

impl DevicePairingService {
    pub fn new(store: Arc<dyn PairingStore>, config: PairingConfig) -> Self {
        Self { store, config }
    }

    /// Initiate a device pairing request. Returns the request with its code.
    ///
    /// `identity_hint` is the caller's email address or identity UUID; the
    /// gateway uses it on approval to auto-link the device to the matching
    /// identity. `None` means the device is paired anonymously and can be
    /// linked later.
    pub fn initiate(
        &self,
        device_name: &str,
        scopes: Vec<String>,
        public_key: Option<&str>,
        client_id: Option<String>,
        identity_hint: Option<String>,
        device_kind: Option<String>,
    ) -> Result<DevicePairingRequest> {
        if !self.config.device_pairing_enabled {
            return Err(eyre::eyre!("Device pairing is not enabled"));
        }

        let total_pending = self.store.count_pending()?;
        if total_pending >= self.config.max_pending {
            return Err(eyre::eyre!("Too many pending pairing requests"));
        }

        let code = generate_pairing_code();
        let now = Utc::now();
        let req = DevicePairingRequest {
            id: Uuid::new_v4().to_string(),
            code: code.clone(),
            device_name: device_name.to_string(),
            client_id,
            public_key: public_key.map(|s| s.to_string()),
            scopes,
            status: PairingStatus::Pending,
            created_at: now,
            expires_at: now + Duration::seconds(self.config.code_ttl_secs as i64),
            approved_by: None,
            approved_at: None,
            pending_token: None,
            identity_hint,
            device_kind,
        };
        self.store.create_device_request(&req)?;

        info!(device_name, code, "Created device pairing request");
        Ok(req)
    }

    /// Approve a device pairing request. Returns the approved device and a bearer token.
    pub fn approve(&self, code: &str, approved_by: &str) -> Result<(ApprovedDevice, String)> {
        approve_in(self.store.as_ref(), &self.config, code, approved_by)
    }

    /// This workspace's pairing settings.
    pub fn config(&self) -> &PairingConfig {
        &self.config
    }

    /// Reject a device pairing request.
    pub fn reject(&self, code: &str) -> Result<()> {
        let req = self
            .store
            .get_device_request_by_code(code)?
            .ok_or_else(|| eyre::eyre!("Unknown pairing code: {code}"))?;

        if req.status != PairingStatus::Pending {
            return Err(eyre::eyre!(
                "Device pairing request is already {}",
                req.status.as_str()
            ));
        }

        self.store
            .update_device_request_status(&req.id, PairingStatus::Rejected, None)?;
        info!(code, "Rejected device pairing");
        Ok(())
    }

    /// Validate a bearer token, returning the device if valid, not revoked, and not expired.
    ///
    /// A token matching via `prev_token_hash` stays valid until the next
    /// successful rotation retires it — validation never mutates token state.
    pub fn validate_token(&self, token: &str) -> Result<Option<ApprovedDevice>> {
        Ok(self.validate_token_with_match(token)?.map(|(d, _)| d))
    }

    /// Validate a bearer token and also report which slot matched.
    ///
    /// Validation is side-effect-free apart from `last_used`. A persisted
    /// token must never be invalidated by merely *seeing* another one: a
    /// client is a set of concurrent holders (REST, WebSocket) that realign
    /// from the keychain on relaunch, so a stale holder presenting the
    /// previous token is routine, not a rollback signal. Tokens are retired
    /// only by the next successful rotation, which callers must refuse off a
    /// `TokenMatch::Previous` match (see the rotate endpoint's 409 guard) —
    /// that guard is what keeps the grace window one generation deep.
    pub fn validate_token_with_match(
        &self,
        token: &str,
    ) -> Result<Option<(ApprovedDevice, TokenMatch)>> {
        let token_hash = blake3::hash(token.as_bytes()).to_hex().to_string();
        match self.store.validate_device_token_hash(&token_hash)? {
            Some((device, kind)) if !device.revoked => {
                if let Some(exp) = device.expires_at {
                    if exp < Utc::now() {
                        return Ok(None);
                    }
                }
                if kind == TokenMatch::Previous {
                    info!(
                        device_id = device.id,
                        last_used = ?device.last_used,
                        "Client used previous token — still within rotation grace window"
                    );
                }
                self.store.update_device_last_used(&device.id)?;
                Ok(Some((device, kind)))
            }
            _ => Ok(None),
        }
    }

    /// Revoke a device's access.
    pub fn revoke(&self, device_id: &str) -> Result<()> {
        self.store.revoke_device(device_id)?;
        info!(device_id, "Revoked device");
        Ok(())
    }

    /// List all paired devices.
    pub fn list_devices(&self) -> Result<Vec<ApprovedDevice>> {
        self.store.list_devices()
    }

    /// List pending device requests.
    pub fn list_pending(&self) -> Result<Vec<DevicePairingRequest>> {
        self.store.list_pending_device_requests()
    }

    /// Check the status of a device pairing request by code (for polling).
    pub fn check_status(&self, code: &str) -> Result<Option<DevicePairingRequest>> {
        self.store.get_device_request_by_code(code)
    }

    /// Clear the pending token for a device pairing request.
    pub fn clear_pending_token(&self, id: &str) -> Result<()> {
        self.store.clear_device_pending_token(id)
    }

    /// Rotate a device's bearer token. Returns the new token.
    pub fn rotate_token(&self, device_id: &str) -> Result<String> {
        let token = format!("wfp_{}", Uuid::new_v4().as_simple());
        let token_hash = blake3::hash(token.as_bytes()).to_hex().to_string();
        let new_expires_at = if self.config.device_token_ttl_secs == 0 {
            None
        } else {
            Some(Utc::now() + Duration::seconds(self.config.device_token_ttl_secs as i64))
        };
        self.store
            .rotate_device_token(device_id, &token_hash, new_expires_at)?;
        info!(device_id, "Rotated device token");
        Ok(token)
    }

    // ── Async wrappers (tokio-safe via spawn_blocking) ──────────────

    /// Async-safe version of [`initiate`].
    pub async fn initiate_async(
        &self,
        device_name: String,
        scopes: Vec<String>,
        public_key: Option<String>,
        client_id: Option<String>,
        identity_hint: Option<String>,
        device_kind: Option<String>,
    ) -> Result<DevicePairingRequest> {
        let store = Arc::clone(&self.store);
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            if !config.device_pairing_enabled {
                return Err(eyre::eyre!("Device pairing is not enabled"));
            }
            let total_pending = store.count_pending()?;
            if total_pending >= config.max_pending {
                return Err(eyre::eyre!("Too many pending pairing requests"));
            }
            let code = generate_pairing_code();
            let now = Utc::now();
            let req = DevicePairingRequest {
                id: Uuid::new_v4().to_string(),
                code: code.clone(),
                device_name: device_name.clone(),
                client_id,
                public_key,
                scopes,
                status: PairingStatus::Pending,
                created_at: now,
                expires_at: now + Duration::seconds(config.code_ttl_secs as i64),
                approved_by: None,
                approved_at: None,
                pending_token: None,
                identity_hint,
                device_kind,
            };
            store.create_device_request(&req)?;
            info!(device_name, code, "Created device pairing request");
            Ok(req)
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Async-safe version of [`approve`].
    pub async fn approve_async(
        &self,
        code: String,
        approved_by: String,
    ) -> Result<(ApprovedDevice, String)> {
        let store = Arc::clone(&self.store);
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            approve_in(store.as_ref(), &config, &code, &approved_by)
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// The device an approved request registered: the request's own id, or
    /// the earlier device it re-paired when it carried a known `client_id`.
    pub async fn device_for_request_async(
        &self,
        req: &DevicePairingRequest,
    ) -> Result<Option<ApprovedDevice>> {
        let store = Arc::clone(&self.store);
        let (id, client_id) = (req.id.clone(), req.client_id.clone());
        tokio::task::spawn_blocking(move || {
            if let Some(device) = store.get_device_by_id(&id)? {
                return Ok(Some(device));
            }
            match client_id {
                Some(cid) => store.find_device_by_client_id(&cid),
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Thin async wrapper that discards the [`TokenMatch`] kind — for callers
    /// that only need to know whether the token is valid. The blocking work
    /// happens inside [`validate_token_with_match_async`].
    pub async fn validate_token_async(&self, token: String) -> Result<Option<ApprovedDevice>> {
        Ok(self
            .validate_token_with_match_async(token)
            .await?
            .map(|(d, _)| d))
    }

    /// Async-safe version of [`validate_token_with_match`]. Runs the blocking
    /// pairing-store lookup on a `spawn_blocking` worker.
    pub async fn validate_token_with_match_async(
        &self,
        token: String,
    ) -> Result<Option<(ApprovedDevice, TokenMatch)>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let token_hash = blake3::hash(token.as_bytes()).to_hex().to_string();
            match store.validate_device_token_hash(&token_hash)? {
                Some((device, kind)) if !device.revoked => {
                    if let Some(exp) = device.expires_at {
                        if exp < Utc::now() {
                            return Ok(None);
                        }
                    }
                    if kind == TokenMatch::Previous {
                        info!(
                            device_id = device.id,
                            last_used = ?device.last_used,
                            "Client used previous token — still within rotation grace window"
                        );
                    }
                    store.update_device_last_used(&device.id)?;
                    Ok(Some((device, kind)))
                }
                _ => Ok(None),
            }
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Async-safe version of [`reject`].
    pub async fn reject_async(&self, code: String) -> Result<()> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let req = store
                .get_device_request_by_code(&code)?
                .ok_or_else(|| eyre::eyre!("Unknown pairing code: {code}"))?;
            if req.status != PairingStatus::Pending {
                return Err(eyre::eyre!(
                    "Device pairing request is already {}",
                    req.status.as_str()
                ));
            }
            store.update_device_request_status(&req.id, PairingStatus::Rejected, None)?;
            info!(code, "Rejected device pairing");
            Ok(())
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Async-safe version of [`revoke`].
    pub async fn revoke_async(&self, device_id: String) -> Result<()> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            store.revoke_device(&device_id)?;
            info!(device_id, "Revoked device");
            Ok(())
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Async-safe version of [`list_devices`].
    pub async fn list_devices_async(&self) -> Result<Vec<ApprovedDevice>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.list_devices())
            .await
            .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Async-safe version of [`list_pending`].
    pub async fn list_pending_async(&self) -> Result<Vec<DevicePairingRequest>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.list_pending_device_requests())
            .await
            .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Async-safe version of [`check_status`].
    pub async fn check_status_async(&self, code: String) -> Result<Option<DevicePairingRequest>> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.get_device_request_by_code(&code))
            .await
            .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Async-safe version of [`clear_pending_token`].
    pub async fn clear_pending_token_async(&self, id: String) -> Result<()> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.clear_device_pending_token(&id))
            .await
            .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }

    /// Async-safe version of [`rotate_token`].
    pub async fn rotate_token_async(&self, device_id: String) -> Result<String> {
        let store = Arc::clone(&self.store);
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            let token = format!("wfp_{}", Uuid::new_v4().as_simple());
            let token_hash = blake3::hash(token.as_bytes()).to_hex().to_string();
            let new_expires_at = if config.device_token_ttl_secs == 0 {
                None
            } else {
                Some(Utc::now() + Duration::seconds(config.device_token_ttl_secs as i64))
            };
            store.rotate_device_token(&device_id, &token_hash, new_expires_at)?;
            info!(device_id, "Rotated device token");
            Ok(token)
        })
        .await
        .map_err(|e| eyre::eyre!("spawn_blocking join error: {e}"))?
    }
}

/// Approve the pending request `code`: claim it (once, however many approvers
/// race), then register the device and park its one-time token for the
/// device to collect.
fn approve_in(
    store: &dyn PairingStore,
    config: &PairingConfig,
    code: &str,
    approved_by: &str,
) -> Result<(ApprovedDevice, String)> {
    let req = store
        .get_device_request_by_code(code)?
        .ok_or_else(|| eyre::eyre!("Unknown pairing code: {code}"))?;

    if req.status != PairingStatus::Pending {
        return Err(eyre::eyre!(
            "Device pairing request is already {}",
            req.status.as_str()
        ));
    }

    if req.expires_at < Utc::now() {
        store.update_device_request_status(&req.id, PairingStatus::Expired, None)?;
        return Err(eyre::eyre!("Pairing code has expired"));
    }

    if !store.claim_device_request(&req.id, approved_by)? {
        return Err(eyre::eyre!("Device pairing request is no longer pending"));
    }

    // Generate a bearer token for the device
    let token = format!("wfp_{}", Uuid::new_v4().as_simple());
    let token_hash = blake3::hash(token.as_bytes()).to_hex().to_string();

    let now = Utc::now();
    let expires_at = if config.device_token_ttl_secs == 0 {
        None
    } else {
        Some(now + Duration::seconds(config.device_token_ttl_secs as i64))
    };

    // If client_id matches an existing device, reuse its id for stable identity
    let device_id = if let Some(ref cid) = req.client_id {
        if let Ok(Some(existing)) = store.find_device_by_client_id(cid) {
            // Remove the old device record so we can re-register with the same id
            store.revoke_device(&existing.id)?;
            existing.id
        } else {
            req.id.clone()
        }
    } else {
        req.id.clone()
    };

    let device = ApprovedDevice {
        id: device_id.clone(),
        device_name: req.device_name.clone(),
        client_id: req.client_id.clone(),
        scopes: req.scopes.clone(),
        token_hash,
        created_at: now,
        last_used: None,
        revoked: false,
        expires_at,
    };
    store.register_device(&device)?;
    store.set_device_pending_token(&req.id, &token)?;

    info!(
        device_name = req.device_name,
        device_id = device_id,
        "Approved device pairing"
    );
    Ok((device, token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqlitePairingStore;
    use std::collections::HashMap;

    fn device_config() -> PairingConfig {
        PairingConfig {
            channel_policy: PairingPolicy::Open,
            platform_policies: HashMap::new(),
            code_ttl_secs: 300,
            max_pending: 50,
            device_pairing_enabled: true,
            node_pairing_enabled: false,
            device_token_ttl_secs: 2_592_000,
            who_can_connect: WhoCanConnect::default(),
        }
    }

    #[test]
    fn device_pairing_flow() {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let svc = DevicePairingService::new(store, device_config());

        // Initiate
        let req = svc
            .initiate(
                "my-app",
                vec!["chat".into(), "sessions".into()],
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(req.device_name, "my-app");
        assert_eq!(req.scopes, vec!["chat", "sessions"]);

        // Approve
        let (device, token) = svc.approve(&req.code, "admin").unwrap();
        assert_eq!(device.device_name, "my-app");
        assert!(token.starts_with("wfp_"));

        // Validate token
        let validated = svc
            .validate_token(&token)
            .unwrap()
            .expect("should validate");
        assert_eq!(validated.device_name, "my-app");

        // Invalid token
        assert!(svc.validate_token("bad_token").unwrap().is_none());

        // Revoke
        svc.revoke(&device.id).unwrap();
        assert!(svc.validate_token(&token).unwrap().is_none());
    }

    /// The rotation endpoint must be able to refuse rotation when the
    /// presented token matched via the recovery slot — otherwise a chain of
    /// failed persists on the iOS side gradually overwrites
    /// `prev_token_hash`, eventually stranding the device on a token the
    /// server no longer recognises.
    #[test]
    fn validate_token_with_match_reports_slot_used() {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let svc = DevicePairingService::new(store, device_config());

        let req = svc
            .initiate("phone", vec!["chat".into()], None, None, None, None)
            .unwrap();
        let (device, original_token) = svc.approve(&req.code, "admin").unwrap();

        let (_, kind_current) = svc
            .validate_token_with_match(&original_token)
            .unwrap()
            .expect("matched original");
        assert_eq!(kind_current, TokenMatch::Current);

        let new_token = svc.rotate_token(&device.id).unwrap();

        let (_, kind_new) = svc
            .validate_token_with_match(&new_token)
            .unwrap()
            .expect("matched new");
        assert_eq!(kind_new, TokenMatch::Current);

        // Presenting the original token after rotation: it lives in
        // prev_token_hash. validate_token_with_match must report `Previous`
        // so the rotation endpoint can refuse to advance the chain.
        let (_, kind_prev) = svc
            .validate_token_with_match(&original_token)
            .unwrap()
            .expect("matched prev");
        assert_eq!(kind_prev, TokenMatch::Previous);

        // Validation must not mutate token state: both generations keep
        // validating on the same slots, in any order, any number of times.
        // The client is concurrent holders (REST + WebSocket) that realign
        // from the keychain on relaunch — seeing the old token is routine
        // and must never invalidate the new one (the prod strand of
        // 2026-06-22 was exactly that).
        let (_, still_prev) = svc
            .validate_token_with_match(&original_token)
            .unwrap()
            .expect("old token stays valid in the grace window");
        assert_eq!(still_prev, TokenMatch::Previous);
        let (_, still_current) = svc
            .validate_token_with_match(&new_token)
            .unwrap()
            .expect("new token must survive the old one being seen");
        assert_eq!(still_current, TokenMatch::Current);
    }

    /// A device past its `expires_at` must fail validation on both slots —
    /// expiry is the one bound that outlives the grace window (a stranded
    /// or idle device dies at TTL instead of living forever on prev).
    #[test]
    fn expired_device_token_fails_validation() {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let svc = DevicePairingService::new(store.clone(), device_config());

        let token = "wfp_expired_test_token";
        let device = ApprovedDevice {
            id: "expired-device".to_string(),
            device_name: "phone".to_string(),
            client_id: None,
            scopes: vec!["chat".to_string()],
            token_hash: blake3::hash(token.as_bytes()).to_hex().to_string(),
            created_at: Utc::now() - Duration::days(60),
            last_used: None,
            revoked: false,
            expires_at: Some(Utc::now() - Duration::days(30)),
        };
        store.register_device(&device).unwrap();

        assert!(svc.validate_token(token).unwrap().is_none());
        assert!(svc.validate_token_with_match(token).unwrap().is_none());
    }

    /// Tokens are retired only by rotation: the second rotation drops the
    /// oldest generation while the newest two keep validating.
    #[test]
    fn second_rotation_retires_oldest_token() {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let svc = DevicePairingService::new(store, device_config());

        let req = svc
            .initiate("phone", vec!["chat".into()], None, None, None, None)
            .unwrap();
        let (device, first_token) = svc.approve(&req.code, "admin").unwrap();
        let second_token = svc.rotate_token(&device.id).unwrap();
        let third_token = svc.rotate_token(&device.id).unwrap();

        assert!(svc
            .validate_token_with_match(&first_token)
            .unwrap()
            .is_none());
        let (_, second_kind) = svc
            .validate_token_with_match(&second_token)
            .unwrap()
            .expect("previous generation valid");
        assert_eq!(second_kind, TokenMatch::Previous);
        let (_, third_kind) = svc
            .validate_token_with_match(&third_token)
            .unwrap()
            .expect("current generation valid");
        assert_eq!(third_kind, TokenMatch::Current);
    }

    #[test]
    fn disabled_rejects() {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let mut config = device_config();
        config.device_pairing_enabled = false;
        let svc = DevicePairingService::new(store, config);

        assert!(svc.initiate("app", vec![], None, None, None, None).is_err());
    }

    #[test]
    fn device_pairing_pending_token_flow() {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let svc = DevicePairingService::new(store.clone(), device_config());

        let req = svc
            .initiate("test-phone", vec!["chat".into()], None, None, None, None)
            .unwrap();

        // Before approval, no pending token
        let check = store
            .get_device_request_by_code(&req.code)
            .unwrap()
            .unwrap();
        assert!(check.pending_token.is_none());

        // Approve
        let (_device, token) = svc.approve(&req.code, "admin").unwrap();
        assert!(token.starts_with("wfp_"));

        // After approval, pending token is set
        let check = store
            .get_device_request_by_code(&req.code)
            .unwrap()
            .unwrap();
        assert_eq!(check.pending_token.as_deref(), Some(token.as_str()));

        // Clear pending token
        store.clear_device_pending_token(&check.id).unwrap();
        let check = store
            .get_device_request_by_code(&req.code)
            .unwrap()
            .unwrap();
        assert!(check.pending_token.is_none());
    }

    #[test]
    fn client_id_reuses_device_id() {
        let store = Arc::new(SqlitePairingStore::in_memory().unwrap());
        let svc = DevicePairingService::new(store, device_config());

        // First pairing with client_id
        let req1 = svc
            .initiate(
                "laptop",
                vec![],
                None,
                Some("stable-uuid-123".to_string()),
                None,
                None,
            )
            .unwrap();
        let (device1, _token1) = svc.approve(&req1.code, "admin").unwrap();

        // Second pairing with same client_id
        let req2 = svc
            .initiate(
                "laptop",
                vec![],
                None,
                Some("stable-uuid-123".to_string()),
                None,
                None,
            )
            .unwrap();
        let (device2, _token2) = svc.approve(&req2.code, "admin").unwrap();

        // Device ID should be reused
        assert_eq!(device1.id, device2.id);
        assert_eq!(device2.client_id.as_deref(), Some("stable-uuid-123"));
    }
}
