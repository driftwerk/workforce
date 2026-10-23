//! Thin HTTP client for the `workforce training` CLI remote mode.
//!
//! Mirrors the four endpoints exposed by `crate::training_handlers`
//! in the hub crate. Talks JSON in, JSON out, plus multipart for the
//! upload. Auth is always the owner token (`Authorization: Bearer …`
//! header), matching `require_admin_action` on the hub side.

use eyre::{eyre, Result, WrapErr};
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde_json::Value;
use workforce_training::{sanitize_filename, validate_slug};

use super::format::{render_doc_table, DocRow};
use super::{DeleteArgs, DocDeleteArgs, DocsArgs, ListArgs, LoadArgs};
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Stateless client. One per CLI invocation — `reqwest::Client` is
/// cheap to construct and we don't pool across subcommands.
pub struct RemoteClient {
    base: String,
    token: String,
    http: Client,
}

impl RemoteClient {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base: trim_trailing_slash(base.into()),
            token: token.into(),
            http: Client::builder()
                .build()
                .expect("reqwest::Client builds with default config"),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
}

fn trim_trailing_slash(mut s: String) -> String {
    while s.ends_with('/') {
        s.pop();
    }
    s
}

pub async fn load(client: &RemoteClient, args: &LoadArgs) -> Result<()> {
    validate_slug(&args.agent).map_err(|e| eyre!("--agent {e}"))?;
    validate_slug(&args.corpus).map_err(|e| eyre!("--corpus {e}"))?;
    if !args.file.is_file() {
        return Err(eyre!("not a file: {}", args.file.display()));
    }
    let bytes =
        std::fs::read(&args.file).wrap_err_with(|| format!("read {}", args.file.display()))?;
    let raw_filename = args
        .file
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| args.file.display().to_string());
    let filename = sanitize_filename(&raw_filename);

    let file_part = Part::bytes(bytes)
        .file_name(filename.clone())
        .mime_str("application/octet-stream")
        .expect("octet-stream is a valid MIME literal");
    let form = Form::new()
        .text("corpus", args.corpus.clone())
        .part("file", file_part);

    let path = format!("/api/agents/{}/training/corpora", args.agent);
    let resp = client
        .http
        .post(client.url(&path))
        .bearer_auth(&client.token)
        .multipart(form)
        .send()
        .await
        .wrap_err("send upload request")?;
    let body = read_json_or_err(resp, "training upload").await?;
    print_load_outcome(&filename, &args.corpus, &body);
    Ok(())
}

pub async fn list(client: &RemoteClient, args: &ListArgs) -> Result<()> {
    validate_slug(&args.agent).map_err(|e| eyre!("--agent {e}"))?;
    let path = format!("/api/agents/{}/training/corpora", args.agent);
    let resp = client
        .http
        .get(client.url(&path))
        .bearer_auth(&client.token)
        .send()
        .await
        .wrap_err("send list request")?;
    let body = read_json_or_err(resp, "training list").await?;
    let corpora = body
        .get("corpora")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if corpora.is_empty() {
        println!("no training corpora for `{}` (remote)", args.agent);
        return Ok(());
    }
    for c in corpora {
        println!(
            "{:<32}  {} doc(s)  {} chunk(s)  created {}",
            c.get("name").and_then(Value::as_str).unwrap_or("?"),
            c.get("document_count").and_then(Value::as_u64).unwrap_or(0),
            c.get("chunk_count").and_then(Value::as_u64).unwrap_or(0),
            c.get("created_at").and_then(Value::as_str).unwrap_or("?"),
        );
    }
    Ok(())
}

pub async fn docs(client: &RemoteClient, args: &DocsArgs) -> Result<()> {
    validate_slug(&args.agent).map_err(|e| eyre!("--agent {e}"))?;
    validate_slug(&args.corpus).map_err(|e| eyre!("--corpus {e}"))?;
    let path = format!(
        "/api/agents/{}/training/corpora/{}",
        args.agent, args.corpus,
    );
    let resp = client
        .http
        .get(client.url(&path))
        .bearer_auth(&client.token)
        .send()
        .await
        .wrap_err("send docs request")?;
    let body = read_json_or_err(resp, "training docs").await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }

    let rows = doc_rows_from_corpus_envelope(&body);
    println!("{}", render_doc_table(&args.agent, &args.corpus, &rows));
    Ok(())
}

fn doc_rows_from_corpus_envelope(body: &Value) -> Vec<DocRow> {
    body.get("documents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(doc_row_from_json)
        .collect()
}

fn doc_row_from_json(value: &Value) -> DocRow {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::nil);
    let short_id = value
        .get("short_id")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    let filename = value
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    let size_bytes = value.get("size_bytes").and_then(Value::as_i64).unwrap_or(0);
    let chunk_count = value
        .get("chunk_count")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let status = value
        .get("processing_status")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    let uploaded_at = value
        .get("uploaded_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    DocRow {
        id,
        short_id,
        filename,
        size_bytes,
        chunk_count,
        status,
        uploaded_at,
    }
}

pub async fn delete(client: &RemoteClient, args: &DeleteArgs) -> Result<()> {
    validate_slug(&args.agent).map_err(|e| eyre!("--agent {e}"))?;
    validate_slug(&args.corpus).map_err(|e| eyre!("--corpus {e}"))?;
    let path = format!(
        "/api/agents/{}/training/corpora/{}",
        args.agent, args.corpus,
    );
    let resp = client
        .http
        .delete(client.url(&path))
        .bearer_auth(&client.token)
        .send()
        .await
        .wrap_err("send delete request")?;
    let body = read_json_or_err(resp, "training delete").await?;
    println!(
        "deleted corpus `{}` for `{}` (remote: {} chunk(s) removed, {} memory row(s) forgotten, {} file(s) unlinked)",
        args.corpus,
        args.agent,
        body.get("chunks_removed")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        body.get("memories_forgotten")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        body.get("files_unlinked")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    Ok(())
}

pub async fn doc_delete(client: &RemoteClient, args: &DocDeleteArgs) -> Result<()> {
    validate_slug(&args.agent).map_err(|e| eyre!("--agent {e}"))?;
    validate_slug(&args.corpus).map_err(|e| eyre!("--corpus {e}"))?;
    let doc_ref = args.doc.trim();
    if doc_ref.is_empty() {
        return Err(eyre!("--doc is empty"));
    }
    // Remote mode requires the full UUID — short-id disambiguation needs the
    // store the remote owns, so the hub can't safely resolve it for us.
    let doc_id = uuid::Uuid::parse_str(doc_ref).map_err(|_| {
        eyre!(
            "remote mode needs the full UUID — get it from `workforce training list --agent {} --gateway ...` then use the `id` field, or pass a 36-char UUID directly",
            args.agent,
        )
    })?;

    let path = format!("/api/agents/{}/training/documents/{}", args.agent, doc_id,);
    let resp = client
        .http
        .delete(client.url(&path))
        .bearer_auth(&client.token)
        .send()
        .await
        .wrap_err("send delete request")?;
    let body = read_json_or_err(resp, "training doc-delete").await?;
    println!(
        "deleted document `{}` from `{}/{}` (remote: {} chunk(s) removed, {} memory row(s) forgotten, {} file(s) unlinked)",
        doc_id,
        args.agent,
        args.corpus,
        body.get("chunks_removed")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        body.get("memories_forgotten")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        body.get("files_unlinked")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    );
    Ok(())
}

fn print_load_outcome(filename: &str, corpus: &str, body: &Value) {
    let written = body.get("written").and_then(Value::as_u64).unwrap_or(0);
    let unchanged = body.get("unchanged").and_then(Value::as_u64).unwrap_or(0);
    let deleted = body.get("deleted").and_then(Value::as_u64).unwrap_or(0);
    println!(
        "loaded {} into `{}` (remote): {} written, {} unchanged, {} deleted",
        filename, corpus, written, unchanged, deleted
    );
    if let Some(doc_id) = body.get("document_id").and_then(Value::as_str) {
        println!("  document id: {doc_id}");
    }
}

async fn read_json_or_err(resp: reqwest::Response, op: &str) -> Result<Value> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .wrap_err_with(|| format!("read {op} response body"))?;
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| text.clone());
        return Err(eyre!("{op} failed ({status}): {detail}"));
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).wrap_err_with(|| format!("parse {op} response: {text}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_trailing_slash_normalises_base_url() {
        assert_eq!(
            trim_trailing_slash("https://x.test/".into()),
            "https://x.test"
        );
        assert_eq!(
            trim_trailing_slash("https://x.test//".into()),
            "https://x.test"
        );
        assert_eq!(
            trim_trailing_slash("https://x.test".into()),
            "https://x.test"
        );
    }
}
