//! Append-only redaction of a streamed answer.
//!
//! A streamed answer reaches a human in pieces, and anything already shown must
//! stay shown — a delivery surface that can only append cannot un-send a chunk
//! it emitted too early. [`StreamRedactor`] earns that by emitting only the
//! portion of the accumulated text whose redaction is *stable*: redacting a
//! longer prefix must never change how an already-emitted shorter one rendered.
//!
//! Two things threaten that invariant, and both are held back rather than
//! emitted: a credential straddling a delta boundary (a trailing byte window),
//! and an unterminated secret marker, whose completed form collapses to a short
//! preview and so would rewrite text already sent.

/// Bytes of streamed text held back before emission, so a catalogued secret
/// straddling two deltas is redacted once complete rather than sent raw.
const HOLDBACK_BYTES: usize = 64;

/// Opening delimiter of a secret marker (`[[secret:<id> …]]`). The completed
/// marker renders as a short preview, so emitting a partial one would break the
/// append-only invariant when the rest arrives. [`StreamRedactor::new`] uses
/// it; a redactor over another marker family passes its own opener to
/// [`StreamRedactor::with_marker_open`].
pub const MARKER_OPEN: &str = "[[secret:";
const MARKER_CLOSE: &str = "]]";

/// Longest a real marker can run. Past this, an unclosed `[[secret:` is prose
/// rather than a marker in flight, and holding text back for it would stall the
/// stream for the rest of the turn.
const MARKER_MAX_BYTES: usize = 256;

/// Offsets captured at a round boundary so a retry can rewind both halves of
/// the redactor consistently. A `sent` offset is not a valid `raw` offset —
/// redaction changes lengths — so both are recorded together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoundMark {
    raw: usize,
    sent: usize,
}

impl RoundMark {
    /// Whether this marks the very start of the stream — nothing had been
    /// written when it was taken, so rewinding to it leaves nothing behind.
    pub fn is_start(&self) -> bool {
        self.raw == 0
    }
}

/// Redacts a streamed answer across delta boundaries, emitting only text that
/// will never need to change again.
pub struct StreamRedactor<F> {
    raw: String,
    sent: String,
    /// How far into `raw` has already been emitted. `finish` emits everything,
    /// so a later `push` would otherwise recompute a *shorter* stable prefix and
    /// retract text already sent.
    emitted: usize,
    redact: F,
    /// The opener whose in-flight or bisected occurrences hold the cut back.
    marker_open: &'static str,
}

impl<F: Fn(&str) -> String> StreamRedactor<F> {
    pub fn new(redact: F) -> Self {
        Self::with_marker_open(redact, MARKER_OPEN)
    }

    /// A redactor whose rewrite is keyed on a different `[[…]]` opener — the
    /// same append-only guarantee for any marker that `redact` rewrites.
    pub fn with_marker_open(redact: F, marker_open: &'static str) -> Self {
        Self {
            raw: String::new(),
            sent: String::new(),
            emitted: 0,
            redact,
            marker_open,
        }
    }

    /// Append a delta; return the newly-safe, redacted suffix to emit, if any.
    pub fn push(&mut self, delta: &str) -> Option<String> {
        self.raw.push_str(delta);
        self.emit(stable_end(&self.raw, self.marker_open))
    }

    /// Flush the held-back tail once the stream is complete. `push` always keeps
    /// a trailing window back, so without this the end of every answer is never
    /// emitted; call it once when the run ends.
    pub fn finish(&mut self) -> Option<String> {
        self.emit(self.raw.len())
    }

    /// Redact `raw[..end]` and return whatever it adds beyond what was already
    /// emitted.
    ///
    /// A longer prefix that no longer starts with the previous emission means
    /// [`stable_end`] let through something whose redaction was not final — a
    /// defect rather than a normal outcome. Re-seed from the longer redaction so
    /// the stream keeps moving instead of silently stalling for the rest of the
    /// turn.
    fn emit(&mut self, end: usize) -> Option<String> {
        let end = end.max(self.emitted);
        self.emitted = end;
        let redacted = (self.redact)(&self.raw[..end]);
        let suffix = match redacted.strip_prefix(self.sent.as_str()) {
            Some(suffix) => suffix.to_string(),
            None => {
                tracing::warn!(
                    target: "stream_redact",
                    "redaction of a longer prefix diverged from what was already emitted"
                );
                String::new()
            }
        };
        self.sent = redacted;
        Some(suffix).filter(|s| !s.is_empty())
    }

    /// Everything emitted so far — exactly what the human has seen. Call after
    /// [`Self::finish`] so the held-back tail is included.
    pub fn sent(&self) -> &str {
        &self.sent
    }

    pub fn into_text(self) -> String {
        self.sent
    }

    pub fn is_empty(&self) -> bool {
        self.sent.is_empty()
    }

    /// Mark the current position so a round that turns out to be garbage can be
    /// rewound without disturbing what came before it.
    pub fn mark(&self) -> RoundMark {
        RoundMark {
            raw: self.raw.len(),
            sent: self.sent.len(),
        }
    }

    /// Drop everything appended since `mark`. Both buffers rewind together:
    /// truncating `sent` alone would leave the discarded text in `raw`, and the
    /// next redaction would re-introduce it.
    pub fn rewind(&mut self, mark: RoundMark) {
        self.raw.truncate(mark.raw);
        self.sent.truncate(mark.sent);
        self.emitted = self.emitted.min(mark.raw);
    }
}

/// The longest prefix of `raw` whose redaction is final: before the trailing
/// holdback window, and never cutting through a secret marker. Always a char
/// boundary.
///
/// A marker must not be split by the cut in *either* direction — a terminated
/// one collapses to a short preview, so emitting part of it and completing it
/// later rewrites text already sent, exactly as an unterminated one does.
fn stable_end(raw: &str, marker_open: &str) -> usize {
    let holdback = char_boundary(raw, raw.len().saturating_sub(HOLDBACK_BYTES));
    // A cut landing part-way through the opener itself would emit `[[secr` as
    // prose and recognise the marker only once the rest arrived.
    let holdback = (1..marker_open.len())
        .rev()
        .find(|&k| holdback >= k && raw[..holdback].ends_with(&marker_open[..k]))
        .map_or(holdback, |k| holdback - k);
    let end = match raw[..holdback].rfind(marker_open) {
        Some(open)
            if raw[open..]
                .find(MARKER_CLOSE)
                .map(|i| open + i + MARKER_CLOSE.len())
                .is_none_or(|close| close > holdback)
                // A `[[secret:` in prose that never closes would otherwise
                // freeze the preview for the rest of the turn.
                && raw.len() - open <= MARKER_MAX_BYTES =>
        {
            open
        }
        _ => holdback,
    };
    char_boundary(raw, token_start(raw, end))
}

/// Longest run of identifier characters the cut backs off for. A credential
/// or a placeholder id is far shorter; past this an unbroken run is a blob,
/// and holding it back would stall the stream.
const TOKEN_MAX_BYTES: usize = 128;

/// If `end` falls inside a run of identifier characters, the start of that
/// run; otherwise `end`. A redaction that only sees the head of a token (the
/// first characters of a key, or of a bare placeholder id) renders it as
/// prose, and the completed token then rewrites what was already emitted.
fn token_start(raw: &str, end: usize) -> usize {
    let is_token = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if end == 0 || end >= raw.len() || !raw[end..].starts_with(is_token) {
        return end;
    }
    let head = &raw[..end];
    let start = head.trim_end_matches(is_token).len();
    if end - start <= TOKEN_MAX_BYTES {
        start
    } else {
        end
    }
}

/// The largest char boundary at or below `at`.
fn char_boundary(s: &str, at: usize) -> usize {
    (0..=at.min(s.len()))
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> StreamRedactor<fn(&str) -> String> {
        StreamRedactor::new(|s: &str| s.to_string())
    }

    /// Stands in for a shortening redaction: the marker collapses to its
    /// preview only once terminated.
    fn marker_redact(s: &str) -> String {
        crate::secret_marker::render_markers_as_preview(s)
    }

    #[test]
    fn holds_back_the_trailing_window() {
        let mut r = plain();
        assert_eq!(r.push("short"), None);
        assert!(r.sent().is_empty());
    }

    #[test]
    fn emits_once_past_the_holdback_then_flushes_the_tail() {
        let mut r = plain();
        let long = "a ".repeat(HOLDBACK_BYTES / 2 + 5);
        assert_eq!(r.push(&long).as_deref(), Some("a ".repeat(5).as_str()));
        assert_eq!(
            r.finish().as_deref(),
            Some("a ".repeat(HOLDBACK_BYTES / 2).as_str())
        );
        assert_eq!(r.sent(), long);
    }

    #[test]
    fn finish_is_idempotent() {
        let mut r = plain();
        r.push("hello world").unwrap_or_default();
        assert_eq!(r.finish().as_deref(), Some("hello world"));
        assert_eq!(r.finish(), None);
    }

    /// A full marker runs past the byte holdback, so only the structural term in
    /// [`stable_end`] keeps it from being emitted half-written.
    const MARKER: &str = "[[secret:deadbeefdeadbeefdeadbeefdeadbeef type=GITHUB_PAT \
                          preview=ghp_...AbCd]]";

    #[test]
    fn every_emission_extends_the_last() {
        let mut r = StreamRedactor::new(marker_redact as fn(&str) -> String);
        let (open, close) = MARKER.split_at(30);
        let mut seen = String::new();
        for chunk in [
            "here is a token ",
            open,
            close,
            " and more prose after it",
            &"x".repeat(HOLDBACK_BYTES),
        ] {
            if let Some(suffix) = r.push(chunk) {
                seen.push_str(&suffix);
            }
            assert_eq!(seen, r.sent(), "emitted text must equal what was sent");
        }
        if let Some(suffix) = r.finish() {
            seen.push_str(&suffix);
        }
        assert_eq!(seen, r.sent());
        assert!(
            !seen.contains("[[secret:"),
            "marker must not survive: {seen}"
        );
    }

    /// The invariant the whole design rests on: redacting a longer stable prefix
    /// never rewrites what a shorter one already emitted.
    #[test]
    fn stable_prefixes_are_prefix_ordered() {
        let text = &format!(
            "prose {MARKER} tail [Tool Result: {{\"a\": 1}}] more prose to push past the window"
        );
        let ends: Vec<usize> = (0..=text.len())
            .filter(|&i| text.is_char_boundary(i))
            .map(|i| stable_end(&text[..i], MARKER_OPEN))
            .collect();
        for pair in ends.windows(2) {
            let (a, b) = (
                marker_redact(&text[..pair[0]]),
                marker_redact(&text[..pair[1]]),
            );
            assert!(
                b.starts_with(&a),
                "redaction of a longer prefix must extend the shorter one:\n{a:?}\n{b:?}"
            );
        }
    }

    #[test]
    fn stable_end_never_cuts_through_a_marker() {
        let prose = "a".repeat(200);
        let in_flight = format!("{prose}{MARKER_OPEN}{}", "x".repeat(120));
        assert_eq!(
            stable_end(&in_flight, MARKER_OPEN),
            prose.len(),
            "a marker still being written holds the cut at its start"
        );

        let straddling = format!("{prose}{MARKER}{}", "c".repeat(HOLDBACK_BYTES / 2));
        assert_eq!(
            stable_end(&straddling, MARKER_OPEN),
            prose.len(),
            "a complete marker the holdback would bisect also holds the cut at its start"
        );

        let clear = format!("{prose}{MARKER}{}", "c ".repeat(HOLDBACK_BYTES));
        assert!(
            stable_end(&clear, MARKER_OPEN) > prose.len() + MARKER.len(),
            "once the marker is well clear of the window the cut moves past it"
        );
    }

    #[test]
    fn stable_end_releases_a_stray_opener_that_never_closes() {
        let text = format!("prose {MARKER_OPEN}{}", "x".repeat(MARKER_MAX_BYTES * 2));
        assert!(
            stable_end(&text, MARKER_OPEN) > 6,
            "a `[[secret:` in prose must not freeze the stream for the whole turn"
        );
    }

    #[test]
    fn stable_end_never_retreats_as_text_arrives() {
        let text = &format!("prose {MARKER} tail {MARKER_OPEN}unclosed and then some more prose");
        let ends: Vec<usize> = (0..=text.len())
            .filter(|&i| text.is_char_boundary(i))
            .map(|i| stable_end(&text[..i], MARKER_OPEN))
            .collect();
        for pair in ends.windows(2) {
            assert!(pair[0] <= pair[1], "stable_end retreated: {pair:?}");
        }
    }

    #[test]
    fn stable_end_never_bisects_a_token() {
        let prose = "a ".repeat(50);
        // 53 trailing bytes put the byte holdback five characters into the
        // hex run; the cut backs off to the space before it.
        let text = format!("{prose}0123456789abcdef {}", "b ".repeat(26));
        let end = stable_end(&text, MARKER_OPEN);
        assert_eq!(end, prose.len(), "{:?}", &text[..end]);
        assert!(text[end..].starts_with("0123456789abcdef"));

        let blob = format!("{prose}{}", "x".repeat(TOKEN_MAX_BYTES * 3));
        assert!(
            stable_end(&blob, MARKER_OPEN) > prose.len(),
            "an unbroken blob longer than a token is not held back"
        );
    }

    #[test]
    fn stable_end_lands_on_a_char_boundary() {
        let text = format!("{}é", "€".repeat(40));
        let end = stable_end(&text, MARKER_OPEN);
        assert!(text.is_char_boundary(end));
    }

    #[test]
    fn rewind_restores_both_buffers() {
        let mut r = plain();
        r.push(&"a".repeat(HOLDBACK_BYTES + 10));
        r.finish();
        let mark = r.mark();
        let kept = r.sent().to_string();
        r.push(&"b".repeat(HOLDBACK_BYTES + 10));
        r.finish();
        assert_ne!(r.sent(), kept);
        r.rewind(mark);
        assert_eq!(r.sent(), kept);
        assert_eq!(r.finish(), None, "rewound text must not resurface");
    }

    #[test]
    fn rewind_to_the_start_empties_the_stream() {
        let mut r = plain();
        let mark = r.mark();
        r.push(&"a".repeat(HOLDBACK_BYTES + 10));
        r.finish();
        r.rewind(mark);
        assert!(r.is_empty());
    }
}
