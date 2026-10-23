//! How a memory is held as a stored object's properties, the same in every
//! store that holds raw objects: camelCase names, RFC 3339 dates, and no
//! empty identity.

use chrono::{DateTime, Utc};
use serde_json::Value;
use workforce_common::vectorizable::VectorizableText;
use workforce_core::types::AgentId;

use crate::types::{Memory, MemoryId, MemoryLayer, MemorySource, Visibility};

pub(crate) fn layer_to_string(layer: &MemoryLayer) -> &'static str {
    match layer {
        MemoryLayer::Working => "working",
        MemoryLayer::Episodic => "episodic",
        MemoryLayer::Semantic => "semantic",
        MemoryLayer::Team => "team",
        MemoryLayer::Organisation => "organisation",
    }
}

pub(crate) fn string_to_layer(s: &str) -> MemoryLayer {
    match s {
        "working" => MemoryLayer::Working,
        "episodic" => MemoryLayer::Episodic,
        "semantic" => MemoryLayer::Semantic,
        "team" => MemoryLayer::Team,
        "organisation" => MemoryLayer::Organisation,
        _ => MemoryLayer::Episodic,
    }
}

pub(crate) fn visibility_to_string(vis: &Visibility) -> String {
    match vis {
        Visibility::Private => "private".to_string(),
        Visibility::Team(id) => format!("team:{id}"),
        Visibility::Global => "global".to_string(),
    }
}

pub(crate) fn string_to_visibility(s: &str) -> Visibility {
    if s == "private" {
        Visibility::Private
    } else if s == "global" {
        Visibility::Global
    } else if let Some(team_id) = s.strip_prefix("team:") {
        Visibility::Team(team_id.to_string())
    } else {
        Visibility::Private
    }
}

/// The properties a new memory is stored with. `identityId` and
/// `sourceSessionId` are null when absent or empty, never `""`: recall
/// matches a memory scoped to nobody by the property having no value, and
/// an empty string under a word-tokenised schema is "only stopwords".
pub(crate) fn properties(memory: &Memory, content: &VectorizableText) -> Value {
    let present = |value: &Option<String>| {
        value
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    serde_json::json!({
        "content": content,
        "memoryId": memory.id.0,
        "layer": layer_to_string(&memory.layer),
        "owner": memory.owner.as_str(),
        "visibility": visibility_to_string(&memory.visibility),
        "importance": memory.importance,
        "tags": memory.tags,
        "deleted": false,
        "accessCount": 0,
        "createdAt": memory.created_at.to_rfc3339(),
        "lastAccessed": memory.last_accessed.to_rfc3339(),
        "source": memory.source.as_str(),
        "identityId": present(&memory.identity_id),
        "sourceSessionId": present(&memory.source_session_id),
        "sourceMessageIds": memory.source_message_ids,
        "recurrenceCount": memory.recurrence_count,
        "lastRecurrenceAt": memory.last_recurrence_at.map(|at| at.to_rfc3339()),
        "eventTime": memory.event_time.to_rfc3339(),
    })
}

/// The memory a stored object's properties describe, or `None` without its
/// content or memory id.
pub(crate) fn decode_memory(item: &Value) -> Option<Memory> {
    let content = item["content"].as_str()?;
    let memory_id = item["memoryId"].as_str()?;
    let text = |name: &str| {
        item[name]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let texts = |name: &str| -> Vec<String> {
        item[name]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let date = |name: &str| -> Option<DateTime<Utc>> {
        item[name]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
    };
    let created_at = date("createdAt").unwrap_or_else(Utc::now);

    Some(Memory {
        id: MemoryId(memory_id.to_string()),
        content: content.to_string(),
        layer: string_to_layer(item["layer"].as_str().unwrap_or("episodic")),
        owner: AgentId::new(item["owner"].as_str().unwrap_or("unknown")),
        visibility: string_to_visibility(item["visibility"].as_str().unwrap_or("private")),
        importance: item["importance"].as_f64().unwrap_or(0.5) as f32,
        tags: texts("tags"),
        source: item["source"]
            .as_str()
            .map(MemorySource::from_str_lossy)
            .unwrap_or_default(),
        identity_id: text("identityId"),
        source_session_id: text("sourceSessionId"),
        source_message_ids: texts("sourceMessageIds"),
        created_at,
        // Legacy memories written before the field existed fall back to
        // when they were made, so timeline queries keep finding them.
        event_time: date("eventTime").unwrap_or(created_at),
        last_accessed: date("lastAccessed").unwrap_or_else(Utc::now),
        access_count: item["accessCount"].as_u64().unwrap_or(0) as u32,
        recurrence_count: item["recurrenceCount"].as_u64().unwrap_or(0) as u32,
        last_recurrence_at: date("lastRecurrenceAt"),
        deleted: false,
    })
}
