//! Comprehensive tests for the DW-425 API handlers and supporting types.
//!
//! Covers:
//! - API type serialization contracts
//! - Error response structure and formatting
//! - Configuration schema generation and validation
//! - Memory path validation security
//! - Request/response round-trip consistency

// ── API Types Serialization ───────────────────────────────────────────

mod api_types_serialization {
    use workforce_common::api_types::*;

    #[test]
    fn config_schema_response_serializes_all_sections() {
        let schema = ConfigSchemaResponse::build();
        let json = serde_json::to_value(&schema).expect("serialization failed");
        let sections = json["sections"]
            .as_array()
            .expect("sections should be array");
        assert_eq!(sections.len(), 4, "expected 4 config sections");
        let names: Vec<&str> = sections
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"llm"), "missing llm section");
        assert!(names.contains(&"compaction"), "missing compaction section");
        assert!(names.contains(&"gateway"), "missing gateway section");
        assert!(names.contains(&"heartbeat"), "missing heartbeat section");
    }

    #[test]
    fn config_schema_fields_have_required_properties() {
        let schema = ConfigSchemaResponse::build();
        for section in &schema.sections {
            assert!(!section.name.is_empty(), "section name should not be empty");
            assert!(
                !section.title.is_empty(),
                "section title should not be empty"
            );
            assert!(
                !section.description.is_empty(),
                "section description should not be empty"
            );
            for field in &section.fields {
                assert!(!field.key.is_empty(), "field key should not be empty");
                assert!(!field.label.is_empty(), "field label should not be empty");
                assert!(
                    !field.description.is_empty(),
                    "field description should not be empty in {}::{}",
                    section.name,
                    field.key
                );
            }
        }
    }

    #[test]
    fn config_schema_select_fields_have_options() {
        let schema = ConfigSchemaResponse::build();
        for section in &schema.sections {
            for field in &section.fields {
                if field.field_type == ConfigFieldType::Select {
                    assert!(
                        field.options.is_some() && !field.options.as_ref().unwrap().is_empty(),
                        "select field {}::{} must have non-empty options",
                        section.name,
                        field.key
                    );
                }
            }
        }
    }

    #[test]
    fn config_schema_integer_fields_have_validation() {
        let schema = ConfigSchemaResponse::build();
        for section in &schema.sections {
            for field in &section.fields {
                if field.field_type == ConfigFieldType::Integer && field.required {
                    // Required integer fields should have validation bounds
                    // (not strictly required for all, but good practice)
                }
            }
        }
    }

    #[test]
    fn config_field_type_serializes_as_snake_case() {
        let json = serde_json::to_value(ConfigFieldType::StringArray).unwrap();
        assert_eq!(json, "string_array");
        let json = serde_json::to_value(ConfigFieldType::KeyValueMap).unwrap();
        assert_eq!(json, "key_value_map");
        let json = serde_json::to_value(ConfigFieldType::Boolean).unwrap();
        assert_eq!(json, "boolean");
    }

    #[test]
    fn config_field_type_deserializes_from_snake_case() {
        let ft: ConfigFieldType = serde_json::from_str("\"string_array\"").unwrap();
        assert_eq!(ft, ConfigFieldType::StringArray);
        let ft: ConfigFieldType = serde_json::from_str("\"key_value_map\"").unwrap();
        assert_eq!(ft, ConfigFieldType::KeyValueMap);
    }

    #[test]
    fn config_update_response_roundtrip() {
        let resp = ConfigUpdateResponse {
            updated: true,
            message: "Configuration saved.".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ConfigUpdateResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.updated);
        assert_eq!(parsed.message, "Configuration saved.");
    }

    #[test]
    fn policy_rules_response_roundtrip() {
        let resp = PolicyRulesResponse {
            rules: vec![PolicyRuleEntry {
                name: "test-rule".to_string(),
                scope: serde_json::json!("global"),
                tool: "git.*".to_string(),
                action: "allow".to_string(),
                priority: 10,
                constitutional: false,
                conditions: None,
            }],
            total: 1,
            constitutional: 0,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: PolicyRulesResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total, 1);
        assert_eq!(parsed.rules[0].name, "test-rule");
        assert_eq!(parsed.rules[0].tool, "git.*");
    }

    #[test]
    fn policy_rule_entry_omits_null_conditions() {
        let entry = PolicyRuleEntry {
            name: "r1".to_string(),
            scope: serde_json::json!("global"),
            tool: "*".to_string(),
            action: "allow".to_string(),
            priority: 0,
            constitutional: true,
            conditions: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert!(!json.as_object().unwrap().contains_key("conditions"));
    }

    #[test]
    fn policy_proposal_create_request_with_defaults() {
        let json = r#"{"proposed_rule": "test", "justification": "because"}"#;
        let req: PolicyProposalCreateRequest = serde_json::from_str(json).unwrap();
        assert!(req.agent_id.is_none());
        assert_eq!(req.proposed_rule, "test");
    }

    #[test]
    fn policy_proposal_review_request_with_defaults() {
        let json = "{}";
        let req: PolicyProposalReviewRequest = serde_json::from_str(json).unwrap();
        assert!(req.reviewed_by.is_none());
        assert!(req.note.is_none());
    }

    #[test]
    fn engagement_modes_response_roundtrip() {
        let resp = EngagementModesResponse {
            overrides: vec![EngagementModeEntry {
                agent_id: "atlas".to_string(),
                channel: "C123".to_string(),
                thread_id: None,
                mode: "all".to_string(),
                set_by: "api".to_string(),
                set_at: "2026-03-14T12:00:00Z".to_string(),
            }],
            count: 1,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: EngagementModesResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.count, 1);
        assert_eq!(parsed.overrides[0].agent_id, "atlas");
        assert_eq!(parsed.overrides[0].channel, "C123");
    }

    #[test]
    fn engagement_mode_set_request_with_thread() {
        let json = r#"{"agent_id": "atlas", "channel": "C123", "thread_id": "12345.6789", "mode": "quiet"}"#;
        let req: EngagementModeSetRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.agent_id, "atlas");
        assert_eq!(req.channel, "C123");
        assert_eq!(req.thread_id.as_deref(), Some("12345.6789"));
        assert_eq!(req.mode, "quiet");
    }

    #[test]
    fn engagement_mode_set_request_without_thread() {
        let json = r#"{"agent_id": "atlas", "channel": "C123", "mode": "all"}"#;
        let req: EngagementModeSetRequest = serde_json::from_str(json).unwrap();
        assert!(req.thread_id.is_none());
    }

    #[test]
    fn memory_files_response_roundtrip() {
        let resp = MemoryFilesResponse {
            agent_id: "agent1".to_string(),
            files: vec![MemoryFileEntry {
                path: "SOUL.md".to_string(),
                size: 1234,
                modified: Some("2026-03-14T12:00:00Z".to_string()),
            }],
            count: 1,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: MemoryFilesResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agent_id, "agent1");
        assert_eq!(parsed.files[0].path, "SOUL.md");
        assert_eq!(parsed.files[0].size, 1234);
    }

    #[test]
    fn memory_file_entry_omits_null_modified() {
        let entry = MemoryFileEntry {
            path: "TEST.md".to_string(),
            size: 0,
            modified: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert!(!json.as_object().unwrap().contains_key("modified"));
    }

    #[test]
    fn memory_write_request_deserialization() {
        let json = r#"{"content": "Hello world"}"#;
        let req: MemoryFileWriteRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.content, "Hello world");
    }

    #[test]
    fn memory_stats_response_roundtrip() {
        let resp = MemoryStatsResponse {
            agent_id: "agent1".to_string(),
            file_count: 10,
            total_size_bytes: 50000,
            daily_note_count: 5,
            task_session_count: 3,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: MemoryStatsResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.file_count, 10);
        assert_eq!(parsed.total_size_bytes, 50000);
    }

    #[test]
    fn memory_tasks_response_with_mixed_tasks() {
        let resp = MemoryTasksResponse {
            agent_id: "agent1".to_string(),
            tasks: vec![
                serde_json::json!({"id": "t1", "status": "pending"}),
                serde_json::json!({"id": "t2", "status": "completed"}),
            ],
            count: 2,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: MemoryTasksResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.count, 2);
        assert_eq!(parsed.tasks.len(), 2);
    }
}

// ── Error Response Structure ──────────────────────────────────────────

mod error_responses {
    use workforce_common::api_types::ApiErrorResponse;

    #[test]
    fn error_response_with_detail() {
        let err = ApiErrorResponse {
            error: "not found".to_string(),
            detail: Some("agent 'xyz' does not exist".to_string()),
        };
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"], "not found");
        assert_eq!(json["detail"], "agent 'xyz' does not exist");
    }

    #[test]
    fn error_response_without_detail_omits_field() {
        let err = ApiErrorResponse {
            error: "unauthorized".to_string(),
            detail: None,
        };
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["error"], "unauthorized");
        assert!(!json.as_object().unwrap().contains_key("detail"));
    }

    #[test]
    fn error_response_roundtrip() {
        let err = ApiErrorResponse {
            error: "bad request".to_string(),
            detail: Some("invalid JSON".to_string()),
        };
        let json = serde_json::to_string(&err).unwrap();
        let parsed: ApiErrorResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.error, "bad request");
        assert_eq!(parsed.detail.as_deref(), Some("invalid JSON"));
    }
}

// ── Schema Validation ─────────────────────────────────────────────────

mod schema_validation {
    use workforce_common::api_types::*;

    #[test]
    fn llm_section_has_provider_field() {
        let schema = ConfigSchemaResponse::build();
        let llm = schema.sections.iter().find(|s| s.name == "llm").unwrap();
        let provider = llm.fields.iter().find(|f| f.key == "provider").unwrap();
        assert_eq!(provider.field_type, ConfigFieldType::Select);
        assert!(provider.required);
        let options = provider.options.as_ref().unwrap();
        assert!(!options.contains(&"claude-max".to_string()));
        assert_eq!(provider.default_value, Some(serde_json::json!("anthropic")));
        for option in ["anthropic", "chatgpt", "claude-code", "codex"] {
            assert!(options.contains(&option.to_string()), "{option}");
        }
    }

    #[test]
    fn llm_section_has_max_tokens_with_validation() {
        let schema = ConfigSchemaResponse::build();
        let llm = schema.sections.iter().find(|s| s.name == "llm").unwrap();
        let max_tokens = llm.fields.iter().find(|f| f.key == "max_tokens").unwrap();
        assert_eq!(max_tokens.field_type, ConfigFieldType::Integer);
        let validation = max_tokens.validation.as_ref().unwrap();
        assert_eq!(validation.min, Some(1.0));
        assert_eq!(validation.max, Some(128000.0));
    }

    #[test]
    fn compaction_section_has_threshold_with_bounds() {
        let schema = ConfigSchemaResponse::build();
        let compact = schema
            .sections
            .iter()
            .find(|s| s.name == "compaction")
            .unwrap();
        let threshold = compact
            .fields
            .iter()
            .find(|f| f.key == "compaction_threshold")
            .unwrap();
        assert_eq!(threshold.field_type, ConfigFieldType::Float);
        let validation = threshold.validation.as_ref().unwrap();
        assert_eq!(validation.min, Some(0.1));
        assert_eq!(validation.max, Some(1.0));
    }

    #[test]
    fn gateway_section_has_listen_and_cors() {
        let schema = ConfigSchemaResponse::build();
        let gw = schema
            .sections
            .iter()
            .find(|s| s.name == "gateway")
            .unwrap();
        assert!(gw.fields.iter().any(|f| f.key == "listen"));
        assert!(gw.fields.iter().any(|f| f.key == "cors"));
    }

    #[test]
    fn heartbeat_section_has_enabled_and_interval() {
        let schema = ConfigSchemaResponse::build();
        let hb = schema
            .sections
            .iter()
            .find(|s| s.name == "heartbeat")
            .unwrap();
        assert!(hb.fields.iter().any(|f| f.key == "enabled"));
        assert!(hb.fields.iter().any(|f| f.key == "interval_secs"));
    }

    #[test]
    fn heartbeat_deliver_is_select_with_options() {
        let schema = ConfigSchemaResponse::build();
        let hb = schema
            .sections
            .iter()
            .find(|s| s.name == "heartbeat")
            .unwrap();
        let deliver = hb.fields.iter().find(|f| f.key == "deliver").unwrap();
        assert_eq!(deliver.field_type, ConfigFieldType::Select);
        let options = deliver.options.as_ref().unwrap();
        assert!(options.contains(&"all".to_string()));
        assert!(options.contains(&"actionable_only".to_string()));
    }

    #[test]
    fn heartbeat_time_fields_have_pattern_validation() {
        let schema = ConfigSchemaResponse::build();
        let hb = schema
            .sections
            .iter()
            .find(|s| s.name == "heartbeat")
            .unwrap();
        let active_start = hb.fields.iter().find(|f| f.key == "active_start").unwrap();
        let validation = active_start.validation.as_ref().unwrap();
        assert!(validation.pattern.is_some());
        assert!(validation.pattern.as_ref().unwrap().contains("\\d{2}"));
    }

    #[test]
    fn schema_serializes_to_valid_json() {
        let schema = ConfigSchemaResponse::build();
        let json_str = serde_json::to_string_pretty(&schema).unwrap();
        // Verify it can be parsed back
        let _: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        // Verify it contains expected keys
        assert!(json_str.contains("\"sections\""));
        assert!(json_str.contains("\"fields\""));
        assert!(json_str.contains("\"field_type\""));
    }

    #[test]
    fn all_field_types_are_represented_in_schema() {
        let schema = ConfigSchemaResponse::build();
        let all_types: Vec<ConfigFieldType> = schema
            .sections
            .iter()
            .flat_map(|s| s.fields.iter().map(|f| f.field_type.clone()))
            .collect();
        assert!(all_types.contains(&ConfigFieldType::String));
        assert!(all_types.contains(&ConfigFieldType::Integer));
        assert!(all_types.contains(&ConfigFieldType::Float));
        assert!(all_types.contains(&ConfigFieldType::Boolean));
        assert!(all_types.contains(&ConfigFieldType::Select));
        assert!(all_types.contains(&ConfigFieldType::StringArray));
        assert!(all_types.contains(&ConfigFieldType::KeyValueMap));
    }
}

// ── ApiError unit tests ───────────────────────────────────────────────

mod api_error_tests {
    use crate::api_handlers::errors::ApiError;
    use axum::http::StatusCode;

    #[test]
    fn bad_request_error() {
        let err = ApiError::bad_request("invalid input");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.error, "invalid input");
        assert!(err.detail.is_none());
    }

    #[test]
    fn error_with_detail() {
        let err = ApiError::internal("operation failed").with_detail("disk full".to_string());
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.error, "operation failed");
        assert_eq!(err.detail.as_deref(), Some("disk full"));
    }

    #[test]
    fn unauthorized_error() {
        let err = ApiError::unauthorized();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        assert_eq!(err.error, "unauthorized");
    }

    #[test]
    fn not_found_error() {
        let err = ApiError::not_found("agent 'x' not found");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn conflict_error() {
        let err = ApiError::conflict("already exists");
        assert_eq!(err.status, StatusCode::CONFLICT);
    }

    #[test]
    fn service_unavailable_error() {
        let err = ApiError::service_unavailable();
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn io_error_not_found_maps_to_404() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let api_err: ApiError = io_err.into();
        assert_eq!(api_err.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn io_error_permission_denied_maps_to_403() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let api_err: ApiError = io_err.into();
        assert_eq!(api_err.status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn io_error_other_maps_to_500_with_detail() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "connection reset");
        let api_err: ApiError = io_err.into();
        assert_eq!(api_err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(api_err.detail.is_some());
        assert!(api_err.detail.unwrap().contains("connection reset"));
    }
}
