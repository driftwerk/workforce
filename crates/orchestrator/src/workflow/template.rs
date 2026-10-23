//! The `{{…}}` scanner, shared by runtime resolution, secret-reference lookup
//! and save-time validation so the template syntax has exactly one definition.

/// A parsed piece of a template string.
#[derive(Debug, PartialEq, Eq)]
pub enum Segment<'a> {
    Literal(&'a str),
    /// `{{key}}`, or `{{?key}}` when `optional` — an absent optional renders empty.
    Ref {
        optional: bool,
        key: &'a str,
    },
}

/// Split `template` into literals and references. An unterminated `{{` is
/// literal text, so prose containing braces resolves rather than erroring.
pub fn segments(template: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let literal = &rest[..start];
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            out.push(Segment::Literal(rest));
            return out;
        };
        if !literal.is_empty() {
            out.push(Segment::Literal(literal));
        }
        let raw = after_open[..end].trim();
        let (optional, key) = match raw.strip_prefix('?') {
            Some(rest) => (true, rest.trim()),
            None => (false, raw),
        };
        out.push(Segment::Ref { optional, key });
        rest = &after_open[end + 2..];
    }
    if !rest.is_empty() {
        out.push(Segment::Literal(rest));
    }
    out
}

/// Every reference in `template`, as `(optional, key)`.
pub fn references(template: &str) -> impl Iterator<Item = (bool, &str)> {
    segments(template)
        .into_iter()
        .filter_map(|segment| match segment {
            Segment::Ref { optional, key } => Some((optional, key)),
            Segment::Literal(_) => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_splits_literals_and_refs() {
        assert_eq!(
            segments("a {{x}} b"),
            vec![
                Segment::Literal("a "),
                Segment::Ref {
                    optional: false,
                    key: "x"
                },
                Segment::Literal(" b"),
            ]
        );
    }

    #[test]
    fn segments_marks_optional_and_trims() {
        assert_eq!(
            segments("{{? steps.a.b }}"),
            vec![Segment::Ref {
                optional: true,
                key: "steps.a.b"
            }]
        );
    }

    #[test]
    fn segments_treats_unterminated_open_as_literal() {
        assert_eq!(
            segments("keep {{ this"),
            vec![Segment::Literal("keep {{ this")]
        );
    }

    #[test]
    fn segments_handles_no_refs() {
        assert_eq!(segments("plain"), vec![Segment::Literal("plain")]);
    }

    #[test]
    fn references_yields_only_refs() {
        let refs: Vec<_> = references("{{a}} text {{?b}}").collect();
        assert_eq!(refs, vec![(false, "a"), (true, "b")]);
    }
}
