//! Google Workspace skill — pure-Rust OAuth 2.0 + REST client.
//!
//! Provides tools for Calendar, Drive, Sheets, Docs, Gmail, Tasks, Chat,
//! Slides, Contacts, and People across 8 service modules. Authentication
//! uses the workforce vault for OAuth refresh/access tokens; setup is via
//! the `google.setup` agent tool (preferred: agent drives the whole flow
//! from chat) or the `workforce google login` CLI (developer fallback).

pub mod api;
pub mod oauth;
pub mod services;
pub mod setup;
pub mod token_store;
pub mod tool_helpers;
pub mod web_flow;

/// Tool names registered by `services::build_all()` plus `google.setup`.
pub fn tool_names() -> Vec<&'static str> {
    vec![
        // Calendar
        "google.calendar.calendars",
        "google.calendar.events",
        "google.calendar.event_get",
        "google.calendar.create",
        "google.calendar.update",
        "google.calendar.delete",
        "google.calendar.search",
        "google.calendar.freebusy",
        "google.calendar.colors",
        // Drive
        "google.drive.ls",
        "google.drive.search",
        "google.drive.get",
        "google.drive.upload",
        "google.drive.download",
        "google.drive.copy",
        "google.drive.move",
        "google.drive.rename",
        "google.drive.delete",
        "google.drive.mkdir",
        "google.drive.permissions",
        "google.drive.share",
        "google.drive.comments_list",
        "google.drive.drives",
        // Sheets
        "google.sheets.create",
        "google.sheets.get",
        "google.sheets.update",
        "google.sheets.append",
        "google.sheets.clear",
        "google.sheets.batch_update",
        "google.sheets.metadata",
        "google.sheets.add_tab",
        "google.sheets.delete_tab",
        "google.sheets.rename_tab",
        // Docs
        "google.docs.create",
        "google.docs.cat",
        "google.docs.info",
        "google.docs.append",
        "google.docs.insert",
        "google.docs.find_replace",
        "google.docs.batch_update",
        "google.docs.copy",
        // Gmail
        "google.gmail.search",
        "google.gmail.get",
        "google.gmail.send",
        "google.gmail.modify",
        "google.gmail.batch_modify",
        "google.gmail.mark_read",
        "google.gmail.mark_unread",
        "google.gmail.archive",
        "google.gmail.trash",
        "google.gmail.untrash",
        "google.gmail.thread_get",
        "google.gmail.thread_modify",
        "google.gmail.attachment",
        "google.gmail.history",
        "google.gmail.labels_list",
        "google.gmail.label_get",
        "google.gmail.label_create",
        "google.gmail.label_update",
        "google.gmail.label_delete",
        "google.gmail.draft_create",
        "google.gmail.draft_get",
        "google.gmail.drafts_list",
        "google.gmail.draft_delete",
        "google.gmail.draft_send",
        "google.gmail.filter_create",
        "google.gmail.filter_delete",
        "google.gmail.filters_list",
        "google.gmail.forwarding_list",
        "google.gmail.sendas_list",
        "google.gmail.vacation_get",
        "google.gmail.vacation_set",
        // Tasks
        "google.tasks.lists",
        "google.tasks.lists_create",
        "google.tasks.list",
        "google.tasks.get",
        "google.tasks.add",
        "google.tasks.update",
        "google.tasks.done",
        "google.tasks.undo",
        "google.tasks.delete",
        "google.tasks.clear",
        // Contacts + People (registered from contacts.rs)
        "google.contacts.list",
        "google.contacts.get",
        "google.contacts.search",
        "google.contacts.create",
        "google.contacts.update",
        "google.contacts.delete",
        "google.contacts.other_list",
        "google.contacts.directory_search",
        "google.people.me",
        "google.people.get",
        "google.people.search",
        // Chat
        "google.chat.spaces_list",
        "google.chat.spaces_find",
        "google.chat.spaces_create",
        "google.chat.messages_list",
        "google.chat.messages_send",
        "google.chat.dm_send",
        "google.chat.threads_list",
        "google.chat.reactions_list",
        "google.chat.reactions_create",
        "google.chat.reactions_delete",
        // Slides
        "google.slides.create",
        "google.slides.copy",
        "google.slides.info",
        "google.slides.list_slides",
        "google.slides.read_slide",
        "google.slides.add_slide",
        "google.slides.delete_slide",
        "google.slides.replace_text",
        "google.slides.update_notes",
        "google.slides.batch_update",
        "google.slides.export",
        // Setup
        "google.setup",
    ]
}

/// Curated Google read-only tools offered to read-only worker sub-agents:
/// Drive/Docs/Sheets *content* reads only. Deliberately conservative — NO
/// Gmail, Contacts, Calendar, Chat, or any write tool — because a cheap
/// autonomous worker reading a user's inbox/contacts is a privacy surface we
/// don't want on by default. Explicit names, never a `google.*` glob. Google is
/// the OAuth/per-user, 145-tool outlier, so rather than partition the whole
/// surface this is a hand-picked investigation subset; the test below pins every
/// entry to a real registered tool and bars mutating operation names.
pub const WORKER_READ_TOOLS: &[&str] = &[
    "google.drive.ls",
    "google.drive.search",
    "google.drive.get",
    "google.drive.download",
    "google.docs.cat",
    "google.docs.info",
    "google.sheets.get",
    "google.sheets.metadata",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_read_tools_are_real_and_non_mutating() {
        let all: std::collections::HashSet<&str> = tool_names().into_iter().collect();
        // Mutating operation prefixes (the final dotted segment of a tool name).
        const WRITE_OPS: &[&str] = &[
            "create", "update", "delete", "append", "insert", "clear", "upload", "move", "rename",
            "share", "mkdir", "send", "modify", "trash", "untrash", "archive", "mark", "add",
            "done", "undo", "batch", "replace", "set", "copy", "dm",
        ];
        for name in WORKER_READ_TOOLS {
            assert!(all.contains(name), "{name} is not a registered google tool");
            let op = name.rsplit('.').next().unwrap_or("");
            assert!(
                !WRITE_OPS.iter().any(|v| op.starts_with(v)),
                "{name} looks mutating and must not be in the worker read set"
            );
        }
    }
}
