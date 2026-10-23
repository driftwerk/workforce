//! A scripted language server for tests: it speaks JSON-RPC over the same
//! channels a real one's stdio arrives on, renames by whole words across the
//! files it is told make up the project, and reports a diagnostic on every
//! line that says `ERROR`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_util::task::AbortOnDropHandle;

use super::edit::{offset, position, Encoding};
use super::rpc::take_message;
use super::server::{path_of, uri_of, Launched, Launcher, ServerSpec};

#[derive(Default)]
pub struct Script {
    /// The project's files, read from disk when asked to rename.
    pub files: Vec<PathBuf>,
    /// Never report having loaded the project.
    pub never_quiescent: bool,
    /// Never answer a rename.
    pub hang_on_rename: bool,
    /// Answer a rename with this instead.
    pub rename_answer: Option<Value>,
    /// Fail to start, as a missing binary does.
    pub missing: bool,
    /// Locations added to every references answer.
    pub extra_locations: Vec<Value>,
}

#[derive(Default)]
pub struct FakeLauncher {
    pub script: Script,
    pub launches: AtomicUsize,
    pub renames: Arc<AtomicUsize>,
    /// Every document the servers were shown, by uri.
    pub shown: Arc<Mutex<Vec<String>>>,
}

impl FakeLauncher {
    pub fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script,
            ..Self::default()
        })
    }

    pub fn launches(&self) -> usize {
        self.launches.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Launcher for FakeLauncher {
    async fn launch(&self, _spec: &ServerSpec) -> Result<Launched, String> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        let (stdin, from_client) = mpsc::channel::<String>(64);
        let (to_client, stdout) = mpsc::channel::<String>(64);
        let (ended_tx, ended) = oneshot::channel();
        if self.script.missing {
            let _ = ended_tx.send("Failed to spawn command: No such file or directory".into());
            return Ok(Launched {
                stdin,
                stdout,
                ended,
                process: None,
            });
        }
        let server = Server {
            files: self.script.files.clone(),
            never_quiescent: self.script.never_quiescent,
            hang_on_rename: self.script.hang_on_rename,
            rename_answer: self.script.rename_answer.clone(),
            extra_locations: self.script.extra_locations.clone(),
            open: Mutex::new(HashMap::new()),
            renames: Arc::clone(&self.renames),
            shown: Arc::clone(&self.shown),
        };
        let task = tokio::spawn(async move {
            server.serve(from_client, to_client).await;
            let _ = ended_tx.send("it exited with code 0".into());
        });
        Ok(Launched {
            stdin,
            stdout,
            ended,
            process: Some(AbortOnDropHandle::new(task)),
        })
    }
}

struct Server {
    files: Vec<PathBuf>,
    never_quiescent: bool,
    hang_on_rename: bool,
    rename_answer: Option<Value>,
    extra_locations: Vec<Value>,
    open: Mutex<HashMap<String, (i64, String)>>,
    renames: Arc<AtomicUsize>,
    shown: Arc<Mutex<Vec<String>>>,
}

fn frame(message: &Value) -> String {
    let body = message.to_string();
    format!("Content-Length: {}\r\n\r\n{body}", body.len())
}

fn word_at(text: &str, at: usize) -> &str {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let start = text[..at].rfind(|c: char| !is_word(c)).map_or(0, |i| i + 1);
    let end = text[at..]
        .find(|c: char| !is_word(c))
        .map_or(text.len(), |i| at + i);
    &text[start..end]
}

fn occurrences(text: &str, word: &str) -> Vec<usize> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(word)
        .map(|(at, _)| at)
        .filter(|&at| {
            !text[..at].chars().next_back().is_some_and(is_word)
                && !text[at + word.len()..].chars().next().is_some_and(is_word)
        })
        .collect()
}

impl Server {
    async fn serve(&self, mut input: mpsc::Receiver<String>, out: mpsc::Sender<String>) {
        let mut buffer = Vec::new();
        while let Some(chunk) = input.recv().await {
            buffer.extend_from_slice(chunk.as_bytes());
            while let Ok(Some(message)) = take_message(&mut buffer) {
                let method = message["method"].as_str().unwrap_or("");
                if method == "exit" {
                    return;
                }
                for reply in self.answer(method, &message) {
                    if out.send(frame(&reply)).await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    fn text_of(&self, uri: &str) -> Option<String> {
        self.open
            .lock()
            .get(uri)
            .map(|(_, text)| text.clone())
            .or_else(|| std::fs::read_to_string(path_of(uri)?).ok())
    }

    fn answer(&self, method: &str, message: &Value) -> Vec<Value> {
        let id = message.get("id").cloned();
        let params = &message["params"];
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        let at = |text: &str| offset(text, &params["position"], Encoding::Utf8).unwrap_or(0);
        match method {
            "initialize" => vec![reply(
                json!({"capabilities": {"positionEncoding": "utf-8"}}),
            )],
            "initialized" => [false, !self.never_quiescent]
                .map(|quiescent| {
                    json!({"jsonrpc": "2.0", "method": "experimental/serverStatus",
                        "params": {"health": "ok", "quiescent": quiescent}})
                })
                .to_vec(),
            "textDocument/didOpen" => {
                let document = &params["textDocument"];
                let uri = document["uri"].as_str().unwrap_or("").to_string();
                let text = document["text"].as_str().unwrap_or("").to_string();
                let version = document["version"].as_i64().unwrap_or(0);
                let diagnostics: Vec<Value> = text
                    .lines()
                    .enumerate()
                    .filter(|(_, line)| line.contains("ERROR"))
                    .map(|(line, _)| json!({
                        "range": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 5}},
                        "severity": 1, "message": "an error here", "source": "fake",
                    }))
                    .collect();
                self.shown.lock().push(uri.clone());
                self.open.lock().insert(uri.clone(), (version, text));
                vec![
                    json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                    "params": {"uri": uri, "version": version, "diagnostics": diagnostics}}),
                ]
            }
            "textDocument/didClose" => {
                if let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) {
                    self.open.lock().remove(uri);
                }
                Vec::new()
            }
            "textDocument/rename" => {
                self.renames.fetch_add(1, Ordering::SeqCst);
                if self.hang_on_rename {
                    return Vec::new();
                }
                if let Some(answer) = &self.rename_answer {
                    return vec![reply(answer.clone())];
                }
                let uri = params
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let Some(text) = self.text_of(uri) else {
                    return vec![reply(Value::Null)];
                };
                let old = word_at(&text, at(&text)).to_string();
                let new_name = params["newName"].as_str().unwrap_or("");
                let changes: Vec<Value> = self
                    .files
                    .iter()
                    .map(|path| uri_of(path))
                    .filter_map(|file| {
                        let text = self.text_of(&file)?;
                        let version = self.open.lock().get(&file).map(|(v, _)| *v);
                        let edits: Vec<Value> = occurrences(&text, &old)
                            .into_iter()
                            .map(|at| json!({
                                "range": {"start": position(&text, at, Encoding::Utf8),
                                          "end": position(&text, at + old.len(), Encoding::Utf8)},
                                "newText": new_name,
                            }))
                            .collect();
                        (!edits.is_empty()).then(|| {
                            json!({
                                "textDocument": {"uri": file, "version": version},
                                "edits": edits,
                            })
                        })
                    })
                    .collect();
                vec![reply(json!({"documentChanges": changes}))]
            }
            "textDocument/references" | "textDocument/definition" => {
                let uri = params
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let Some(text) = self.text_of(uri) else {
                    return vec![reply(Value::Null)];
                };
                let word = word_at(&text, at(&text)).to_string();
                let mut found: Vec<Value> = self
                    .files
                    .iter()
                    .map(|path| uri_of(path))
                    .flat_map(|file| {
                        let text = self.text_of(&file).unwrap_or_default();
                        occurrences(&text, &word)
                            .into_iter()
                            .map(|at| {
                                json!({"uri": file, "range": {
                                "start": position(&text, at, Encoding::Utf8),
                                "end": position(&text, at + word.len(), Encoding::Utf8)}})
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect();
                found.push(json!({"uri": "file:///outside/lib.rs", "range": {
                    "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}));
                found.extend(self.extra_locations.iter().cloned());
                if method == "textDocument/definition" {
                    found.truncate(1);
                }
                vec![reply(Value::Array(found))]
            }
            "shutdown" => vec![reply(Value::Null)],
            _ if id.is_some() => vec![reply(Value::Null)],
            _ => Vec::new(),
        }
    }
}
