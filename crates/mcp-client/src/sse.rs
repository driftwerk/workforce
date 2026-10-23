//! Minimal Server-Sent-Events parser for the SSE-on-POST response
//! shape used by Streamable HTTP MCP. Each frame is a sequence of
//! `field: value\n` lines terminated by a blank line; we only
//! consume `data:` lines (concatenating multi-line `data:` per the
//! SSE spec) and skip everything else.
//!
//! Why not pull in `eventsource-stream`? Two reasons:
//! 1. Single-purpose use — we just want to find the JSON-RPC reply
//!    that matches our request id, not arbitrary event streams.
//! 2. Keeping the dep surface minimal — every transitive dep is
//!    another supply-chain liability for a security-sensitive
//!    crate.

/// Parser state for an SSE byte stream. Feed it bytes via
/// [`SseParser::feed`]; it returns every complete frame's
/// reassembled `data:` payload that arrives in this chunk (along
/// with any frames whose terminator landed inside this chunk).
#[derive(Default)]
pub(crate) struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `chunk` to the internal buffer and return the
    /// reassembled payload of every complete frame now in buffer.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(end) = find_frame_terminator(&self.buf) {
            let frame_bytes: Vec<u8> = self.buf.drain(..end.frame_end).collect();
            self.buf.drain(..end.term_len);
            if let Ok(frame) = std::str::from_utf8(&frame_bytes) {
                if let Some(payload) = extract_data_payload(frame) {
                    out.push(payload);
                }
            }
        }
        out
    }
}

struct FrameEnd {
    /// Number of bytes up to (not including) the terminator.
    frame_end: usize,
    /// Length of the terminator itself (`\n\n` = 2, `\r\n\r\n` = 4).
    term_len: usize,
}

/// Find the first `\n\n` or `\r\n\r\n` sequence. SSE technically
/// allows lone-`\r` line endings too but no real implementation
/// emits them.
fn find_frame_terminator(buf: &[u8]) -> Option<FrameEnd> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(FrameEnd {
                frame_end: i,
                term_len: 2,
            });
        }
        if i + 3 < buf.len()
            && buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some(FrameEnd {
                frame_end: i,
                term_len: 4,
            });
        }
        i += 1;
    }
    None
}

/// Concatenate every `data:` line in a frame, joined with `\n` per
/// the SSE spec. Lines other than `data:` (`event:`, `id:`,
/// `retry:`, comments starting with `:`) are ignored.
fn extract_data_payload(frame: &str) -> Option<String> {
    let mut out: Option<String> = None;
    for line in frame.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        match out.as_mut() {
            Some(s) => {
                s.push('\n');
                s.push_str(rest);
            }
            None => out = Some(rest.to_string()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_frame_single_data_line() {
        let mut p = SseParser::new();
        let frames = p.feed(b"event: message\ndata: {\"id\":1}\n\n");
        assert_eq!(frames, vec![r#"{"id":1}"#]);
    }

    #[test]
    fn handles_multi_data_lines_joined_with_newline() {
        let mut p = SseParser::new();
        let frames = p.feed(b"data: {\"a\":\ndata: 1}\n\n");
        assert_eq!(frames, vec!["{\"a\":\n1}"]);
    }

    #[test]
    fn handles_crlf_terminator() {
        let mut p = SseParser::new();
        let frames = p.feed(b"event: message\r\ndata: hello\r\n\r\n");
        assert_eq!(frames, vec!["hello"]);
    }

    #[test]
    fn split_across_chunks_reassembles() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: par").is_empty());
        assert!(p.feed(b"tial").is_empty());
        let frames = p.feed(b"\n\n");
        assert_eq!(frames, vec!["partial"]);
    }

    #[test]
    fn multiple_frames_in_one_feed() {
        let mut p = SseParser::new();
        let frames = p.feed(b"data: one\n\ndata: two\n\n");
        assert_eq!(frames, vec!["one", "two"]);
    }

    #[test]
    fn comments_and_unknown_fields_skipped() {
        let mut p = SseParser::new();
        let frames = p.feed(b": keepalive\nevent: message\nid: 5\ndata: payload\n\n");
        assert_eq!(frames, vec!["payload"]);
    }

    #[test]
    fn frame_with_no_data_lines_yields_nothing() {
        let mut p = SseParser::new();
        let frames = p.feed(b": keepalive\nevent: open\n\n");
        assert!(frames.is_empty());
    }
}
