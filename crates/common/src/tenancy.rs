//! Multi-tenancy — namespace isolation with shared infrastructure.
//!
//! Each organisation gets isolated NATS accounts, Weaviate tenants,
//! and database schemas.

use serde::{Deserialize, Serialize};

/// A tenant (organisation) in the multi-tenant system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    /// Unique organisation ID.
    pub org_id: String,
    /// Organisation name.
    pub name: String,
    /// NATS account name.
    pub nats_account: String,
    /// Database schema name.
    pub db_schema: String,
    /// Weaviate tenant ID.
    pub weaviate_tenant: String,
    /// Encryption key ID for this org's secrets.
    pub encryption_key_id: String,
    /// Whether the tenant is active.
    pub active: bool,
    /// When the tenant was created.
    pub created_at: String,
    /// Isolation level.
    pub isolation: IsolationLevel,
}

/// Isolation level for a tenant.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationLevel {
    /// Shared infrastructure with namespace separation (default, cheapest).
    #[default]
    Shared,
    /// Dedicated database schema, shared NATS/Weaviate.
    SemiDedicated,
    /// Fully dedicated infrastructure (compliance/regulated).
    Dedicated,
}

/// NATS subject prefix for a tenant.
pub fn nats_prefix(org_id: &str) -> String {
    format!("org.{}.workforce", org_id)
}

/// Database schema name for a tenant.
pub fn db_schema_name(org_id: &str) -> String {
    format!("wf_{}", org_id.replace('-', "_"))
}

/// Weaviate tenant ID.
pub fn weaviate_tenant_id(org_id: &str) -> String {
    format!("wf-{}", org_id)
}

/// Tenant context — passed through the request chain for scoping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantContext {
    pub org_id: String,
    pub nats_prefix: String,
    pub db_schema: String,
}

impl TenantContext {
    pub fn new(org_id: &str) -> Self {
        Self {
            org_id: org_id.to_string(),
            nats_prefix: nats_prefix(org_id),
            db_schema: db_schema_name(org_id),
        }
    }

    /// Scope a NATS subject to this tenant.
    pub fn scoped_subject(&self, subject: &str) -> String {
        format!("{}.{}", self.nats_prefix, subject)
    }
}

/// Tenant registry.
pub struct TenantRegistry {
    tenants: std::collections::HashMap<String, Tenant>,
}

impl TenantRegistry {
    pub fn new() -> Self {
        Self {
            tenants: std::collections::HashMap::new(),
        }
    }

    /// Register a new tenant.
    pub fn register(&mut self, org_id: &str, name: &str) -> Tenant {
        let tenant = Tenant {
            org_id: org_id.to_string(),
            name: name.to_string(),
            nats_account: format!("org-{}", org_id),
            db_schema: db_schema_name(org_id),
            weaviate_tenant: weaviate_tenant_id(org_id),
            encryption_key_id: format!("key-{}", org_id),
            active: true,
            created_at: chrono::Utc::now().to_rfc3339(),
            isolation: IsolationLevel::default(),
        };
        self.tenants.insert(org_id.to_string(), tenant.clone());
        tenant
    }

    /// Get a tenant.
    pub fn get(&self, org_id: &str) -> Option<&Tenant> {
        self.tenants.get(org_id)
    }

    /// List active tenants.
    pub fn active(&self) -> Vec<&Tenant> {
        self.tenants.values().filter(|t| t.active).collect()
    }

    /// Deactivate a tenant.
    pub fn deactivate(&mut self, org_id: &str) -> bool {
        if let Some(tenant) = self.tenants.get_mut(org_id) {
            tenant.active = false;
            true
        } else {
            false
        }
    }

    /// Count tenants.
    pub fn count(&self) -> usize {
        self.tenants.len()
    }

    /// Validate cross-tenant isolation.
    pub fn validate_access(&self, requester_org: &str, target_org: &str) -> bool {
        requester_org == target_org
    }
}

impl Default for TenantRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nats_prefix_format() {
        assert_eq!(nats_prefix("contoso"), "org.contoso.workforce");
    }

    #[test]
    fn db_schema_format() {
        assert_eq!(db_schema_name("contoso"), "wf_contoso");
        assert_eq!(db_schema_name("my-org"), "wf_my_org");
    }

    #[test]
    fn tenant_context_scoping() {
        let ctx = TenantContext::new("contoso");
        assert_eq!(
            ctx.scoped_subject("teams.backend.messages"),
            "org.contoso.workforce.teams.backend.messages"
        );
    }

    #[test]
    fn register_and_get() {
        let mut reg = TenantRegistry::new();
        let tenant = reg.register("star", "Contoso");

        assert_eq!(tenant.name, "Contoso");
        assert!(tenant.active);
        assert!(reg.get("star").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn deactivate_tenant() {
        let mut reg = TenantRegistry::new();
        reg.register("star", "Contoso");

        assert_eq!(reg.active().len(), 1);
        reg.deactivate("star");
        assert_eq!(reg.active().len(), 0);
    }

    #[test]
    fn cross_tenant_isolation() {
        let reg = TenantRegistry::new();
        assert!(reg.validate_access("org-a", "org-a"));
        assert!(!reg.validate_access("org-a", "org-b"));
    }

    #[test]
    fn tenant_serialization() {
        let mut reg = TenantRegistry::new();
        let tenant = reg.register("test", "Test Org");
        let json = serde_json::to_string(&tenant).unwrap();
        let parsed: Tenant = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.org_id, "test");
        assert_eq!(parsed.isolation, IsolationLevel::Shared);
    }
}
