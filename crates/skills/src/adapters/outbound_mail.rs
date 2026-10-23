//! MIME assembly for the senders that build their own message (SMTP via
//! lettre, Gmail's `raw`): one body shape for every outbound mail with files —
//! `mixed(related(alternative(text, html), inline…), attachments…)`, each
//! layer present only when it has something to carry.

use lettre::message::header::ContentType;
use lettre::message::{Attachment, MultiPart, SinglePart};

use super::utils::{html_with_inline_images, text_to_html, StagedAttachment};

/// A file carried by an outbound message: inline when it has a `content_id`
/// the HTML references as `cid:…`, otherwise attached.
pub struct MailPart {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
    pub content_id: Option<String>,
}

impl MailPart {
    pub fn attached(filename: String, mime_type: String, data: Vec<u8>) -> Self {
        Self {
            filename,
            mime_type,
            data,
            content_id: None,
        }
    }

    fn part(&self) -> SinglePart {
        let content_type = ContentType::parse(&self.mime_type)
            .unwrap_or_else(|_| ContentType::parse("application/octet-stream").expect("valid"));
        match &self.content_id {
            Some(cid) => Attachment::new_inline(cid.clone()).body(self.data.clone(), content_type),
            None => Attachment::new(self.filename.clone()).body(self.data.clone(), content_type),
        }
    }
}

impl From<StagedAttachment> for MailPart {
    fn from(staged: StagedAttachment) -> Self {
        Self {
            filename: staged.name,
            mime_type: staged.content_type,
            data: staged.bytes,
            content_id: staged.content_id,
        }
    }
}

/// The message body: plain `text` with its `html` alternative when there is
/// one, inline parts related to it, and the rest attached. An inline part
/// only shows from HTML, so a text-only message with one gets an HTML
/// alternative made from its text.
pub fn assemble(text: &str, html: Option<&str>, parts: &[MailPart]) -> MultiPart {
    let (inline, attached): (Vec<&MailPart>, Vec<&MailPart>) =
        parts.iter().partition(|p| p.content_id.is_some());
    let html = html
        .map(str::to_string)
        .or_else(|| (!inline.is_empty()).then(|| text_to_html(text)))
        .map(|h| {
            html_with_inline_images(
                &h,
                inline.iter().filter_map(|p| {
                    p.content_id
                        .as_deref()
                        .map(|cid| (cid, p.filename.as_str()))
                }),
            )
        });
    let body = html.into_iter().fold(
        MultiPart::alternative().singlepart(SinglePart::plain(text.to_string())),
        |body, html| body.singlepart(SinglePart::html(html)),
    );
    let body = match inline.as_slice() {
        [] => body,
        inline => inline
            .iter()
            .fold(MultiPart::related().multipart(body), |m, p| {
                m.singlepart(p.part())
            }),
    };
    match attached.as_slice() {
        [] => body,
        attached => attached
            .iter()
            .fold(MultiPart::mixed().multipart(body), |m, p| {
                m.singlepart(p.part())
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(cid: Option<&str>) -> MailPart {
        MailPart {
            filename: "q3.png".into(),
            mime_type: "image/png".into(),
            data: b"\x89PNG".to_vec(),
            content_id: cid.map(str::to_string),
        }
    }

    fn formatted(part: &MultiPart) -> String {
        String::from_utf8_lossy(&part.formatted()).into_owned()
    }

    #[test]
    fn assemble_collapses_to_an_alternative_when_there_are_no_parts() {
        let out = formatted(&assemble("hi", Some("<p>hi</p>"), &[]));
        assert!(
            out.starts_with("Content-Type: multipart/alternative;"),
            "{out}"
        );
        assert!(!out.contains("multipart/related") && !out.contains("multipart/mixed"));
        assert!(out.contains("<p>hi</p>"));

        let plain = formatted(&assemble("hi", None, &[]));
        assert!(plain.contains("text/plain") && !plain.contains("text/html"));
    }

    #[test]
    fn assemble_nests_related_inside_mixed_and_references_the_cid() {
        let out = formatted(&assemble(
            "hi",
            Some("<p>hi</p>"),
            &[
                png(Some("chart1")),
                MailPart::attached("data.csv".into(), "text/csv".into(), b"a,b".to_vec()),
            ],
        ));
        let mixed = out.find("multipart/mixed").expect("mixed outermost");
        let related = out.find("multipart/related").expect("related inside");
        let alternative = out
            .find("multipart/alternative")
            .expect("alternative innermost");
        assert!(mixed < related && related < alternative, "{out}");
        assert!(out.contains("Content-ID: <chart1>"), "{out}");
        assert!(out.contains("Content-Disposition: inline"), "{out}");
        assert!(
            out.contains("<img src=\"cid:chart1\" alt=\"q3.png\">"),
            "unreferenced inline part gets an img: {out}"
        );
        assert!(
            out.contains("Content-Disposition: attachment; filename=\"data.csv\""),
            "{out}"
        );
    }

    #[test]
    fn assemble_skips_the_related_layer_when_nothing_is_inline() {
        let out = formatted(&assemble("hi", Some("<p>hi</p>"), &[png(None)]));
        assert!(
            out.contains("multipart/mixed") && !out.contains("multipart/related"),
            "{out}"
        );
        assert!(!out.contains("<img"), "{out}");
    }

    #[test]
    fn assemble_builds_an_html_alternative_when_inline_parts_have_no_html() {
        let out = formatted(&assemble(
            "See the chart & more",
            None,
            &[png(Some("chart1"))],
        ));
        assert!(
            out.contains("multipart/related") && out.contains("text/html"),
            "{out}"
        );
        assert!(out.contains("<p>See the chart &amp; more</p>"), "{out}");
        assert!(
            out.contains("<img src=\"cid:chart1\" alt=\"q3.png\">"),
            "{out}"
        );
    }

    #[test]
    fn a_staged_attachment_becomes_a_part() {
        let part = MailPart::from(StagedAttachment {
            name: "q3.png".into(),
            content_type: "image/png".into(),
            bytes: b"\x89PNG".to_vec(),
            content_id: Some("abc".into()),
        });
        assert_eq!(part.data, b"\x89PNG");
        assert_eq!(part.content_id.as_deref(), Some("abc"));
    }
}
