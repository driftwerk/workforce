//! Shared helpers for tool adapters: parameter coercion (LLMs pass `"71"`
//! or `71.0` for integers), response hardening, staged attachments, rate-limit
//! headers, and instance selection.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use serde_json::{json, Value};
use tracing::warn;
use workforce_agent::security::external_content::wrap_external_content;

fn selector(configured: Option<&str>) -> Option<&str> {
    configured.map(str::trim).filter(|name| !name.is_empty())
}

/// The instance a call targets when it names none: the configured selector
/// if it exists, else `default`, else the sole instance.
pub fn preferred_instance<'a, V>(
    clients: &'a HashMap<String, V>,
    configured: Option<&'a str>,
) -> Option<&'a str> {
    selector(configured)
        .filter(|name| clients.contains_key(*name))
        .or_else(|| clients.contains_key("default").then_some("default"))
        .or_else(|| {
            let mut keys = clients.keys();
            match (keys.next(), keys.next()) {
                (Some(sole), None) => Some(sole.as_str()),
                _ => None,
            }
        })
}

/// [`preferred_instance`] at build time: a stale selector is logged and
/// ignored; with several instances and nothing selecting one, the configured
/// name is kept so the call-time error lists what exists. Never errors, since
/// with per-user credentials the instance set differs per enabling user.
pub fn default_instance<V>(
    skill: &str,
    clients: &HashMap<String, V>,
    configured: Option<&str>,
) -> String {
    let configured = selector(configured);
    if let Some(stale) = configured.filter(|name| !clients.contains_key(*name)) {
        let mut available: Vec<&str> = clients.keys().map(String::as_str).collect();
        available.sort_unstable();
        warn!(
            skill,
            selector = stale,
            ?available,
            "default instance selector names no configured instance"
        );
    }
    preferred_instance(clients, configured)
        .unwrap_or(configured.unwrap_or("default"))
        .to_string()
}

/// One client per configured instance and the default a call targets when
/// it names none.
pub struct Instances<C> {
    pub clients: Arc<HashMap<String, Arc<C>>>,
    pub default: String,
}

/// Builds every resolved instance with `make` and picks the default by
/// [`default_instance`]; `empty` is the error when nothing is configured.
pub fn build_instances<W, C>(
    skill: &str,
    resolved: &[(String, W)],
    configured: Option<&str>,
    empty: &str,
    make: impl Fn(&str, &W) -> eyre::Result<C>,
) -> eyre::Result<Instances<C>> {
    if resolved.is_empty() {
        return Err(eyre::eyre!("{empty}"));
    }
    let clients = resolved
        .iter()
        .map(|(name, instance)| make(name, instance).map(|client| (name.clone(), Arc::new(client))))
        .collect::<eyre::Result<HashMap<_, _>>>()?;
    let default = default_instance(skill, &clients, configured);
    Ok(Instances {
        clients: Arc::new(clients),
        default,
    })
}

/// The `Available: …. Default: ….` tail of an instance parameter's
/// description; the default reads `none — pass <param>` when no instance
/// carries that name.
pub fn instance_choices<V>(clients: &HashMap<String, V>, default: &str, param: &str) -> String {
    let mut names: Vec<&str> = clients.keys().map(String::as_str).collect();
    names.sort_unstable();
    let default = if clients.contains_key(default) {
        default.to_string()
    } else {
        format!("none — pass {param}")
    };
    format!("Available: {}. Default: {default}.", names.join(", "))
}

/// Parse a `Retry-After` response header as whole seconds. The HTTP-date form
/// is not used by the APIs these adapters call, so it parses as `None`.
#[cfg(feature = "reqwest")]
pub fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// Coerce a JSON value to `u64`, trying:
/// 1. Native `as_u64()`
/// 2. String parse: `value.as_str()?.trim().parse::<u64>()`
/// 3. Float cast: `value.as_f64()` with bounds checking (must be non-negative,
///    finite, within `u64::MAX`, and have no fractional part)
pub fn coerce_to_u64(value: &Value) -> Option<u64> {
    // 1. Native unsigned integer
    if let Some(n) = value.as_u64() {
        return Some(n);
    }

    // 2. String parse (handles "71", " 71 ", etc.)
    if let Some(s) = value.as_str() {
        if let Ok(n) = s.trim().parse::<u64>() {
            return Some(n);
        }
    }

    // 3. Float cast with bounds checking
    if let Some(f) = value.as_f64() {
        if f.is_finite() && f >= 0.0 && f <= u64::MAX as f64 && f.fract() == 0.0 {
            return Some(f as u64);
        }
    }

    None
}

/// Coerce an id-like JSON value to a string: strings are trimmed, and
/// non-negative integers (LLMs pass `2` for `"2"`) are stringified.
pub fn coerce_to_id_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(_) => coerce_to_u64(value).map(|n| n.to_string()),
        _ => None,
    }
}

/// Coerce a JSON value to `i64`, trying:
/// 1. Native `as_i64()`
/// 2. String parse: `value.as_str()?.trim().parse::<i64>()`
/// 3. Float cast: `value.as_f64()` with bounds checking (must be finite,
///    within `i64` range, and have no fractional part)
pub fn coerce_to_i64(value: &Value) -> Option<i64> {
    // 1. Native signed integer
    if let Some(n) = value.as_i64() {
        return Some(n);
    }

    // 2. String parse (handles "-5", " 42 ", etc.)
    if let Some(s) = value.as_str() {
        if let Ok(n) = s.trim().parse::<i64>() {
            return Some(n);
        }
    }

    // 3. Float cast with bounds checking
    if let Some(f) = value.as_f64() {
        if f.is_finite() && f >= i64::MIN as f64 && f <= i64::MAX as f64 && f.fract() == 0.0 {
            return Some(f as i64);
        }
    }

    None
}

/// Resolve the body of a file upload from the mutually-exclusive `content_text`
/// and `content_base64` parameters shared by the file-upload tools.
///
/// Base64 input tolerates embedded whitespace, since the `base64` CLI wraps its
/// output at 76 columns. Returns the decoded bytes, or a user-facing error
/// message describing what was wrong with the input.
pub fn decode_upload_content(
    content_text: Option<&str>,
    content_base64: Option<&str>,
) -> Result<Vec<u8>, String> {
    let bytes = match (content_text, content_base64) {
        (Some(_), Some(_)) => {
            return Err(
                "upload_file: provide exactly one of `content_text` or `content_base64`"
                    .to_string(),
            );
        }
        (Some(text), None) => text.as_bytes().to_vec(),
        (None, Some(b64)) => {
            let stripped: String = b64.split_whitespace().collect();
            base64::engine::general_purpose::STANDARD
                .decode(&stripped)
                .map_err(|e| format!("upload_file: content_base64 is not valid base64: {e}"))?
        }
        (None, None) => {
            return Err(
                "upload_file: provide one of `content_text` or `content_base64`".to_string(),
            );
        }
    };
    if bytes.is_empty() {
        return Err("upload_file: file content is empty".to_string());
    }
    Ok(bytes)
}

/// A staged artifact resolved into the form a tool needs to ship it out of the
/// agent: raw bytes, a best-effort MIME, and a default filename.
pub struct StagedFile {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub filename: String,
}

/// Best-effort MIME from leading magic bytes. Artifacts are stored as opaque
/// octets when produced (a generated image, a browser.pdf), so their real type
/// survives only in the producing tool's output — this recovers it so a
/// shipped-out file keeps a usable extension even if the model omits it.
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'%', b'P', b'D', b'F', ..] => Some("application/pdf"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'A', b'V', b'E', ..] => Some("audio/wav"),
        [0x50, 0x4B, 0x03, 0x04, ..] => Some("application/zip"),
        [_, _, _, _, b'f', b't', b'y', b'p', brand @ ..] if brand.len() >= 4 => {
            Some(match &brand[..4] {
                b"qt  " => "video/quicktime",
                b"M4A " | b"M4B " => "audio/mp4",
                b"heic" | b"heix" | b"mif1" => "image/heic",
                b"avif" => "image/avif",
                _ => "video/mp4",
            })
        }
        [0x1A, 0x45, 0xDF, 0xA3, ..] => Some("video/webm"),
        [b'O', b'g', b'g', b'S', ..] => Some("audio/ogg"),
        [b'f', b'L', b'a', b'C', ..] => Some("audio/flac"),
        [b'I', b'D', b'3', ..] => Some("audio/mpeg"),
        [0xFF, b, ..] if b & 0xE0 == 0xE0 && b & 0x06 != 0 => Some("audio/mpeg"),
        _ => None,
    }
}

/// File extension (no dot) for a MIME type, covering the staged-artifact and
/// [`sniff_mime`] types. The single source for naming a shipped-out file.
pub fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" => "heic",
        "image/avif" => "avif",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "text/csv" => "csv",
        "application/json" => "json",
        "text/plain" => "txt",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/mp4" => "m4a",
        "audio/ogg" | "audio/opus" => "ogg",
        "audio/flac" => "flac",
        _ => "bin",
    }
}

/// Read a staged artifact (owned by `agent_id`) and derive a MIME + default
/// filename for shipping it out as a file (Slack upload, email attachment,
/// browser file input). The MIME is the artifact's stored content type, or —
/// for opaque octets — sniffed from the leading bytes; callers override the
/// filename from their own input.
///
/// `agent_id` is the owning agent, NOT read from a task-local: a tool runs in a
/// `JoinSet::spawn`-ed task that may not carry `CURRENT_AGENT_ID`, so the caller
/// must pass the agent it was constructed for (as `generate_image` and the image
/// sink do).
pub fn resolve_staged_file(agent_id: &str, artifact_id: &str) -> eyre::Result<StagedFile> {
    use workforce_agent::artifacts::{
        global_store, resolve_bytes_input, ArtifactId, BytesRequest, ContentType,
    };
    if agent_id.is_empty() {
        return Err(eyre::eyre!(
            "no agent context to resolve artifact {artifact_id}"
        ));
    }
    let store = global_store().ok_or_else(|| eyre::eyre!("artifact store is not configured"))?;
    let resolved = resolve_bytes_input(
        store.as_ref(),
        agent_id,
        BytesRequest {
            artifact_id: Some(artifact_id),
            ..Default::default()
        },
    )?;
    let descriptor = ArtifactId::parse(artifact_id)
        .ok()
        .and_then(|id| store.metadata(agent_id, &id).ok().flatten());
    let mime = descriptor
        .as_ref()
        .and_then(|d| d.media_type.clone())
        .unwrap_or_else(|| match resolved.mime {
            ContentType::OctetStream => sniff_mime(&resolved.bytes)
                .unwrap_or(ContentType::OctetStream.as_str())
                .to_string(),
            typed => typed.as_str().to_string(),
        });
    let filename = descriptor
        .and_then(|d| d.filename)
        .unwrap_or_else(|| format!("file.{}", extension_for_mime(&mime)));
    Ok(StagedFile {
        bytes: resolved.bytes,
        mime,
        filename,
    })
}

/// Wrap a JSON string field with security boundary markers when it carries
/// content. Other JSON shapes pass through unchanged; `None` yields `null`.
pub fn wrap_field(value: Option<&Value>, source: &str) -> Value {
    match value.and_then(Value::as_str) {
        Some(s) if !s.is_empty() => Value::String(wrap_external_content(s, source).wrapped_content),
        _ => value.cloned().unwrap_or(Value::Null),
    }
}

/// Wrap `keys` in place inside `obj` under the shared `prefix`, leaving absent
/// or null keys untouched.
pub fn wrap_fields_in_place(obj: &mut Value, keys: &[&str], prefix: &str) {
    for &key in keys {
        if let Some(v) = obj.get(key).filter(|v| !v.is_null()).cloned() {
            obj[key] = wrap_field(Some(&v), &format!("{prefix}.{key}"));
        }
    }
}

/// Truncate `obj[key]` to `max_bytes` on a char boundary, appending a marker
/// that records the original size. Run before wrapping so the boundary
/// markers of the wrapper stay intact.
pub fn truncate_string_field(obj: &mut Value, key: &str, max_bytes: usize) {
    let Some(s) = obj.get(key).and_then(Value::as_str) else {
        return;
    };
    let original_len = s.len();
    if original_len <= max_bytes {
        return;
    }
    let end = (0..=max_bytes)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    obj[key] = Value::String(format!(
        "{}... [truncated; original length {original_len} bytes]",
        &s[..end]
    ));
}

pub fn truncate_string_fields(obj: &mut Value, keys: &[&str], max_bytes: usize) {
    for &key in keys {
        truncate_string_field(obj, key, max_bytes);
    }
}

/// A staged artifact resolved for an outbound email attachment. An inline
/// part carries a `content_id` the HTML body references as `cid:…`.
#[derive(Debug)]
pub struct StagedAttachment {
    pub name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
    pub content_id: Option<String>,
}

impl StagedAttachment {
    pub fn inline(&self) -> bool {
        self.content_id.is_some()
    }

    /// The bytes as the JSON mail APIs carry them.
    pub fn content_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.bytes)
    }
}

/// A content id made of the characters every mail client agrees on, so the
/// `Content-ID` header and the HTML's `cid:` reference stay the same text.
fn cid_safe(id: &str) -> String {
    id.chars()
        .map(|c| if is_cid_char(c) { c } else { '-' })
        .collect()
}

fn is_cid_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-')
}

/// Whether `html` references `cid` as a whole token, not as a prefix of a
/// longer id.
fn references(html: &str, cid: &str) -> bool {
    let needle = format!("cid:{cid}");
    html.match_indices(&needle)
        .any(|(at, _)| !html[at + needle.len()..].starts_with(is_cid_char))
}

/// Schema for the `attach_artifact_ids` input shared by the email adapters:
/// staged artifacts whose bytes are read from the store and base64-encoded
/// server-side, so they never pass through the model.
pub fn artifact_attachments_schema(attachments_field: &str) -> Value {
    json!({
        "type": "array",
        "description": format!("Staged artifacts to attach (from a chart / generate_image / export_xlsx / view_attachment result). The file bytes are read from the store and base64-encoded for you — do NOT paste base64 into `{attachments_field}`."),
        "items": { "type": "object", "properties": {
            "artifact_id":  { "type": "string", "description": "Artifact id (art_…) of the staged file." },
            "name":         { "type": "string", "description": "Filename for the attachment, e.g. 'report.xlsx'." },
            "content_type": { "type": "string", "description": "Optional MIME; derived from the artifact if omitted." },
            "inline":       { "type": "boolean", "description": "Embed an image in the HTML body instead of attaching it. Reference it as <img src=\"cid:CONTENT_ID\">; an unreferenced inline image is appended after the body." },
            "content_id":   { "type": "string", "description": "The cid the HTML references (implies inline). Defaults to the artifact id without its `art_` prefix." }
        }, "required": ["artifact_id"] }
    })
}

/// `text` as HTML: escaped, paragraphs on blank lines, breaks on the rest.
pub fn text_to_html(text: &str) -> String {
    text.split("\n\n")
        .map(|p| format!("<p>{}</p>", html_escape(p).replace('\n', "<br>")))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `html` with an `<img>` appended for every inline part it does not already
/// reference by `cid:`, so a chart a model attached inline is seen even when
/// it forgot the tag. `parts` are `(content_id, name)`.
pub fn html_with_inline_images<'a>(
    html: &str,
    parts: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> String {
    parts
        .into_iter()
        .filter(|(cid, _)| !references(html, cid))
        .fold(html.to_string(), |acc, (cid, name)| {
            format!(
                "{acc}\n<p><img src=\"cid:{}\" alt=\"{}\"></p>",
                cid_safe(cid),
                html_escape(name)
            )
        })
}

/// Resolve `attach_artifact_ids` into base64 attachments, enforcing
/// `max_total_bytes` of encoded content across every file (`limit_label`
/// names the provider's cap in the error). Empty when none are requested.
pub fn resolve_artifact_attachments(
    input: &Value,
    max_total_bytes: usize,
    limit_label: &str,
) -> Result<Vec<StagedAttachment>, String> {
    let Some(ids) = input
        .get("attach_artifact_ids")
        .and_then(Value::as_array)
        .filter(|ids| !ids.is_empty())
    else {
        return Ok(Vec::new());
    };
    let agent_id = workforce_agent::runner::CURRENT_AGENT_ID
        .try_with(|v| v.clone())
        .unwrap_or_default();
    let mut total = 0usize;
    ids.iter()
        .enumerate()
        .map(|(i, item)| {
            let id = non_empty(item, "artifact_id")
                .ok_or_else(|| format!("attach_artifact_ids[{i}] is missing 'artifact_id'"))?;
            let staged = resolve_staged_file(&agent_id, id)
                .map_err(|e| format!("attach_artifact_ids[{i}] ({id}): {e}"))?;
            total += (staged.bytes.len() * 4).div_ceil(3);
            if total > max_total_bytes {
                return Err(format!(
                    "attachments total {total} bytes, over {limit_label} — send fewer or smaller files"
                ));
            }
            let inline = item.get("inline") == Some(&Value::Bool(true));
            let content_id = non_empty(item, "content_id")
                .map(cid_safe)
                .or_else(|| inline.then(|| id.trim_start_matches("art_").to_string()));
            Ok(StagedAttachment {
                name: non_empty(item, "name")
                    .map(str::to_string)
                    .unwrap_or(staged.filename),
                content_type: non_empty(item, "content_type")
                    .map(str::to_string)
                    .unwrap_or(staged.mime),
                bytes: staged.bytes,
                content_id,
            })
        })
        .collect()
}

fn non_empty<'a>(item: &'a Value, key: &str) -> Option<&'a str> {
    item.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// A staged PNG for the adapters' attachment tests, in an `FsArtifactStore`
/// under a fresh temp dir, stamped with a filename and media type. Run the
/// code under test inside `CURRENT_ARTIFACT_STORE` and `CURRENT_AGENT_ID`
/// scopes for `AGENT`.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;

    use workforce_agent::artifacts::{
        ArtifactBody, ArtifactDescriptor, ArtifactId, ArtifactStore, ContentType, FsArtifactStore,
    };

    pub const AGENT: &str = "agent";

    pub fn staged_png(filename: &str) -> (Arc<dyn ArtifactStore>, ArtifactId, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FsArtifactStore::new(dir.path().to_path_buf()).expect("store");
        let descriptor = ArtifactDescriptor::from_context("chart", ContentType::OctetStream, AGENT)
            .with_file_meta(filename, "image/png");
        let id = store
            .write(AGENT, descriptor, ArtifactBody::Bytes(b"\x89PNG".to_vec()))
            .expect("write")
            .id;
        (store, id, dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- sniff_mime / extension_for_mime ----

    #[test]
    fn sniff_mime_detects_common_types() {
        assert_eq!(sniff_mime(b"\x89PNG\r\n"), Some("image/png"));
        assert_eq!(sniff_mime(b"\xFF\xD8\xFFxx"), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a"), Some("image/gif"));
        assert_eq!(sniff_mime(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(sniff_mime(b"RIFF\0\0\0\0WEBPxx"), Some("image/webp"));
        assert_eq!(sniff_mime(b"PK\x03\x04rest"), Some("application/zip"));
        assert_eq!(sniff_mime(b"\0\0\0\x20ftypisom\0\0"), Some("video/mp4"));
        assert_eq!(
            sniff_mime(b"\0\0\0\x14ftypqt  \0\0"),
            Some("video/quicktime")
        );
        assert_eq!(sniff_mime(b"\0\0\0\x20ftypM4A \0\0"), Some("audio/mp4"));
        assert_eq!(sniff_mime(b"\x1A\x45\xDF\xA3xx"), Some("video/webm"));
        assert_eq!(sniff_mime(b"ID3\x04"), Some("audio/mpeg"));
        assert_eq!(sniff_mime(b"\xFF\xFBxx"), Some("audio/mpeg"));
        assert_eq!(sniff_mime(b"RIFF\0\0\0\0WAVEfmt "), Some("audio/wav"));
        assert_eq!(sniff_mime(b"OggS\0"), Some("audio/ogg"));
        assert_eq!(sniff_mime(b"plain text"), None);
        assert_eq!(sniff_mime(b""), None);
    }

    #[test]
    fn extension_for_mime_maps_known_and_falls_back() {
        assert_eq!(extension_for_mime("image/png"), "png");
        assert_eq!(extension_for_mime("application/pdf"), "pdf");
        assert_eq!(extension_for_mime("text/csv"), "csv");
        assert_eq!(extension_for_mime("application/octet-stream"), "bin");
        assert_eq!(extension_for_mime("who/knows"), "bin");
        assert_eq!(extension_for_mime("video/quicktime"), "mov");
        assert_eq!(extension_for_mime("audio/mpeg"), "mp3");
    }

    // ---- coerce_to_id_string ----

    #[test]
    fn id_string_accepts_strings_and_non_negative_integers() {
        assert_eq!(coerce_to_id_string(&json!(" 2.1 ")), Some("2.1".into()));
        assert_eq!(coerce_to_id_string(&json!("")), Some(String::new()));
        assert_eq!(coerce_to_id_string(&json!(3)), Some("3".into()));
        assert_eq!(coerce_to_id_string(&json!(3.0)), Some("3".into()));
        assert_eq!(coerce_to_id_string(&json!(-1)), None);
        assert_eq!(coerce_to_id_string(&json!(null)), None);
        assert_eq!(coerce_to_id_string(&json!(["1"])), None);
    }

    // ---- coerce_to_u64 ----

    #[test]
    fn u64_native_integer() {
        assert_eq!(coerce_to_u64(&json!(42)), Some(42));
        assert_eq!(coerce_to_u64(&json!(0)), Some(0));
        assert_eq!(coerce_to_u64(&json!(u64::MAX)), Some(u64::MAX));
    }

    #[test]
    fn u64_from_string() {
        assert_eq!(coerce_to_u64(&json!("71")), Some(71));
        assert_eq!(coerce_to_u64(&json!(" 71 ")), Some(71));
        assert_eq!(coerce_to_u64(&json!("0")), Some(0));
        assert_eq!(coerce_to_u64(&json!("999999")), Some(999999));
    }

    #[test]
    fn u64_from_float() {
        assert_eq!(coerce_to_u64(&json!(71.0)), Some(71));
        assert_eq!(coerce_to_u64(&json!(0.0)), Some(0));
        assert_eq!(coerce_to_u64(&json!(100.0)), Some(100));
    }

    #[test]
    fn u64_rejects_negative() {
        assert_eq!(coerce_to_u64(&json!(-1)), None);
        assert_eq!(coerce_to_u64(&json!("-1")), None);
        assert_eq!(coerce_to_u64(&json!(-1.0)), None);
    }

    #[test]
    fn u64_rejects_fractional() {
        assert_eq!(coerce_to_u64(&json!(71.5)), None);
        assert_eq!(coerce_to_u64(&json!(0.1)), None);
    }

    #[test]
    fn u64_rejects_non_numeric() {
        assert_eq!(coerce_to_u64(&json!("abc")), None);
        assert_eq!(coerce_to_u64(&json!("")), None);
        assert_eq!(coerce_to_u64(&json!(null)), None);
        assert_eq!(coerce_to_u64(&json!(true)), None);
        assert_eq!(coerce_to_u64(&json!(false)), None);
        assert_eq!(coerce_to_u64(&json!([])), None);
        assert_eq!(coerce_to_u64(&json!({})), None);
    }

    #[test]
    fn u64_rejects_infinity_and_nan() {
        assert_eq!(coerce_to_u64(&json!(f64::INFINITY)), None);
        assert_eq!(coerce_to_u64(&json!(f64::NEG_INFINITY)), None);
        assert_eq!(coerce_to_u64(&json!(f64::NAN)), None);
    }

    // ---- coerce_to_i64 ----

    #[test]
    fn i64_native_integer() {
        assert_eq!(coerce_to_i64(&json!(42)), Some(42));
        assert_eq!(coerce_to_i64(&json!(-42)), Some(-42));
        assert_eq!(coerce_to_i64(&json!(0)), Some(0));
        assert_eq!(coerce_to_i64(&json!(i64::MAX)), Some(i64::MAX));
        assert_eq!(coerce_to_i64(&json!(i64::MIN)), Some(i64::MIN));
    }

    #[test]
    fn i64_from_string() {
        assert_eq!(coerce_to_i64(&json!("71")), Some(71));
        assert_eq!(coerce_to_i64(&json!(" -5 ")), Some(-5));
        assert_eq!(coerce_to_i64(&json!("0")), Some(0));
        assert_eq!(coerce_to_i64(&json!("-999")), Some(-999));
    }

    #[test]
    fn i64_from_float() {
        assert_eq!(coerce_to_i64(&json!(71.0)), Some(71));
        assert_eq!(coerce_to_i64(&json!(-5.0)), Some(-5));
        assert_eq!(coerce_to_i64(&json!(0.0)), Some(0));
    }

    #[test]
    fn i64_rejects_fractional() {
        assert_eq!(coerce_to_i64(&json!(71.5)), None);
        assert_eq!(coerce_to_i64(&json!(-0.5)), None);
    }

    #[test]
    fn i64_rejects_non_numeric() {
        assert_eq!(coerce_to_i64(&json!("abc")), None);
        assert_eq!(coerce_to_i64(&json!("")), None);
        assert_eq!(coerce_to_i64(&json!(null)), None);
        assert_eq!(coerce_to_i64(&json!(true)), None);
        assert_eq!(coerce_to_i64(&json!(false)), None);
        assert_eq!(coerce_to_i64(&json!([])), None);
        assert_eq!(coerce_to_i64(&json!({})), None);
    }

    #[test]
    fn i64_rejects_infinity_and_nan() {
        assert_eq!(coerce_to_i64(&json!(f64::INFINITY)), None);
        assert_eq!(coerce_to_i64(&json!(f64::NEG_INFINITY)), None);
        assert_eq!(coerce_to_i64(&json!(f64::NAN)), None);
    }

    // ---- Backward compatibility ----

    #[test]
    fn backward_compat_unwrap_or_pattern() {
        // Simulates: input["limit"].as_u64().unwrap_or(20) → coerce_to_u64(...).unwrap_or(20)
        let input = json!({"limit": 50});
        assert_eq!(coerce_to_u64(&input["limit"]).unwrap_or(20), 50);

        let input = json!({"limit": "50"});
        assert_eq!(coerce_to_u64(&input["limit"]).unwrap_or(20), 50);

        let input = json!({});
        assert_eq!(coerce_to_u64(&input["limit"]).unwrap_or(20), 20);
    }

    #[test]
    fn backward_compat_match_pattern() {
        // Simulates: match input["number"].as_u64() { Some(n) => ..., None => error }
        let input = json!({"number": 71});
        assert!(coerce_to_u64(&input["number"]).is_some());

        let input = json!({"number": "71"});
        assert!(coerce_to_u64(&input["number"]).is_some());

        let input = json!({"number": 71.0});
        assert!(coerce_to_u64(&input["number"]).is_some());

        let input = json!({});
        assert!(coerce_to_u64(&input["number"]).is_none());
    }

    // ---- Boolean coercion (LLMs sometimes pass true/false) ----

    #[test]
    fn u64_rejects_boolean_true() {
        // serde_json: json!(true).as_u64() returns None, our fn should too
        assert_eq!(coerce_to_u64(&json!(true)), None);
    }

    #[test]
    fn i64_rejects_boolean() {
        assert_eq!(coerce_to_i64(&json!(true)), None);
        assert_eq!(coerce_to_i64(&json!(false)), None);
    }

    // ---- decode_upload_content ----

    #[test]
    fn upload_content_text() {
        assert_eq!(
            decode_upload_content(Some("hello"), None).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn upload_content_base64() {
        let b64 = base64::engine::general_purpose::STANDARD.encode("hello world");
        assert_eq!(
            decode_upload_content(None, Some(&b64)).unwrap(),
            b"hello world"
        );
    }

    #[test]
    fn upload_content_base64_tolerates_line_wraps() {
        let raw = base64::engine::general_purpose::STANDARD.encode("x".repeat(200).as_bytes());
        let wrapped = raw
            .as_bytes()
            .chunks(76)
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(wrapped.contains('\n'));
        assert_eq!(
            decode_upload_content(None, Some(&wrapped)).unwrap().len(),
            200
        );
    }

    #[test]
    fn upload_content_rejects_both() {
        let err = decode_upload_content(Some("hi"), Some("aGk=")).unwrap_err();
        assert!(err.contains("exactly one"));
    }

    #[test]
    fn upload_content_rejects_neither() {
        let err = decode_upload_content(None, None).unwrap_err();
        assert!(err.contains("provide one of"));
    }

    #[test]
    fn upload_content_rejects_invalid_base64() {
        let err = decode_upload_content(None, Some("!!!not-base64!!!")).unwrap_err();
        assert!(err.contains("not valid base64"));
    }

    #[test]
    fn upload_content_rejects_empty() {
        assert!(decode_upload_content(Some(""), None)
            .unwrap_err()
            .contains("empty"));
        assert!(decode_upload_content(None, Some(""))
            .unwrap_err()
            .contains("empty"));
    }

    #[test]
    fn wrap_field_passes_through_non_strings() {
        assert_eq!(wrap_field(None, "src"), Value::Null);
        assert_eq!(wrap_field(Some(&json!(42)), "src"), json!(42));
        assert_eq!(wrap_field(Some(&json!(true)), "src"), json!(true));
    }

    #[test]
    fn wrap_field_wraps_non_empty_string() {
        let out = wrap_field(Some(&json!("hello")), "test");
        let s = out.as_str().expect("wrapped to string");
        assert!(s.contains("hello"));
        assert_ne!(s, "hello");
    }

    #[test]
    fn wrap_field_leaves_empty_string_alone() {
        assert_eq!(wrap_field(Some(&json!("")), "src"), json!(""));
    }

    #[test]
    fn wrap_fields_in_place_skips_absent_and_null_keys() {
        let mut obj = json!({ "Subject": "hi", "Null": null });
        wrap_fields_in_place(&mut obj, &["Subject", "Null", "Missing"], "t");
        assert!(obj["Subject"].as_str().unwrap().contains("hi"));
        assert_eq!(obj["Null"], Value::Null);
        assert!(obj.get("Missing").is_none());
    }

    #[test]
    fn truncate_string_field_leaves_short_and_exact_strings_alone() {
        let mut obj = json!({ "HtmlBody": "<p>hi</p>", "Body": "x".repeat(10) });
        truncate_string_field(&mut obj, "HtmlBody", 100);
        truncate_string_field(&mut obj, "Body", 10);
        assert_eq!(obj["HtmlBody"], json!("<p>hi</p>"));
        assert_eq!(obj["Body"].as_str().unwrap().len(), 10);
    }

    #[test]
    fn truncate_string_field_skips_absent_null_and_non_string() {
        let mut obj = json!({ "TextBody": null, "Other": 42 });
        truncate_string_field(&mut obj, "TextBody", 10);
        truncate_string_field(&mut obj, "Other", 10);
        truncate_string_field(&mut obj, "Missing", 10);
        assert_eq!(obj["TextBody"], Value::Null);
        assert_eq!(obj["Other"], json!(42));
        assert!(obj.get("Missing").is_none());
    }

    #[test]
    fn truncate_string_field_truncates_oversized_strings_with_marker() {
        let mut obj = json!({ "Body": "a".repeat(50_000) });
        truncate_string_field(&mut obj, "Body", 100);
        let out = obj["Body"].as_str().unwrap();
        assert!(out.starts_with(&"a".repeat(100)));
        assert!(out.contains("truncated; original length 50000 bytes"));
        assert!(out.len() < 500);
    }

    #[test]
    fn truncate_string_field_respects_char_boundaries() {
        let mut obj = json!({ "Body": format!("aaaa{}aaaa", "💀") });
        truncate_string_field(&mut obj, "Body", 5);
        let out = obj["Body"].as_str().unwrap();
        assert!(out.starts_with("aaaa..."));
        assert!(out.contains("truncated"));
    }

    #[test]
    fn truncate_string_field_handles_zero_cap() {
        let mut obj = json!({ "Body": "anything" });
        truncate_string_field(&mut obj, "Body", 0);
        let out = obj["Body"].as_str().unwrap();
        assert!(out.starts_with("..."));
        assert!(out.contains("original length 8 bytes"));
    }

    #[test]
    fn truncate_string_fields_applies_to_each_key() {
        let mut obj = json!({
            "HtmlBody": "h".repeat(50),
            "TextBody": "t".repeat(50),
            "Other":    "untouched",
        });
        truncate_string_fields(&mut obj, &["HtmlBody", "TextBody"], 10);
        assert!(obj["HtmlBody"].as_str().unwrap().contains("truncated"));
        assert!(obj["TextBody"].as_str().unwrap().contains("truncated"));
        assert_eq!(obj["Other"], json!("untouched"));
    }

    #[test]
    fn resolve_artifact_attachments_empty_when_absent() {
        assert!(resolve_artifact_attachments(&json!({}), 1, "the cap")
            .unwrap()
            .is_empty());
        assert!(
            resolve_artifact_attachments(&json!({ "attach_artifact_ids": [] }), 1, "the cap")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn html_with_inline_images_appends_unreferenced_parts_only() {
        let html = "<p>See <img src=\"cid:q3\"> and <img src='cid:q3b'></p>";
        let out =
            html_with_inline_images(html, [("q3", "q3.png"), ("q3b", "b.png"), ("q", "<x>.png")]);
        assert_eq!(
            out,
            "<p>See <img src=\"cid:q3\"> and <img src='cid:q3b'></p>\n<p><img src=\"cid:q\" alt=\"&lt;x&gt;.png\"></p>"
        );
        assert_eq!(html_with_inline_images(html, [("q3", "q3.png")]), html);
    }

    #[test]
    fn content_ids_are_made_header_safe() {
        assert_eq!(cid_safe("a&b <c>"), "a-b--c-");
        assert_eq!(cid_safe("chart_1@wf.local"), "chart_1@wf.local");
    }

    #[test]
    fn text_to_html_escapes_and_keeps_paragraphs() {
        assert_eq!(
            text_to_html("Hi <you>\nline two\n\nBye & thanks"),
            "<p>Hi &lt;you&gt;<br>line two</p>\n<p>Bye &amp; thanks</p>"
        );
    }

    #[test]
    fn resolve_artifact_attachments_reads_the_stored_meta_and_derives_inline_cids() {
        use super::test_support::{staged_png, AGENT};
        use workforce_agent::runner::{CURRENT_AGENT_ID, CURRENT_ARTIFACT_STORE};

        let (store, id, _dir) = staged_png("signups.png");
        let input = json!({ "attach_artifact_ids": [
            { "artifact_id": id.as_str(), "inline": true },
            { "artifact_id": id.as_str(), "content_id": "hero <1>", "name": "hero.png" },
            { "artifact_id": id.as_str() },
        ] });
        let parts = CURRENT_ARTIFACT_STORE.sync_scope(store, || {
            CURRENT_AGENT_ID.sync_scope(AGENT.to_string(), || {
                resolve_artifact_attachments(&input, 1 << 20, "the cap").expect("resolves")
            })
        });
        assert_eq!(parts[0].name, "signups.png");
        assert_eq!(
            parts[0].content_type, "image/png",
            "the stamped media type, not a sniff"
        );
        assert_eq!(parts[0].bytes, b"\x89PNG");
        assert_eq!(
            parts[0].content_id.as_deref(),
            Some(id.as_str().trim_start_matches("art_"))
        );
        assert_eq!(parts[1].content_id.as_deref(), Some("hero--1-"));
        assert_eq!(parts[1].name, "hero.png");
        assert!(!parts[2].inline());
    }

    #[test]
    fn resolve_artifact_attachments_errors_on_missing_id() {
        let input = json!({ "attach_artifact_ids": [{ "name": "x.png" }] });
        let err = resolve_artifact_attachments(&input, 1, "the cap").unwrap_err();
        assert!(err.contains("missing 'artifact_id'"), "{err}");
    }

    fn instances(names: &[&str]) -> HashMap<String, ()> {
        names.iter().map(|n| (n.to_string(), ())).collect()
    }

    #[test]
    fn preferred_instance_honours_a_configured_selector_that_exists() {
        let clients = instances(&["default", "acme"]);
        assert_eq!(preferred_instance(&clients, Some("acme")), Some("acme"));
        assert_eq!(preferred_instance(&clients, None), Some("default"));
    }

    #[test]
    fn preferred_instance_falls_from_a_stale_selector_to_default_then_the_sole_instance() {
        assert_eq!(
            preferred_instance(&instances(&["default", "acme"]), Some("gone")),
            Some("default")
        );
        assert_eq!(
            preferred_instance(&instances(&["harbor"]), Some("gone")),
            Some("harbor")
        );
        assert_eq!(
            preferred_instance(&instances(&["harbor"]), None),
            Some("harbor")
        );
    }

    #[test]
    fn preferred_instance_is_none_when_nothing_selects_one() {
        assert!(preferred_instance(&instances(&["acme", "client"]), None).is_none());
        assert!(preferred_instance(&instances(&["acme", "client"]), Some("gone")).is_none());
        assert!(preferred_instance(&instances(&[]), None).is_none());
    }

    #[test]
    fn a_blank_selector_counts_as_none() {
        assert_eq!(
            preferred_instance(&instances(&["harbor"]), Some("  ")),
            Some("harbor")
        );
        assert_eq!(
            default_instance("t", &instances(&["acme", "client"]), Some(" ")),
            "default"
        );
        assert_eq!(
            default_instance("t", &instances(&["acme", "client"]), Some(" acme ")),
            "acme"
        );
    }

    #[test]
    fn default_instance_follows_the_rule_and_keeps_the_configured_name_last() {
        assert_eq!(
            default_instance("t", &instances(&["default", "acme"]), Some("acme")),
            "acme"
        );
        assert_eq!(
            default_instance("t", &instances(&["default", "acme"]), Some("gone")),
            "default"
        );
        assert_eq!(
            default_instance("t", &instances(&["harbor"]), Some("gone")),
            "harbor"
        );
        assert_eq!(
            default_instance("t", &instances(&["acme", "client"]), None),
            "default"
        );
        assert_eq!(
            default_instance("t", &instances(&["acme", "client"]), Some("gone")),
            "gone"
        );
    }

    #[test]
    fn build_instances_builds_every_instance_and_picks_the_default() {
        let resolved = vec![("acme".to_string(), 1u8)];
        let built = build_instances("t", &resolved, None, "nothing", |name, n| {
            Ok(format!("{name}{n}"))
        })
        .unwrap();
        assert_eq!(built.default, "acme");
        assert_eq!(*built.clients["acme"], "acme1");

        let none: [(String, u8); 0] = [];
        let err = build_instances("t", &none, None, "nothing", |_, _| Ok(()))
            .err()
            .expect("nothing configured");
        assert_eq!(err.to_string(), "nothing");

        let err = build_instances("t", &resolved, None, "nothing", |_, _| {
            Err::<(), _>(eyre::eyre!("boom"))
        })
        .err()
        .expect("make failed");
        assert_eq!(err.to_string(), "boom");
    }

    #[test]
    fn instance_choices_sorts_names_and_names_an_absent_default() {
        let clients = instances(&["client", "acme"]);
        assert_eq!(
            instance_choices(&clients, "acme", "workspace"),
            "Available: acme, client. Default: acme."
        );
        assert_eq!(
            instance_choices(&clients, "default", "workspace"),
            "Available: acme, client. Default: none — pass workspace."
        );
    }
}
