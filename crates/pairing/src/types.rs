use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Pairing policy for a channel platform.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingPolicy {
    /// Unknown senders get a pairing code; owner must approve.
    Pairing,
    /// Only pre-approved senders can interact.
    Allowlist,
    /// Anyone can interact (no pairing needed).
    #[default]
    Open,
    /// No interactions accepted.
    Disabled,
}

/// Status of a pairing request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

impl PairingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

/// A channel pairing request from an unknown sender.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelPairingRequest {
    pub id: String,
    /// 8-character pairing code.
    pub code: String,
    /// Platform name (e.g. "slack", "discord", "telegram", "ws").
    pub platform: String,
    /// Platform-specific channel/conversation ID.
    pub channel_id: String,
    /// Platform-specific sender ID.
    pub sender_id: String,
    /// Human-readable sender name.
    pub sender_name: String,
    pub status: PairingStatus,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub approved_by: Option<String>,
    pub approved_at: Option<DateTime<Utc>>,
}

/// An approved sender on a channel's allowlist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelAllowlistEntry {
    pub id: String,
    pub platform: String,
    pub channel_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub added_at: DateTime<Utc>,
    /// How the entry was added: "pairing:<code>" or "manual".
    pub added_by: String,
}

/// A device/API pairing request from an external service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevicePairingRequest {
    pub id: String,
    /// 8-character pairing code.
    pub code: String,
    /// Human-readable device/service name.
    pub device_name: String,
    /// Stable client-generated UUID for device identity across re-pairs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Optional Ed25519 public key (base64).
    pub public_key: Option<String>,
    /// Requested API scopes.
    pub scopes: Vec<String>,
    pub status: PairingStatus,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub approved_by: Option<String>,
    pub approved_at: Option<DateTime<Utc>>,
    /// Plaintext token stored temporarily after admin approval so the device can retrieve it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_token: Option<String>,
    /// Caller-supplied hint for which existing identity owns this device,
    /// either as a UUID (`identities.id`) or an email address. Used by the
    /// gateway on approval to auto-link the device, removing the need for a
    /// separate JWT login step on the client. Mirrors the identity-by-email
    /// auto-match used by Slack and Keycloak/OIDC provisioning. `None` means
    /// the device is paired anonymously and can be linked later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_hint: Option<String>,
    /// Which kind of device this is, e.g. `ios_device` or `voice_device`.
    ///
    /// Becomes the key under which the device is recorded in
    /// `Identity.linked_ids`, which is a map of *platform* to one id. Pairing
    /// used to hardcode `ios_device` for everything, so pairing any second
    /// device silently overwrote the first — unlinking a device still in use.
    /// `None` means `ios_device`, preserving the behaviour of clients that
    /// predate this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_kind: Option<String>,
}

/// The `linked_ids` key used when a pairing request names no kind. Every
/// client that predates `device_kind` was an iOS device, so this keeps their
/// links exactly where they already are.
pub const DEFAULT_DEVICE_KIND: &str = "ios_device";

/// Every `linked_ids` key a paired device may be recorded under, most common
/// first. Lookup walks this list, so a new surface must be added here or it
/// pairs successfully and then never resolves to its owner.
pub const DEVICE_KINDS: &[&str] = &[DEFAULT_DEVICE_KIND, "voice_device"];

/// Normalise a caller-supplied device kind into something safe to use as a
/// `linked_ids` key and a database value: lowercase, ASCII alphanumeric and
/// underscores only, length-bounded. Anything else is rejected rather than
/// coerced, so a typo surfaces at pairing instead of silently creating a
/// second namespace that never matches on lookup.
pub fn normalise_device_kind(raw: &str) -> Option<String> {
    let kind = raw.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    let valid = !kind.is_empty()
        && kind.len() <= 32
        && kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && kind.starts_with(|c: char| c.is_ascii_alphabetic());
    valid.then_some(kind)
}

/// Which token-hash column a presented token matched against during validation.
///
/// `Current` means the active `token_hash` matched — the steady-state path.
/// `Previous` means the historic `prev_token_hash` matched — the recovery path
/// taken when a client never received the response from a successful rotation.
/// Callers in the rotation code path must refuse to rotate when `Previous`
/// matched so the single-entry recovery slot is never overwritten before the
/// client has caught up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenMatch {
    Current,
    Previous,
}

/// An approved device with its token hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovedDevice {
    pub id: String,
    pub device_name: String,
    /// Stable client-generated UUID for device identity across re-pairs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    pub scopes: Vec<String>,
    /// blake3 hash of the issued bearer token.
    pub token_hash: String,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    pub revoked: bool,
    /// Token expiry. `None` means never expires (backward compat).
    pub expires_at: Option<DateTime<Utc>>,
}

/// A node pairing request from a compute node wanting to join.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodePairingRequest {
    pub id: String,
    /// 8-character pairing code.
    pub code: String,
    pub node_id: String,
    pub node_name: String,
    /// Node type (e.g. "local", "remote").
    pub node_type: String,
    pub capabilities: Vec<String>,
    pub status: PairingStatus,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub approved_by: Option<String>,
    pub approved_at: Option<DateTime<Utc>>,
}

/// Configuration for the pairing subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingConfig {
    /// Default channel pairing policy.
    #[serde(default)]
    pub channel_policy: PairingPolicy,
    /// Per-platform policy overrides (e.g. "slack" -> Pairing).
    #[serde(default)]
    pub platform_policies: HashMap<String, PairingPolicy>,
    /// Pairing code TTL in seconds (default: 900 = 15 minutes).
    #[serde(default = "default_code_ttl")]
    pub code_ttl_secs: u64,
    /// Maximum pending pairing requests across all types (default: 50).
    #[serde(default = "default_max_pending")]
    pub max_pending: usize,
    /// Whether device/API pairing is enabled.
    #[serde(default)]
    pub device_pairing_enabled: bool,
    /// Whether node pairing is required (false = auto-accept).
    #[serde(default)]
    pub node_pairing_enabled: bool,
    /// Device token TTL in seconds (default: 30 days). 0 = never expires.
    #[serde(default = "default_device_token_ttl")]
    pub device_token_ttl_secs: u64,
}

impl Default for PairingConfig {
    fn default() -> Self {
        Self {
            channel_policy: PairingPolicy::Open,
            platform_policies: HashMap::new(),
            code_ttl_secs: default_code_ttl(),
            max_pending: default_max_pending(),
            device_pairing_enabled: false,
            node_pairing_enabled: false,
            device_token_ttl_secs: default_device_token_ttl(),
        }
    }
}

fn default_code_ttl() -> u64 {
    900
}

fn default_max_pending() -> usize {
    50
}

fn default_device_token_ttl() -> u64 {
    2_592_000 // 30 days
}
