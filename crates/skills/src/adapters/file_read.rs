//! A file read a chunk at a time, for Desk's board sync: a ticket's Jira
//! attachment or Linear upload, fetched as the person whose board it is,
//! and answered in pieces small enough for an MCP `tools/call` result (which
//! a client caps at 1 MB).
//!
//! The file is downloaded once, up to [`FILE_MAX`], and kept for a short
//! while under the credential it was read with ([`Memo`]): the next chunk is
//! served from there, and a caller with another credential never sees it.
//! Files are kept up to [`KEPT_BYTES`] together, so several read at once
//! each stay for their chunks; callers missing the same file at once wait
//! for one download ([`Memo::fetch_once`]).
//!
//! An answer is `{kind: "file_bytes", media_type, size, offset, length,
//! data}`, `data` the chunk's bytes in base64: bytes for a program, not
//! words for a model. No turn is offered these tools
//! (`workforce_agent::tools::access::PROGRAM_ONLY_TOOLS`), and only the
//! board sync's gated route leaves a chunk unstaged by its `kind`.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// What an answer's `kind` says.
pub const FILE_BYTES_KIND: &str = workforce_agent::spill::FILE_BYTES_KIND;

/// The largest file read.
pub const FILE_MAX: u64 = 20 * 1024 * 1024;

/// The largest chunk one call answers.
pub const CHUNK_MAX: usize = 512 * 1024;

/// How long a downloaded file is kept for the chunks after the first.
const KEPT_FOR: Duration = Duration::from_secs(120);

/// The most bytes of downloaded files kept at once, together.
pub const KEPT_BYTES: usize = 100 * 1024 * 1024;

/// The chunk a call asks for: from `offset`, at most `length` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub offset: u64,
    pub length: usize,
}

/// The range `input` asks for: `offset` (default 0) and `length` (default
/// and at most [`CHUNK_MAX`]).
pub fn range_of(input: &Value) -> Result<Range, String> {
    let offset = match input.get("offset") {
        None | Some(Value::Null) => 0,
        Some(v) => v
            .as_u64()
            .ok_or("offset is a whole number of bytes from the start")?,
    };
    let length = match input.get("length") {
        None | Some(Value::Null) => CHUNK_MAX,
        Some(v) => {
            let n = v
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or("length is a whole number of bytes, more than 0")?;
            (n as usize).min(CHUNK_MAX)
        }
    };
    if offset > FILE_MAX {
        return Err(format!(
            "offset is past the largest file read ({FILE_MAX} bytes)"
        ));
    }
    Ok(Range { offset, length })
}

/// The answer for `range` of `bytes`.
pub fn chunk(bytes: &[u8], media_type: &str, range: Range) -> Value {
    let size = bytes.len() as u64;
    let start = range.offset.min(size) as usize;
    let end = (start + range.length).min(bytes.len());
    json!({
        "kind": FILE_BYTES_KIND,
        "media_type": media_type,
        "size": size,
        "offset": start,
        "length": end - start,
        "data": base64::engine::general_purpose::STANDARD.encode(&bytes[start..end]),
    })
}

/// A credential, as a key a kept file is found by: its hash, never itself.
pub fn fingerprint(credential: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b"workforce-file-read\0");
    h.update(credential);
    let d = h.finalize();
    d.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

struct Kept {
    key: String,
    at: Instant,
    media_type: String,
    bytes: Arc<Vec<u8>>,
}

/// Files downloaded lately, by the credential and the address they were
/// read with: the least lately read go first past the budget.
pub struct Memo {
    kept: Mutex<VecDeque<Kept>>,
    budget: usize,
    /// One gate per file being downloaded, while anyone waits on it.
    fetching: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Default for Memo {
    fn default() -> Self {
        Self::within(KEPT_BYTES)
    }
}

impl Memo {
    /// A memo keeping at most `budget` bytes of files together.
    pub fn within(budget: usize) -> Self {
        Self {
            kept: Mutex::default(),
            budget,
            fetching: Mutex::default(),
        }
    }

    /// The file kept under `key`, while it is fresh; read now, it is the
    /// last to go.
    pub fn get(&self, key: &str) -> Option<(Arc<Vec<u8>>, String)> {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        kept.retain(|k| k.at.elapsed() < KEPT_FOR);
        let i = kept.iter().position(|k| k.key == key)?;
        let k = kept.remove(i)?;
        let found = (Arc::clone(&k.bytes), k.media_type.clone());
        kept.push_back(k);
        Some(found)
    }

    /// Keeps `bytes` under `key`, the least lately read going past the
    /// budget. A file over the whole budget isn't kept.
    pub fn put(&self, key: String, bytes: Arc<Vec<u8>>, media_type: String) {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        kept.retain(|k| k.key != key && k.at.elapsed() < KEPT_FOR);
        if bytes.len() > self.budget {
            return;
        }
        let mut total: usize = kept.iter().map(|k| k.bytes.len()).sum();
        while total + bytes.len() > self.budget {
            let Some(gone) = kept.pop_front() else { break };
            total -= gone.bytes.len();
        }
        kept.push_back(Kept {
            key,
            at: Instant::now(),
            media_type,
            bytes,
        });
    }

    /// The file under `key`: kept, or got by `fetch` and kept. Callers
    /// asking for the same `key` at once wait on one `fetch`; the others'
    /// are never run. A failed `fetch` keeps nothing, and the next caller
    /// tries its own.
    pub async fn fetch_once<E>(
        &self,
        key: &str,
        fetch: impl Future<Output = Result<(Vec<u8>, String), E>>,
    ) -> Result<(Arc<Vec<u8>>, String), E> {
        if let Some(hit) = self.get(key) {
            return Ok(hit);
        }
        let gate = {
            let mut fetching = self.fetching.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(fetching.entry(key.to_string()).or_default())
        };
        let held = gate.lock().await;
        let got = match self.get(key) {
            Some(hit) => Ok(hit),
            None => fetch.await.map(|(bytes, media_type)| {
                let bytes = Arc::new(bytes);
                self.put(key.to_string(), Arc::clone(&bytes), media_type.clone());
                (bytes, media_type)
            }),
        };
        drop(held);
        // The gate goes once no one else holds it: only taken under this
        // lock, so none takes it while it goes.
        let mut fetching = self.fetching.lock().unwrap_or_else(|e| e.into_inner());
        if fetching
            .get(key)
            .is_some_and(|g| Arc::ptr_eq(g, &gate) && Arc::strong_count(g) <= 2)
        {
            fetching.remove(key);
        }
        got
    }
}

/// The process's kept files.
pub fn memo() -> &'static Memo {
    static MEMO: std::sync::OnceLock<Memo> = std::sync::OnceLock::new();
    MEMO.get_or_init(Memo::default)
}

/// `response`'s body, refused once it passes [`FILE_MAX`] (by what it says
/// of its length, or as it arrives).
pub async fn capped_body(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    if response.content_length().is_some_and(|n| n > FILE_MAX) {
        return Err(format!("the file is over {} MB", FILE_MAX / (1024 * 1024)));
    }
    let mut out = Vec::new();
    while let Some(piece) = response
        .chunk()
        .await
        .map_err(|e| format!("the file didn't arrive: {e}"))?
    {
        if out.len() as u64 + piece.len() as u64 > FILE_MAX {
            return Err(format!("the file is over {} MB", FILE_MAX / (1024 * 1024)));
        }
        out.extend_from_slice(&piece);
    }
    Ok(out)
}

/// Whether `url` is a file Linear keeps for its uploads: `https`, on
/// `uploads.linear.app` exactly, with no user, password or port. The one
/// address Linear's key is sent to besides its API.
pub fn is_linear_upload(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    u.scheme() == "https"
        && u.host_str() == Some("uploads.linear.app")
        && u.port().is_none()
        && u.username().is_empty()
        && u.password().is_none()
        && u.path().len() > 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_defaults_and_is_held_to_a_chunk() {
        assert_eq!(
            range_of(&json!({})).unwrap(),
            Range {
                offset: 0,
                length: CHUNK_MAX
            }
        );
        assert_eq!(
            range_of(&json!({"offset": 10, "length": 99_999_999})).unwrap(),
            Range {
                offset: 10,
                length: CHUNK_MAX
            }
        );
        assert!(range_of(&json!({"offset": -1})).is_err());
        assert!(range_of(&json!({"length": 0})).is_err());
        assert!(range_of(&json!({"offset": FILE_MAX + 1})).is_err());
    }

    #[test]
    fn a_chunk_says_where_it_is_and_never_runs_past_the_end() {
        let bytes: Vec<u8> = (0..10).collect();
        let c = chunk(
            &bytes,
            "image/png",
            Range {
                offset: 8,
                length: 5,
            },
        );
        assert_eq!(c["kind"], FILE_BYTES_KIND);
        assert_eq!(c["size"], 10);
        assert_eq!(c["offset"], 8);
        assert_eq!(c["length"], 2);
        let data = base64::engine::general_purpose::STANDARD
            .decode(c["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(data, vec![8, 9]);
        let past = chunk(
            &bytes,
            "image/png",
            Range {
                offset: 50,
                length: 5,
            },
        );
        assert_eq!(past["length"], 0);
        assert_eq!(past["offset"], 10);
    }

    #[test]
    fn a_kept_file_is_found_only_under_its_own_credential() {
        let memo = Memo::default();
        let a = format!("{}:file", fingerprint(b"key-a"));
        let b = format!("{}:file", fingerprint(b"key-b"));
        memo.put(a.clone(), Arc::new(vec![1, 2, 3]), "image/png".into());
        assert!(memo.get(&a).is_some());
        assert!(memo.get(&b).is_none());
        assert!(!fingerprint(b"key-a").contains("key"));
    }

    #[test]
    fn files_are_kept_within_a_byte_budget_the_least_lately_read_going() {
        let memo = Memo::within(1000);
        // Many small files all stay: the budget is bytes, not files.
        for i in 0..10 {
            memo.put(format!("k{i}"), Arc::new(vec![0; 50]), "x".into());
        }
        assert!((0..10).all(|i| memo.get(&format!("k{i}")).is_some()));
        // Read again, k0 is the last of them to go.
        memo.get("k0");
        memo.put("big".into(), Arc::new(vec![0; 900]), "x".into());
        assert!(memo.get("big").is_some());
        assert!(memo.get("k0").is_some(), "lately read: kept");
        assert!(memo.get("k1").is_none(), "least lately read: gone");
        let kept: usize = (0..10)
            .filter_map(|i| memo.get(&format!("k{i}")))
            .map(|(b, _)| b.len())
            .sum();
        assert!(kept + 900 <= 1000, "{kept}");
        // A file over the whole budget isn't kept, and drops nothing.
        memo.put("huge".into(), Arc::new(vec![0; 1001]), "x".into());
        assert!(memo.get("huge").is_none());
        assert!(memo.get("big").is_some());
    }

    #[tokio::test]
    async fn callers_missing_one_file_at_once_wait_for_one_download() {
        let memo = Arc::new(Memo::within(1000));
        let downloads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reads: Vec<_> = (0..8)
            .map(|_| {
                let memo = Arc::clone(&memo);
                let downloads = Arc::clone(&downloads);
                tokio::spawn(async move {
                    memo.fetch_once("k", async {
                        downloads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok::<_, String>((vec![7; 10], "image/png".to_string()))
                    })
                    .await
                    .unwrap()
                })
            })
            .collect();
        for r in reads {
            assert_eq!(*r.await.unwrap().0, vec![7; 10]);
        }
        assert_eq!(downloads.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(memo.fetching.lock().unwrap().is_empty(), "no gate left");
        // A failed download keeps nothing: the next caller tries its own.
        let failed = memo
            .fetch_once("other", async { Err::<(Vec<u8>, String), _>("down") })
            .await;
        assert!(failed.is_err());
        let again = memo
            .fetch_once("other", async { Ok::<_, &str>((vec![1], "x".to_string())) })
            .await
            .unwrap();
        assert_eq!(*again.0, vec![1]);
    }

    #[test]
    fn only_linear_uploads_get_the_key() {
        assert!(is_linear_upload(
            "https://uploads.linear.app/abc/def/shot.png"
        ));
        for no in [
            "http://uploads.linear.app/abc/shot.png",
            "https://uploads.linear.app.evil.example/abc.png",
            "https://evil.example/uploads.linear.app/abc.png",
            "https://user:pw@uploads.linear.app/abc.png",
            "https://uploads.linear.app:8443/abc.png",
            "https://uploads.linear.app/",
            "https://linear.app/abc.png",
            "not a url",
        ] {
            assert!(!is_linear_upload(no), "{no}");
        }
    }
}
