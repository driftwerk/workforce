//! RFC 6266 `Content-Disposition: attachment` value construction.
//!
//! Returns a plain `String` so the lower layer stays free of the `http`
//! crate; the hub wraps it in a `HeaderValue` and the S3 store passes it to
//! `response_content_disposition`. One source of truth for the filename
//! escaping shared by every byte-serving path.

/// Build a `Content-Disposition: attachment` value from an arbitrary
/// filename. ASCII-only names use the plain `filename=` form; anything else
/// gets the percent-encoded RFC 6266 `filename*=UTF-8''…` form alongside a
/// sanitised ASCII fallback for legacy clients. `"`/`\` are escaped (or
/// stripped from the fallback) so a model/user-supplied filename can't
/// truncate the header value.
pub fn content_disposition_attachment(filename: &str) -> String {
    let ascii_only = filename.is_ascii() && !filename.contains(['"', '\\']);
    if ascii_only {
        format!("attachment; filename=\"{filename}\"")
    } else {
        let ascii_fallback: String = filename
            .chars()
            .map(|c| {
                if c.is_ascii_graphic() && c != '"' && c != '\\' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let encoded = percent_encode_filename(filename);
        format!("attachment; filename=\"{ascii_fallback}\"; filename*=UTF-8''{encoded}")
    }
}

/// Percent-encode every byte that isn't in RFC 5987's `attr-char` set.
/// Implemented inline so we don't pull in `percent-encoding` just for one
/// call site.
fn percent_encode_filename(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'!'
            | b'#'
            | b'$'
            | b'&'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~' => out.push(*byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_plain_form_for_ascii() {
        assert_eq!(
            content_disposition_attachment("notes.pdf"),
            "attachment; filename=\"notes.pdf\""
        );
    }

    #[test]
    fn falls_back_for_quotes_or_backslash() {
        let raw = content_disposition_attachment("weird\"name\\.pdf");
        assert!(raw.starts_with("attachment; filename=\""));
        assert!(raw.contains("filename*=UTF-8''"));
        // The ASCII fallback strips quotes and backslashes; only the two
        // boundary quotes around `filename="…"` survive.
        assert_eq!(raw.matches('"').count(), 2, "stray quote in: {raw}");
        assert!(!raw.contains('\\'), "stray backslash in: {raw}");
    }

    #[test]
    fn encodes_unicode_filenames() {
        let raw = content_disposition_attachment("café.pdf");
        assert!(
            raw.contains("filename=\"caf_.pdf\""),
            "ASCII fallback: {raw}"
        );
        assert!(
            raw.contains("filename*=UTF-8''caf%C3%A9.pdf"),
            "RFC 6266 form: {raw}",
        );
    }

    #[test]
    fn percent_encode_keeps_attr_chars_and_escapes_others() {
        assert_eq!(percent_encode_filename("a-b.txt"), "a-b.txt");
        assert_eq!(percent_encode_filename("space here"), "space%20here");
        assert_eq!(percent_encode_filename("é"), "%C3%A9");
    }
}
