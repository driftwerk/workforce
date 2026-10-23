//! Named byte streams attached to a document.
//!
//! A body stays the indexed, queryable, size-capped part of a document; a
//! payload too large to be one — a log export, a captured page, an evidence
//! pack — is content, written in pieces and read in ranges, so nothing in the
//! system ever needs the whole of it in memory.
//!
//! Writes go to a pending version and become visible only at
//! [`DocumentStore::content_commit`]. A reader therefore never sees a torn
//! stream, and a writer that dies mid-upload leaves a pending version that the
//! next writer replaces rather than a corrupted one.

use serde::{Deserialize, Serialize};

use super::StoreError;

/// Largest single write. Bounds what a caller can make the hub hold in memory
/// at once; a bigger payload is written in several calls.
pub const MAX_WRITE_BYTES: usize = 8 * 1024 * 1024;

/// Largest a whole stream may become. Past this, the payload belongs in object
/// storage with a reference to it in the body.
pub const MAX_STREAM_BYTES: i64 = 1024 * 1024 * 1024;

/// Largest range one read may return, for the same reason as `MAX_WRITE_BYTES`.
pub const MAX_READ_BYTES: i64 = 8 * 1024 * 1024;

/// What a body holds where it refers to content, so a reader of the document
/// can resolve the reference without knowing the convention by heart.
pub const CONTENT_REF: &str = "$content";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentInfo {
    pub name: String,
    pub size_bytes: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// BLAKE3 of the committed bytes, computed as they are read back at
    /// commit, so integrity is checkable rather than assumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    pub version: i64,
}

/// A slice of a stream, and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct ContentSlice {
    pub offset: i64,
    pub bytes: Vec<u8>,
    /// The whole stream's length, so a reader knows when it has reached the
    /// end without a second call.
    pub size_bytes: i64,
}

/// Where the next write to a pending stream must start.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ContentWrite {
    pub version: i64,
    pub next_offset: i64,
}

pub fn check_write(offset: i64, bytes: &[u8], pending_len: i64) -> Result<(), StoreError> {
    if bytes.is_empty() {
        return Err(StoreError::Invalid("a content write needs bytes".into()));
    }
    if bytes.len() > MAX_WRITE_BYTES {
        return Err(StoreError::Invalid(format!(
            "a single content write is at most {MAX_WRITE_BYTES} bytes; write a long stream in pieces"
        )));
    }
    if offset != pending_len {
        return Err(StoreError::Invalid(format!(
            "content is written in order: the next write starts at {pending_len}, not {offset}"
        )));
    }
    if pending_len + bytes.len() as i64 > MAX_STREAM_BYTES {
        return Err(StoreError::Invalid(format!(
            "a content stream is at most {MAX_STREAM_BYTES} bytes"
        )));
    }
    Ok(())
}

pub fn check_read(offset: i64, len: i64) -> Result<(), StoreError> {
    if offset < 0 || len <= 0 {
        return Err(StoreError::Invalid(
            "a content read needs a non-negative offset and a positive length".into(),
        ));
    }
    if len > MAX_READ_BYTES {
        return Err(StoreError::Invalid(format!(
            "a content read returns at most {MAX_READ_BYTES} bytes; read a long stream in ranges"
        )));
    }
    Ok(())
}

pub fn digest_of(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_is_written_in_order_from_where_the_last_write_ended() {
        assert!(check_write(0, b"abc", 0).is_ok());
        assert!(check_write(3, b"def", 3).is_ok());
        assert!(
            check_write(0, b"def", 3).is_err(),
            "rewriting an earlier offset would mean rewriting a chunk"
        );
        assert!(
            check_write(9, b"def", 3).is_err(),
            "a gap would leave a stream with a hole in it"
        );
    }

    #[test]
    fn writes_and_reads_are_bounded() {
        assert!(check_write(0, b"", 0).is_err());
        assert!(check_write(0, &vec![0u8; MAX_WRITE_BYTES + 1], 0).is_err());
        assert!(check_write(MAX_STREAM_BYTES, b"x", MAX_STREAM_BYTES).is_err());
        assert!(check_read(0, 0).is_err());
        assert!(check_read(-1, 10).is_err());
        assert!(check_read(0, MAX_READ_BYTES + 1).is_err());
        assert!(check_read(0, MAX_READ_BYTES).is_ok());
    }
}
