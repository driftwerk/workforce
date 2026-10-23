//! Reassemble a chunked byte stream into whole lines.
//!
//! The runner forwards output as it arrives, so a chunk boundary falls wherever
//! the pipe happened to split — mid-line, or mid-UTF-8-sequence. Consumers that
//! parse one JSON object per line need the original framing back.

/// Accumulates chunks and yields complete lines, retaining any partial tail
/// until the bytes completing it arrive.
#[derive(Debug, Default)]
pub struct LineReassembler {
    tail: Vec<u8>,
}

impl LineReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `chunk` and return every line it completed.
    ///
    /// Decoding is lossy at the boundary rather than fallible: a split
    /// multi-byte sequence is a framing artefact, and losing the line it
    /// belongs to would be worse than replacing one character.
    ///
    /// The retained tail never holds a newline, so only the new bytes are
    /// scanned: a long line arriving in many chunks costs time linear in its
    /// length.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let scanned = self.tail.len();
        self.tail.extend_from_slice(chunk);
        let Some(last) = self.tail[scanned..]
            .iter()
            .rposition(|b| *b == b'\n')
            .map(|i| scanned + i)
        else {
            return Vec::new();
        };
        let rest = self.tail.split_off(last + 1);
        let complete = std::mem::replace(&mut self.tail, rest);
        complete[..last]
            .split(|b| *b == b'\n')
            .map(decode)
            .collect()
    }

    /// Take whatever is left when the stream ends.
    ///
    /// A process that exits without a trailing newline still said something,
    /// and for Claude Code the last line is the result of the turn — so the
    /// tail is emitted rather than discarded.
    pub fn finish(&mut self) -> Option<String> {
        (!self.tail.is_empty()).then(|| decode(&std::mem::take(&mut self.tail)))
    }
}

/// Strip a trailing `\r` so CRLF input yields the same line as LF.
fn decode(line: &[u8]) -> String {
    let trimmed = match line.strip_suffix(b"\r") {
        Some(rest) => rest,
        None => line,
    };
    String::from_utf8_lossy(trimmed).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(r: &mut LineReassembler, s: &str) -> Vec<String> {
        r.push(s.as_bytes())
    }

    #[test]
    fn whole_lines_pass_straight_through() {
        let mut r = LineReassembler::new();
        assert_eq!(push(&mut r, "a\nb\n"), vec!["a", "b"]);
        assert_eq!(r.finish(), None);
    }

    #[test]
    fn a_line_split_across_chunks_is_rejoined() {
        let mut r = LineReassembler::new();
        assert!(push(&mut r, "{\"kind\":").is_empty());
        assert!(push(&mut r, "\"result\"").is_empty());
        assert_eq!(push(&mut r, "}\n"), vec![r#"{"kind":"result"}"#]);
    }

    #[test]
    fn one_chunk_can_complete_several_lines() {
        let mut r = LineReassembler::new();
        assert!(push(&mut r, "first").is_empty());
        assert_eq!(
            push(&mut r, "\nsecond\nthird\n"),
            vec!["first", "second", "third"]
        );
    }

    #[test]
    fn a_split_multibyte_sequence_survives_rejoining() {
        let mut r = LineReassembler::new();
        let snowman = "☃".as_bytes();
        assert!(r.push(&snowman[..1]).is_empty());
        assert_eq!(
            r.push(&[&snowman[1..], b"\n".as_slice()].concat()),
            vec!["☃"]
        );
    }

    #[test]
    fn crlf_yields_the_same_line_as_lf() {
        let mut r = LineReassembler::new();
        assert_eq!(push(&mut r, "value\r\n"), vec!["value"]);
    }

    /// The case that matters most: Claude Code's last stream event is the turn
    /// result, and a process may exit without a trailing newline.
    #[test]
    fn a_trailing_partial_line_is_emitted_at_end_of_stream() {
        let mut r = LineReassembler::new();
        assert!(push(&mut r, "no trailing newline").is_empty());
        assert_eq!(r.finish().as_deref(), Some("no trailing newline"));
        assert_eq!(r.finish(), None, "finish must not repeat the tail");
    }

    #[test]
    fn a_long_line_across_many_chunks_is_rejoined_once() {
        let mut r = LineReassembler::new();
        let long = "x".repeat(5 * 1024 * 1024);
        let completed: Vec<String> = long
            .as_bytes()
            .chunks(64 * 1024)
            .flat_map(|chunk| r.push(chunk))
            .collect();
        assert!(completed.is_empty());
        assert_eq!(push(&mut r, "\npartial"), vec![long]);
        assert_eq!(r.finish().as_deref(), Some("partial"));
    }

    #[test]
    fn empty_chunks_and_blank_lines_are_handled() {
        let mut r = LineReassembler::new();
        assert!(push(&mut r, "").is_empty());
        assert_eq!(push(&mut r, "\n\n"), vec!["", ""]);
        assert_eq!(r.finish(), None);
    }
}
