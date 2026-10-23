use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A canonical identity in the system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub id: Uuid,
    pub display_name: String,
    pub email: Option<String>,
    #[serde(skip_serializing)]
    pub password_hash: Option<String>,
    pub role: IdentityRole,
    /// The authoritative role key as persisted — a built-in
    /// (`member`/`manager`/`system_admin`) or a custom-role key. `role` is the
    /// lossy enum view used only for the hard admin gates; `role_key` drives
    /// display, assignment, and the capability matrix. Kept in sync with `role`
    /// via [`Identity::set_role`] / [`Identity::set_role_key`].
    #[serde(default)]
    pub role_key: String,
    pub status: IdentityStatus,
    pub provisioning_source: ProvisioningSource,
    /// Platform-keyed linked IDs: "slack" -> "U0ABC", "ios_device" -> "uuid", etc.
    #[serde(default)]
    pub linked_ids: HashMap<String, String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityRole {
    SystemAdmin,
    Manager,
    Member,
}

impl IdentityRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SystemAdmin => "system_admin",
            Self::Manager => "manager",
            Self::Member => "member",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "system_admin" => Self::SystemAdmin,
            "manager" => Self::Manager,
            _ => Self::Member,
        }
    }

    pub fn can_manage_system(&self) -> bool {
        matches!(self, Self::SystemAdmin)
    }

    pub fn can_manage_teams(&self) -> bool {
        matches!(self, Self::SystemAdmin | Self::Manager)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityStatus {
    Active,
    Suspended,
    AutoProvisioned,
}

impl IdentityStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::AutoProvisioned => "auto_provisioned",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "suspended" => Self::Suspended,
            "auto_provisioned" => Self::AutoProvisioned,
            _ => Self::Active,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisioningSource {
    Manual,
    SlackAuto,
    IosAuto,
    OidcAuto,
}

impl ProvisioningSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::SlackAuto => "slack_auto",
            Self::IosAuto => "ios_auto",
            Self::OidcAuto => "oidc_auto",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "slack_auto" => Self::SlackAuto,
            "ios_auto" => Self::IosAuto,
            "oidc_auto" => Self::OidcAuto,
            _ => Self::Manual,
        }
    }
}

impl Identity {
    pub fn new(display_name: impl Into<String>, role: IdentityRole) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            display_name: display_name.into(),
            email: None,
            password_hash: None,
            role,
            role_key: role.as_str().to_string(),
            status: IdentityStatus::Active,
            provisioning_source: ProvisioningSource::Manual,
            linked_ids: HashMap::new(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn with_email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }

    pub fn with_password_hash(mut self, hash: impl Into<String>) -> Self {
        self.password_hash = Some(hash.into());
        self
    }

    pub fn with_status(mut self, status: IdentityStatus) -> Self {
        self.status = status;
        self
    }

    /// Assign a built-in role, keeping the lossy enum and the authoritative
    /// `role_key` in sync.
    pub fn set_role(&mut self, role: IdentityRole) {
        self.role = role;
        self.role_key = role.as_str().to_string();
    }

    /// Assign a role by key (built-in or custom), deriving the lossy enum view
    /// (custom keys collapse to [`IdentityRole::Member`] for the hard gates).
    pub fn set_role_key(&mut self, key: impl Into<String>) {
        self.role_key = key.into();
        self.role = IdentityRole::from_str_lossy(&self.role_key);
    }

    /// The authoritative role key, falling back to the lossy enum's key when the
    /// raw value is unset (data deserialized before `role_key` existed).
    pub fn effective_role_key(&self) -> &str {
        if self.role_key.is_empty() {
            self.role.as_str()
        } else {
            &self.role_key
        }
    }

    pub fn with_provisioning_source(mut self, source: ProvisioningSource) -> Self {
        self.provisioning_source = source;
        self
    }

    pub fn with_linked_id(mut self, platform: impl Into<String>, id: impl Into<String>) -> Self {
        self.linked_ids.insert(platform.into(), id.into());
        self
    }

    pub fn link_id(&mut self, platform: impl Into<String>, id: impl Into<String>) {
        self.linked_ids.insert(platform.into(), id.into());
        self.updated_at = Utc::now();
    }

    pub fn unlink_id(&mut self, platform: &str) {
        self.linked_ids.remove(platform);
        self.updated_at = Utc::now();
    }
}
