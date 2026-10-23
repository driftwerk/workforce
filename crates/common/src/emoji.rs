//! Emoji normalization across messaging platforms.
//!
//! Each platform has its own emoji format:
//! - **Slack**: uses short names without colons (`thumbsup`, `white_check_mark`)
//! - **Discord**: uses Unicode emoji or `:name:` custom emoji
//! - **Telegram**: uses Unicode emoji or custom emoji IDs
//!
//! This module normalises between formats so agents can use any style.

use std::collections::HashMap;
use std::sync::LazyLock;

/// A mapping entry from Unicode to the canonical Slack name.
struct EmojiEntry {
    unicode: &'static str,
    slack_name: &'static str,
}

/// Curated list of commonly used emoji for agent reactions.
/// Not exhaustive — covers the ones agents typically use.
static EMOJI_TABLE: &[EmojiEntry] = &[
    // Processing indicators
    EmojiEntry {
        unicode: "👀",
        slack_name: "eyes",
    },
    EmojiEntry {
        unicode: "✅",
        slack_name: "white_check_mark",
    },
    EmojiEntry {
        unicode: "❌",
        slack_name: "x",
    },
    EmojiEntry {
        unicode: "⏳",
        slack_name: "hourglass_flowing_sand",
    },
    EmojiEntry {
        unicode: "⌛",
        slack_name: "hourglass",
    },
    // Common reactions
    EmojiEntry {
        unicode: "👍",
        slack_name: "thumbsup",
    },
    EmojiEntry {
        unicode: "👎",
        slack_name: "thumbsdown",
    },
    EmojiEntry {
        unicode: "❤️",
        slack_name: "heart",
    },
    EmojiEntry {
        unicode: "😂",
        slack_name: "joy",
    },
    EmojiEntry {
        unicode: "😢",
        slack_name: "cry",
    },
    EmojiEntry {
        unicode: "😮",
        slack_name: "open_mouth",
    },
    EmojiEntry {
        unicode: "🤔",
        slack_name: "thinking_face",
    },
    EmojiEntry {
        unicode: "💡",
        slack_name: "bulb",
    },
    EmojiEntry {
        unicode: "🔥",
        slack_name: "fire",
    },
    EmojiEntry {
        unicode: "🎉",
        slack_name: "tada",
    },
    EmojiEntry {
        unicode: "🙌",
        slack_name: "raised_hands",
    },
    EmojiEntry {
        unicode: "👏",
        slack_name: "clap",
    },
    EmojiEntry {
        unicode: "💪",
        slack_name: "muscle",
    },
    EmojiEntry {
        unicode: "🚀",
        slack_name: "rocket",
    },
    EmojiEntry {
        unicode: "⭐",
        slack_name: "star",
    },
    EmojiEntry {
        unicode: "💯",
        slack_name: "100",
    },
    EmojiEntry {
        unicode: "✨",
        slack_name: "sparkles",
    },
    EmojiEntry {
        unicode: "🤖",
        slack_name: "robot_face",
    },
    EmojiEntry {
        unicode: "🦉",
        slack_name: "owl",
    },
    EmojiEntry {
        unicode: "📝",
        slack_name: "memo",
    },
    EmojiEntry {
        unicode: "📌",
        slack_name: "pushpin",
    },
    EmojiEntry {
        unicode: "🔍",
        slack_name: "mag",
    },
    EmojiEntry {
        unicode: "⚡",
        slack_name: "zap",
    },
    EmojiEntry {
        unicode: "💀",
        slack_name: "skull",
    },
    EmojiEntry {
        unicode: "😅",
        slack_name: "sweat_smile",
    },
    EmojiEntry {
        unicode: "🫡",
        slack_name: "saluting_face",
    },
    EmojiEntry {
        unicode: "🙏",
        slack_name: "pray",
    },
    EmojiEntry {
        unicode: "😊",
        slack_name: "blush",
    },
    EmojiEntry {
        unicode: "🎯",
        slack_name: "dart",
    },
    EmojiEntry {
        unicode: "🧠",
        slack_name: "brain",
    },
    EmojiEntry {
        unicode: "👋",
        slack_name: "wave",
    },
    EmojiEntry {
        unicode: "⚠️",
        slack_name: "warning",
    },
    EmojiEntry {
        unicode: "😉",
        slack_name: "wink",
    },
    EmojiEntry {
        unicode: "+1",
        slack_name: "thumbsup",
    },
    EmojiEntry {
        unicode: "-1",
        slack_name: "thumbsdown",
    },
];

/// Unicode → Slack name lookup.
static UNICODE_TO_SLACK: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    EMOJI_TABLE
        .iter()
        .map(|e| (e.unicode, e.slack_name))
        .collect()
});

/// Slack name → Unicode lookup.
static SLACK_TO_UNICODE: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    EMOJI_TABLE
        .iter()
        // Skip synthetic entries like "+1"
        .filter(|e| !e.unicode.is_ascii())
        .map(|e| (e.slack_name, e.unicode))
        .collect()
});

/// Normalize an emoji input to a Slack-compatible name (no colons).
///
/// Accepts:
/// - Slack names: `thumbsup`, `eyes`
/// - Shortcodes with colons: `:thumbsup:`, `:eyes:`
/// - Unicode: `👍`, `👀`
///
/// Returns the bare Slack name (e.g. `"thumbsup"`).
pub fn to_slack_name(input: &str) -> String {
    let trimmed = input.trim();

    // If it's a :name: shortcode, strip the colons
    if trimmed.starts_with(':') && trimmed.ends_with(':') && trimmed.len() > 2 {
        return trimmed[1..trimmed.len() - 1].to_string();
    }

    // Try Unicode → Slack lookup
    if let Some(name) = UNICODE_TO_SLACK.get(trimmed) {
        return name.to_string();
    }

    // Already a bare name — return as-is
    trimmed.to_string()
}

/// Convert a Slack name to Unicode emoji (if known).
///
/// Returns `None` if there's no known mapping.
pub fn to_unicode(slack_name: &str) -> Option<&'static str> {
    SLACK_TO_UNICODE.get(slack_name).copied()
}

/// Convert a Slack name to a shortcode with colons (e.g. `:thumbsup:`).
pub fn to_shortcode(slack_name: &str) -> String {
    format!(":{}:", slack_name)
}

/// Replace known Slack `:shortcode:` tokens in free text with their Unicode
/// emoji, leaving unknown shortcodes and stray colons untouched. Shortcode
/// names use the Slack charset `[a-z0-9_+-]`; a `:` with no valid closing `:`
/// (or an unknown name) is emitted verbatim, so `3:00`, ratios like `4:3`, and
/// `:not_an_emoji:` survive unchanged.
pub fn shortcodes_to_unicode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        // `i` is always on a char boundary; a `:` is ASCII so this is sound.
        if bytes[i] == b':' {
            let name_start = i + 1;
            let mut j = name_start;
            while j < text.len()
                && matches!(bytes[j], b'a'..=b'z' | b'0'..=b'9' | b'_' | b'+' | b'-')
            {
                j += 1;
            }
            if j > name_start && j < text.len() && bytes[j] == b':' {
                if let Some(unicode) = to_unicode(&text[name_start..j]) {
                    out.push_str(unicode);
                    i = j + 1;
                    continue;
                }
            }
        }
        let ch = text[i..].chars().next().expect("i on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Returns `true` if the character is an emoji or emoji component.
pub fn is_emoji(c: char) -> bool {
    let cp = c as u32;
    matches!(
        cp,
        0x200D              // Zero Width Joiner
        | 0x20E3            // Combining Enclosing Keycap
        | 0x2300..=0x23FF   // Misc Technical
        | 0x2600..=0x27BF   // Misc Symbols & Dingbats
        | 0x2B00..=0x2BFF   // Misc Symbols & Arrows (⭐ ⬆ …)
        | 0xFE00..=0xFE0F   // Variation Selectors
        | 0x1F000..=0x1F2FF // Mahjong/Dominoes/Cards + Enclosed (incl. flags)
        | 0x1F300..=0x1F5FF // Misc Symbols & Pictographs
        | 0x1F600..=0x1F64F // Emoticons
        | 0x1F680..=0x1F6FF // Transport & Map
        | 0x1F900..=0x1F9FF // Supplemental Symbols & Pictographs
        | 0x1FA00..=0x1FA6F // Chess Symbols
        | 0x1FA70..=0x1FAFF // Symbols & Pictographs Extended-A
        | 0xE0020..=0xE007F // Tags
    )
}

/// Strip all emoji characters from a string, collapsing any resulting
/// runs of horizontal whitespace (spaces/tabs) into a single space.
/// Newlines are preserved.
pub fn strip_emoji(text: &str) -> String {
    let stripped: String = text.chars().filter(|c| !is_emoji(*c)).collect();
    // Collapse runs of spaces/tabs (but not newlines) per line, trim leading/trailing spaces
    let mut result = String::with_capacity(stripped.len());
    for (i, line) in stripped.lines().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        let mut line_buf = String::with_capacity(line.len());
        let mut prev_space = false;
        for c in line.chars() {
            if c == ' ' || c == '\t' {
                if !prev_space && !line_buf.is_empty() {
                    line_buf.push(' ');
                }
                prev_space = true;
            } else {
                line_buf.push(c);
                prev_space = false;
            }
        }
        result.push_str(line_buf.trim_end());
    }
    // Preserve trailing newline if original had one
    if stripped.ends_with('\n') {
        result.push('\n');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_to_slack() {
        assert_eq!(to_slack_name("👍"), "thumbsup");
        assert_eq!(to_slack_name("👀"), "eyes");
        assert_eq!(to_slack_name("✅"), "white_check_mark");
        assert_eq!(to_slack_name("🤔"), "thinking_face");
        assert_eq!(to_slack_name("🔥"), "fire");
    }

    #[test]
    fn shortcode_to_slack() {
        assert_eq!(to_slack_name(":thumbsup:"), "thumbsup");
        assert_eq!(to_slack_name(":eyes:"), "eyes");
        assert_eq!(to_slack_name(":fire:"), "fire");
    }

    #[test]
    fn bare_name_passes_through() {
        assert_eq!(to_slack_name("thumbsup"), "thumbsup");
        assert_eq!(to_slack_name("eyes"), "eyes");
        assert_eq!(to_slack_name("custom_emoji"), "custom_emoji");
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(to_slack_name("  👍  "), "thumbsup");
        assert_eq!(to_slack_name(" :fire: "), "fire");
    }

    #[test]
    fn slack_to_unicode_lookup() {
        assert_eq!(to_unicode("thumbsup"), Some("👍"));
        assert_eq!(to_unicode("eyes"), Some("👀"));
        assert_eq!(to_unicode("fire"), Some("🔥"));
        assert_eq!(to_unicode("nonexistent"), None);
    }

    #[test]
    fn shortcode_formatting() {
        assert_eq!(to_shortcode("thumbsup"), ":thumbsup:");
        assert_eq!(to_shortcode("fire"), ":fire:");
    }

    #[test]
    fn shortcodes_to_unicode_converts_known_and_keeps_the_rest() {
        // Known shortcodes become Unicode; multiple per string.
        assert_eq!(
            shortcodes_to_unicode(":white_check_mark: Resolved · :mag: Under Review"),
            "✅ Resolved · 🔍 Under Review"
        );
        // Unknown names, stray colons, times and ratios are left untouched.
        assert_eq!(
            shortcodes_to_unicode("no :totally_unknown: here"),
            "no :totally_unknown: here"
        );
        assert_eq!(
            shortcodes_to_unicode("meet at 3:00 PM, ratio 4:3, scope 5:6"),
            "meet at 3:00 PM, ratio 4:3, scope 5:6"
        );
        // Non-ASCII text around a shortcode is preserved byte-for-byte.
        assert_eq!(shortcodes_to_unicode("— :rocket: —"), "— 🚀 —");
        // The ASCII +1/-1 aliases are not emitted as emoji from free text.
        assert_eq!(shortcodes_to_unicode(":+1:"), ":+1:");
    }

    #[test]
    fn plus_minus_one_aliases() {
        assert_eq!(to_slack_name("+1"), "thumbsup");
        assert_eq!(to_slack_name("-1"), "thumbsdown");
    }

    #[test]
    fn strip_emoji_basic() {
        assert_eq!(strip_emoji("Hello 🦁 world"), "Hello world");
        assert_eq!(strip_emoji("✅ Done"), "Done");
        assert_eq!(strip_emoji("🎉🎉🎉 Party!"), "Party!");
        assert_eq!(strip_emoji("No emoji here"), "No emoji here");
    }

    #[test]
    fn strip_emoji_preserves_punctuation() {
        assert_eq!(strip_emoji("Hello, world!"), "Hello, world!");
        assert_eq!(strip_emoji("It's 3:00 PM."), "It's 3:00 PM.");
    }

    #[test]
    fn strip_emoji_mixed() {
        assert_eq!(strip_emoji("🔥 Fire! 🚀 Launch!"), "Fire! Launch!");
        assert_eq!(strip_emoji("Step 1️⃣ done"), "Step 1 done");
    }

    #[test]
    fn strip_emoji_covers_symbols_and_arrows() {
        assert_eq!(strip_emoji("Top ⭐ pick"), "Top pick");
        assert_eq!(strip_emoji("Up ⬆ there"), "Up there");
    }
}
