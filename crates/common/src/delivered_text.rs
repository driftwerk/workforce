//! The text a tool call delivered to a person: the one place that knows which
//! tools send messages, emails or reminders, and where in their input the text
//! the recipient reads sits.

use serde_json::Value;

use crate::text::html_to_text;

/// Where a tool's input holds delivered text: on the input itself, or on each
/// object of the array under `each`. The first source holding text wins.
struct Pick {
    each: Option<&'static str>,
    any_of: &'static [Source],
}

enum Source {
    Text(&'static str),
    Html(&'static str),
}

const fn field(any_of: &'static [Source]) -> Pick {
    Pick { each: None, any_of }
}

const fn each(array: &'static str, any_of: &'static [Source]) -> Pick {
    Pick {
        each: Some(array),
        any_of,
    }
}

const DELIVERIES: &[(&[&str], &[Pick])] = &[
    (
        &[
            "messaging.send",
            "messaging.edit",
            "slack.update_message",
            "slack.send",
            "google.chat.messages_send",
            "google.chat.dm_send",
        ],
        &[field(&[Source::Text("text")])],
    ),
    (
        &["messaging.upload_file"],
        &[
            field(&[Source::Text("message")]),
            field(&[Source::Text("content_text")]),
        ],
    ),
    (
        &["messaging.send_file", "reminder"],
        &[field(&[Source::Text("message")])],
    ),
    (
        &["slack.upload_file"],
        &[
            field(&[Source::Text("initial_comment")]),
            field(&[Source::Text("content_text")]),
        ],
    ),
    (
        &["email.send", "email.draft"],
        &[
            field(&[Source::Text("subject")]),
            field(&[Source::Text("body"), Source::Html("html")]),
        ],
    ),
    (&["email.forward"], &[field(&[Source::Text("comment")])]),
    (
        &["google.gmail.send", "google.gmail.draft_create"],
        &[
            field(&[Source::Text("subject")]),
            field(&[Source::Html("html_body"), Source::Text("body")]),
        ],
    ),
    (
        &["google.gmail.vacation_set"],
        &[
            field(&[Source::Text("response_subject")]),
            field(&[
                Source::Html("response_body_html"),
                Source::Text("response_body_plain_text"),
            ]),
        ],
    ),
    (
        &["sendgrid.email_send"],
        &[
            field(&[Source::Text("subject")]),
            field(&[Source::Text("text"), Source::Html("html")]),
        ],
    ),
    (
        &["sendgrid.email_send_batch"],
        &[
            field(&[Source::Text("subject")]),
            field(&[Source::Text("text"), Source::Html("html")]),
            each("messages", &[Source::Text("subject")]),
        ],
    ),
    (
        &["postmark.email_send"],
        &[
            field(&[Source::Text("Subject")]),
            field(&[Source::Text("TextBody"), Source::Html("HtmlBody")]),
        ],
    ),
    (
        &["postmark.email_send_batch"],
        &[
            each("messages", &[Source::Text("Subject")]),
            each(
                "messages",
                &[Source::Text("TextBody"), Source::Html("HtmlBody")],
            ),
        ],
    ),
];

/// The text `tool_name` delivered to a person, read from its `input`: each
/// field the tool's table entry names, in order, joined by a blank line.
/// `None` for a tool that delivers no text or an input holding none.
pub fn delivered_text(tool_name: &str, input: &Value) -> Option<String> {
    let (_, picks) = DELIVERIES
        .iter()
        .find(|(tools, _)| tools.contains(&tool_name))?;
    let parts: Vec<String> = picks.iter().flat_map(|pick| pick.read(input)).collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

impl Pick {
    fn read(&self, input: &Value) -> Vec<String> {
        let objects: Vec<&Value> = match self.each {
            None => vec![input],
            Some(array) => input
                .get(array)
                .and_then(Value::as_array)
                .map(|items| items.iter().filter(|item| item.is_object()).collect())
                .unwrap_or_default(),
        };
        objects
            .into_iter()
            .filter_map(|object| self.any_of.iter().find_map(|source| source.read(object)))
            .collect()
    }
}

impl Source {
    fn read(&self, object: &Value) -> Option<String> {
        let (key, convert): (&str, fn(&str) -> String) = match self {
            Source::Text(key) => (key, str::to_string),
            Source::Html(key) => (key, html_to_text),
        };
        let text = convert(object.get(key)?.as_str()?).trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chat_sends_and_edits_deliver_their_text() {
        for tool in [
            "messaging.send",
            "messaging.edit",
            "slack.update_message",
            "slack.send",
            "google.chat.messages_send",
            "google.chat.dm_send",
        ] {
            assert_eq!(
                delivered_text(tool, &json!({"channel": "C1", "text": " hello "})).as_deref(),
                Some("hello"),
                "{tool}"
            );
        }
    }

    #[test]
    fn uploads_deliver_the_comment_then_the_text_body() {
        assert_eq!(
            delivered_text(
                "messaging.upload_file",
                &json!({"message": "lyrics attached", "content_text": "verse one", "filename": "s.txt"})
            )
            .as_deref(),
            Some("lyrics attached\n\nverse one")
        );
        assert_eq!(
            delivered_text(
                "slack.upload_file",
                &json!({"initial_comment": "the report", "content_text": "rows"})
            )
            .as_deref(),
            Some("the report\n\nrows")
        );
        assert_eq!(
            delivered_text(
                "messaging.send_file",
                &json!({"artifact_id": "art_1", "message": "here it is"})
            )
            .as_deref(),
            Some("here it is")
        );
    }

    #[test]
    fn base64_file_bodies_are_never_read() {
        let input = json!({"content_base64": "aGVsbG8=", "filename": "a.bin"});
        assert_eq!(delivered_text("messaging.upload_file", &input), None);
        assert_eq!(delivered_text("slack.upload_file", &input), None);
    }

    #[test]
    fn emails_deliver_subject_then_body() {
        assert_eq!(
            delivered_text(
                "email.send",
                &json!({"to": "a@b.c", "subject": "Lunch", "body": "Noon?"})
            )
            .as_deref(),
            Some("Lunch\n\nNoon?")
        );
        assert_eq!(
            delivered_text(
                "google.gmail.draft_create",
                &json!({"subject": "Hi", "html_body": "<p>Draft</p>"})
            )
            .as_deref(),
            Some("Hi\n\nDraft")
        );
        assert_eq!(
            delivered_text(
                "postmark.email_send",
                &json!({"Subject": "Invoice", "TextBody": "Attached."})
            )
            .as_deref(),
            Some("Invoice\n\nAttached.")
        );
        assert_eq!(
            delivered_text("email.forward", &json!({"uid": 4, "comment": "FYI"})).as_deref(),
            Some("FYI")
        );
        assert_eq!(
            delivered_text("reminder", &json!({"action": "set", "message": "call mum"})).as_deref(),
            Some("call mum")
        );
        assert_eq!(
            delivered_text(
                "google.gmail.vacation_set",
                &json!({
                    "response_subject": "Away",
                    "response_body_plain_text": "unused",
                    "response_body_html": "<p>Back Monday</p>"
                })
            )
            .as_deref(),
            Some("Away\n\nBack Monday")
        );
    }

    #[test]
    fn plain_text_wins_over_html_and_html_alone_is_converted() {
        let both = json!({"subject": "S", "text": "plain", "html": "<b>rich</b>"});
        assert_eq!(
            delivered_text("sendgrid.email_send", &both).as_deref(),
            Some("S\n\nplain")
        );
        let html = json!({"subject": "S", "html": "<p>Hello</p><p>World</p>"});
        assert_eq!(
            delivered_text("sendgrid.email_send", &html).as_deref(),
            Some("S\n\nHello\nWorld")
        );
        let blank_text = json!({"subject": "S", "body": "  ", "html": "<i>fallback</i>"});
        assert_eq!(
            delivered_text("email.draft", &blank_text).as_deref(),
            Some("S\n\nfallback")
        );
    }

    #[test]
    fn gmail_send_records_the_html_body_it_sends_instead_of_the_plain_one() {
        let input = json!({"subject": "S", "body": "unsent", "html_body": "<p>sent</p>"});
        assert_eq!(
            delivered_text("google.gmail.send", &input).as_deref(),
            Some("S\n\nsent")
        );
    }

    #[test]
    fn each_picks_read_every_element_and_skip_non_objects() {
        let input = json!({"messages": [
            {"Subject": "One", "TextBody": "first"},
            "not an object",
            {"Subject": "Two", "HtmlBody": "<p>second</p>"},
            {"To": "x@y.z"},
        ]});
        assert_eq!(
            delivered_text("postmark.email_send_batch", &input).as_deref(),
            Some("One\n\nTwo\n\nfirst\n\nsecond")
        );
        let sendgrid = json!({
            "subject": "Shared",
            "text": "body",
            "messages": [{"subject": "Override"}, {"to": ["a@b.c"]}]
        });
        assert_eq!(
            delivered_text("sendgrid.email_send_batch", &sendgrid).as_deref(),
            Some("Shared\n\nbody\n\nOverride")
        );
        assert_eq!(
            delivered_text("postmark.email_send_batch", &json!({"messages": "x"})),
            None
        );
    }

    #[test]
    fn blank_missing_and_non_string_fields_are_ignored() {
        assert_eq!(
            delivered_text(
                "email.send",
                &json!({"subject": 7, "body": "only the body"})
            )
            .as_deref(),
            Some("only the body")
        );
        assert_eq!(
            delivered_text("messaging.send", &json!({"text": "   "})),
            None
        );
        assert_eq!(
            delivered_text("email.send", &json!({"html": "<p> </p>"})),
            None
        );
        assert_eq!(delivered_text("messaging.send", &json!({})), None);
        assert_eq!(delivered_text("messaging.send", &json!("text")), None);
    }

    #[test]
    fn tools_outside_the_table_deliver_nothing() {
        let input = json!({"text": "hi", "message": "hi", "body": "hi"});
        for tool in [
            "agent_message",
            "email.confirm",
            "google.gmail.draft_send",
            "postmark.email_send_template",
            "messaging.read",
            "shell",
        ] {
            assert_eq!(delivered_text(tool, &input), None, "{tool}");
        }
    }
}
