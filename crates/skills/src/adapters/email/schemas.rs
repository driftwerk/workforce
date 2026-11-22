//! The input schemas of the mail management tools, shared by the tools and
//! the skill's definition.

use serde_json::{json, Value};

/// Which messages a call means, as every bulk tool takes it.
fn selection() -> Value {
    json!({
        "uid": { "type": "integer", "description": "One message's UID" },
        "uids": { "type": "array", "items": { "type": "integer" }, "description": "Message UIDs" },
        "search": search()
    })
}

/// A search matched on the server.
pub fn search() -> Value {
    json!({
        "type": "object",
        "description": "Match messages on the server instead of listing uids: every field narrows it. Use this for many messages.",
        "properties": {
            "from": { "type": "string" },
            "to": { "type": "string" },
            "subject": { "type": "string" },
            "text": { "type": "string", "description": "Anywhere in the headers or body" },
            "before": { "type": "string", "description": "'30d' or '2026-01-15'" },
            "after": { "type": "string", "description": "'30d' or '2026-01-15'" },
            "unread": { "type": "boolean" },
            "has_attachment": { "type": "boolean" }
        },
        "additionalProperties": false
    })
}

fn object(parts: &[Value], required: &[&str]) -> Value {
    let mut properties = json!({ "account": { "type": "string", "description": "Account name" } });
    for part in parts {
        if let (Some(into), Some(from)) = (properties.as_object_mut(), part.as_object()) {
            into.extend(from.clone());
        }
    }
    let required: Vec<&str> = std::iter::once("account")
        .chain(required.iter().copied())
        .collect();
    json!({ "type": "object", "properties": properties, "required": required })
}

const IN_FOLDER: &str = "Folder holding the messages (default: INBOX)";

/// `email.mark`, `email.move`, `email.archive` or `email.delete`.
pub fn bulk(tool: &str) -> Value {
    let (own, required): (Value, &[&str]) = match tool {
        "email.mark" => (
            json!({
                "folder": { "type": "string", "description": IN_FOLDER },
                "action": { "type": "string", "enum": ["read", "unread", "flag", "unflag"] }
            }),
            &["action"],
        ),
        "email.move" => (
            json!({
                "from_folder": { "type": "string", "description": "Source folder (default: INBOX); nested as 'Clients/Acme'" },
                "to_folder": { "type": "string", "description": "Destination folder; nested as 'Clients/Acme'" }
            }),
            &["to_folder"],
        ),
        "email.delete" => (
            json!({
                "folder": { "type": "string", "description": IN_FOLDER },
                "permanent": { "type": "boolean", "description": "Delete the named uids for good instead (irreversible; never by search)" }
            }),
            &[],
        ),
        _ => (
            json!({ "folder": { "type": "string", "description": IN_FOLDER } }),
            &[],
        ),
    };
    let running = json!({
        "dry_run": { "type": "boolean", "description": "Count the matches and show a sample; change nothing" },
        "job": { "type": "string", "description": "Carry on a job a previous call left unfinished" }
    });
    object(&[own, selection(), running], required)
}

pub fn folder(rename: bool) -> Value {
    let named = json!({ "folder": { "type": "string", "description": "The folder, nested as 'Clients/Acme'" } });
    match rename {
        true => object(
            &[
                named,
                json!({ "to": { "type": "string", "description": "Its new name, nested as 'Clients/Acme'" } }),
            ],
            &["folder", "to"],
        ),
        false => object(&[named], &["folder"]),
    }
}

pub fn undo() -> Value {
    object(
        &[
            json!({ "job": { "type": "string", "description": "The job to undo (default: the latest on the account)" } }),
        ],
        &[],
    )
}

pub fn unsubscribe(leave: bool) -> Value {
    let folder = json!({ "folder": { "type": "string", "description": IN_FOLDER } });
    match leave {
        true => object(
            &[
                folder,
                selection(),
                json!({ "lists": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Ids of the lists to leave, from email.unsubscribe_lists"
            } }),
            ],
            &["lists"],
        ),
        false => object(&[folder, selection()], &[]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bulk_tool_takes_a_selection_a_dry_run_and_a_job() {
        for tool in ["email.mark", "email.move", "email.archive", "email.delete"] {
            let schema = bulk(tool);
            for key in ["account", "uid", "uids", "search", "dry_run", "job"] {
                assert!(schema["properties"].get(key).is_some(), "{tool} {key}");
            }
            assert_eq!(schema["required"][0], "account");
        }
        assert_eq!(
            bulk("email.move")["required"],
            json!(["account", "to_folder"])
        );
        assert_eq!(folder(true)["required"], json!(["account", "folder", "to"]));
        assert!(unsubscribe(false)["properties"].get("lists").is_none());
    }
}
