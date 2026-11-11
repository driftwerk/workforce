//! A file read a chunk at a time, for Desk's board sync: a ticket's Jira
//! attachment or Linear upload, fetched as the person whose board it is,
//! and answered in pieces small enough for an MCP `tools/call` result (which
//! a client caps at 1 MB).
//!
//! The file is downloaded once, up to [`FILE_MAX`], and kept for a short
//! while under the credential it was read with ([`Memo`]): the next chunk is
//! served from there, and a caller with another credential never sees it.
//!
//! An answer is `{kind: "file_bytes", media_type, size, offset, length,
//! data}`, `data` the chunk's bytes in base64. Its `kind` keeps the spill
//! from staging it (`workforce_agent::spill`): it is bytes for a program,
//! not words for a model.

use std::collections::VecDeque;
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

/// How many downloaded files are kept at once.
const KEPT: usize = 3;

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
/// read with.
#[derive(Default)]
pub struct Memo(Mutex<VecDeque<Kept>>);

impl Memo {
    /// The file kept under `key`, while it is fresh.
    pub fn get(&self, key: &str) -> Option<(Arc<Vec<u8>>, String)> {
        let mut kept = self.0.lock().unwrap_or_else(|e| e.into_inner());
        kept.retain(|k| k.at.elapsed() < KEPT_FOR);
        kept.iter()
            .find(|k| k.key == key)
            .map(|k| (Arc::clone(&k.bytes), k.media_type.clone()))
    }

    /// Keeps `bytes` under `key`, the oldest going past [`KEPT`].
    pub fn put(&self, key: String, bytes: Arc<Vec<u8>>, media_type: String) {
        let mut kept = self.0.lock().unwrap_or_else(|e| e.into_inner());
        kept.retain(|k| k.key != key);
        kept.push_back(Kept {
            key,
            at: Instant::now(),
            media_type,
            bytes,
        });
        while kept.len() > KEPT {
            kept.pop_front();
        }
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
    fn only_so_many_files_are_kept() {
        let memo = Memo::default();
        for i in 0..(KEPT + 2) {
            memo.put(format!("k{i}"), Arc::new(vec![]), "x".into());
        }
        assert!(memo.get("k0").is_none());
        assert!(memo.get(&format!("k{}", KEPT + 1)).is_some());
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
