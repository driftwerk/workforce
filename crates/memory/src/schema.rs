//! Memory schemas — structured memory types for skills.
//!
//! Skills can define memory schemas that describe the shape of data
//! they store and retrieve. This enables typed memory access.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A memory schema definition from a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySchema {
    /// Schema name (e.g., "email_contacts").
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Which skill defines this schema.
    pub skill: String,
    /// Schema version (for migration).
    pub version: u32,
    /// Field definitions.
    pub fields: Vec<SchemaField>,
    /// Whether this schema is required by the skill.
    #[serde(default)]
    pub required: bool,
}

/// A field in a memory schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaField {
    /// Field name.
    pub name: String,
    /// Field type.
    pub field_type: FieldType,
    /// Human-readable description.
    pub description: String,
    /// Whether this field is required.
    #[serde(default)]
    pub required: bool,
    /// Default value (as JSON).
    #[serde(default)]
    pub default: Option<serde_json::Value>,
}

/// Supported field types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    Integer,
    Float,
    Boolean,
    DateTime,
    Json,
    Array(Box<FieldType>),
    Map(Box<FieldType>),
}

/// A typed memory entry that conforms to a schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypedMemory {
    /// Which schema this memory conforms to.
    pub schema: String,
    /// Schema version.
    pub version: u32,
    /// Field values.
    pub data: HashMap<String, serde_json::Value>,
}

/// Schema registry — tracks all registered schemas.
pub struct SchemaRegistry {
    schemas: HashMap<String, MemorySchema>,
}

impl SchemaRegistry {
    pub fn new() -> Self {
        Self {
            schemas: HashMap::new(),
        }
    }

    /// Register a schema.
    pub fn register(&mut self, schema: MemorySchema) {
        self.schemas.insert(schema.name.clone(), schema);
    }

    /// Get a schema by name.
    pub fn get(&self, name: &str) -> Option<&MemorySchema> {
        self.schemas.get(name)
    }

    /// List all schemas.
    pub fn list(&self) -> Vec<&MemorySchema> {
        self.schemas.values().collect()
    }

    /// List schemas for a specific skill.
    pub fn for_skill(&self, skill: &str) -> Vec<&MemorySchema> {
        self.schemas.values().filter(|s| s.skill == skill).collect()
    }

    /// Validate a typed memory against its schema.
    pub fn validate(&self, memory: &TypedMemory) -> Result<(), Vec<String>> {
        let schema = self
            .schemas
            .get(&memory.schema)
            .ok_or_else(|| vec![format!("Unknown schema: {}", memory.schema)])?;

        if memory.version != schema.version {
            return Err(vec![format!(
                "Schema version mismatch: expected {}, got {}",
                schema.version, memory.version
            )]);
        }

        let mut errors = Vec::new();

        for field in &schema.fields {
            match memory.data.get(&field.name) {
                Some(value) => {
                    if !validate_field_type(value, &field.field_type) {
                        errors.push(format!(
                            "Field '{}': expected type {:?}, got {}",
                            field.name,
                            field.field_type,
                            value_type_name(value)
                        ));
                    }
                }
                None => {
                    if field.required && field.default.is_none() {
                        errors.push(format!("Missing required field: {}", field.name));
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Apply defaults to a typed memory.
    pub fn apply_defaults(&self, memory: &mut TypedMemory) {
        if let Some(schema) = self.schemas.get(&memory.schema) {
            for field in &schema.fields {
                if !memory.data.contains_key(&field.name) {
                    if let Some(ref default) = field.default {
                        memory.data.insert(field.name.clone(), default.clone());
                    }
                }
            }
        }
    }

    /// Remove a schema.
    pub fn unregister(&mut self, name: &str) -> bool {
        self.schemas.remove(name).is_some()
    }

    /// Count schemas.
    pub fn count(&self) -> usize {
        self.schemas.len()
    }
}

impl Default for SchemaRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_field_type(value: &serde_json::Value, field_type: &FieldType) -> bool {
    match field_type {
        FieldType::String => value.is_string(),
        FieldType::Integer => value.is_i64() || value.is_u64(),
        FieldType::Float => value.is_f64() || value.is_i64(),
        FieldType::Boolean => value.is_boolean(),
        FieldType::DateTime => value.is_string(), // ISO 8601 strings
        FieldType::Json => true,                  // Any JSON is valid
        FieldType::Array(inner) => value
            .as_array()
            .is_some_and(|arr| arr.iter().all(|v| validate_field_type(v, inner))),
        FieldType::Map(inner) => value
            .as_object()
            .is_some_and(|obj| obj.values().all(|v| validate_field_type(v, inner))),
    }
}

fn value_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email_contacts_schema() -> MemorySchema {
        MemorySchema {
            name: "email_contacts".to_string(),
            description: "Known email contacts".to_string(),
            skill: "email-management".to_string(),
            version: 1,
            fields: vec![
                SchemaField {
                    name: "email".to_string(),
                    field_type: FieldType::String,
                    description: "Email address".to_string(),
                    required: true,
                    default: None,
                },
                SchemaField {
                    name: "name".to_string(),
                    field_type: FieldType::String,
                    description: "Contact name".to_string(),
                    required: true,
                    default: None,
                },
                SchemaField {
                    name: "interaction_count".to_string(),
                    field_type: FieldType::Integer,
                    description: "Number of interactions".to_string(),
                    required: false,
                    default: Some(serde_json::json!(0)),
                },
                SchemaField {
                    name: "tags".to_string(),
                    field_type: FieldType::Array(Box::new(FieldType::String)),
                    description: "Contact tags".to_string(),
                    required: false,
                    default: Some(serde_json::json!([])),
                },
            ],
            required: false,
        }
    }

    #[test]
    fn register_and_get() {
        let mut reg = SchemaRegistry::new();
        reg.register(email_contacts_schema());

        assert!(reg.get("email_contacts").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn list_for_skill() {
        let mut reg = SchemaRegistry::new();
        reg.register(email_contacts_schema());
        reg.register(MemorySchema {
            name: "preferences".to_string(),
            description: "User preferences".to_string(),
            skill: "email-management".to_string(),
            version: 1,
            fields: vec![],
            required: false,
        });
        reg.register(MemorySchema {
            name: "bookmarks".to_string(),
            description: "Saved bookmarks".to_string(),
            skill: "web-research".to_string(),
            version: 1,
            fields: vec![],
            required: false,
        });

        assert_eq!(reg.for_skill("email-management").len(), 2);
        assert_eq!(reg.for_skill("web-research").len(), 1);
    }

    #[test]
    fn validate_valid_memory() {
        let mut reg = SchemaRegistry::new();
        reg.register(email_contacts_schema());

        let memory = TypedMemory {
            schema: "email_contacts".to_string(),
            version: 1,
            data: HashMap::from([
                ("email".to_string(), serde_json::json!("test@example.com")),
                ("name".to_string(), serde_json::json!("Test User")),
            ]),
        };

        assert!(reg.validate(&memory).is_ok());
    }

    #[test]
    fn validate_missing_required() {
        let mut reg = SchemaRegistry::new();
        reg.register(email_contacts_schema());

        let memory = TypedMemory {
            schema: "email_contacts".to_string(),
            version: 1,
            data: HashMap::from([("email".to_string(), serde_json::json!("test@example.com"))]),
        };

        let errors = reg.validate(&memory).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("name")));
    }

    #[test]
    fn validate_wrong_type() {
        let mut reg = SchemaRegistry::new();
        reg.register(email_contacts_schema());

        let memory = TypedMemory {
            schema: "email_contacts".to_string(),
            version: 1,
            data: HashMap::from([
                ("email".to_string(), serde_json::json!(42)), // Should be string
                ("name".to_string(), serde_json::json!("Test")),
            ]),
        };

        let errors = reg.validate(&memory).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("email")));
    }

    #[test]
    fn validate_array_type() {
        let mut reg = SchemaRegistry::new();
        reg.register(email_contacts_schema());

        let memory = TypedMemory {
            schema: "email_contacts".to_string(),
            version: 1,
            data: HashMap::from([
                ("email".to_string(), serde_json::json!("test@example.com")),
                ("name".to_string(), serde_json::json!("Test")),
                (
                    "tags".to_string(),
                    serde_json::json!(["friend", "colleague"]),
                ),
            ]),
        };

        assert!(reg.validate(&memory).is_ok());
    }

    #[test]
    fn apply_defaults() {
        let mut reg = SchemaRegistry::new();
        reg.register(email_contacts_schema());

        let mut memory = TypedMemory {
            schema: "email_contacts".to_string(),
            version: 1,
            data: HashMap::from([
                ("email".to_string(), serde_json::json!("test@example.com")),
                ("name".to_string(), serde_json::json!("Test")),
            ]),
        };

        reg.apply_defaults(&mut memory);
        assert_eq!(
            memory.data.get("interaction_count"),
            Some(&serde_json::json!(0))
        );
        assert_eq!(memory.data.get("tags"), Some(&serde_json::json!([])));
    }

    #[test]
    fn version_mismatch() {
        let mut reg = SchemaRegistry::new();
        reg.register(email_contacts_schema());

        let memory = TypedMemory {
            schema: "email_contacts".to_string(),
            version: 99,
            data: HashMap::new(),
        };

        assert!(reg.validate(&memory).is_err());
    }
}
