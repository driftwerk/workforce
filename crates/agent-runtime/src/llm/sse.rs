//! Server-sent events: splitting a byte stream into event frames and reading
//! an event's data.

/// Splits an SSE byte stream into event frames. Bytes are buffered until a
/// blank line, so a multi-byte character split across chunks survives.
#[derive(Default)]
pub struct SseFrames {
    buffer: Vec<u8>,
}

impl SseFrames {
    /// The frames `chunk` completes, in order.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend(chunk.iter().filter(|b| **b != b'\r'));
        std::iter::from_fn(|| {
            let end = self.buffer.windows(2).position(|w| w == b"\n\n")?;
            let frame: Vec<u8> = self.buffer.drain(..end + 2).collect();
            Some(String::from_utf8_lossy(&frame).into_owned())
        })
        .collect()
    }
}

/// The data of one event frame: its `data:` lines joined by newlines, or
/// `None` for an event with no data and the `[DONE]` terminator.
pub fn event_data(frame: &str) -> Option<String> {
    let data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|d| d.strip_prefix(' ').unwrap_or(d))
        .collect::<Vec<_>>()
        .join("\n");
    let data = data.trim();
    (!data.is_empty() && data != "[DONE]").then(|| data.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_survive_split_chunks_and_crlf() {
        let body = "event: delta\r\ndata: {\"delta\":\"é\"}\r\n\r\n";
        let bytes = body.as_bytes();
        let split = body.find('é').expect("accent") + 1;
        let mut frames = SseFrames::default();
        let mut parsed = frames.push(&bytes[..split]);
        assert!(parsed.is_empty());
        parsed.extend(frames.push(&bytes[split..]));
        assert_eq!(parsed.len(), 1);
        assert_eq!(event_data(&parsed[0]).as_deref(), Some("{\"delta\":\"é\"}"));
    }

    #[test]
    fn event_data_joins_lines_and_skips_done_and_empty_events() {
        assert_eq!(
            event_data("data: {\"a\":\ndata: 1}\n\n").as_deref(),
            Some("{\"a\":\n1}")
        );
        assert_eq!(event_data("data: [DONE]\n\n"), None);
        assert_eq!(event_data("event: ping\n\n"), None);
        assert_eq!(event_data(": comment\n\n"), None);
    }
}
