//! The report fonts travel inside the binary (`include_bytes!`) and are
//! materialised once to a stable directory passed to `typst --font-path`. Both
//! the document and the chart SVGs resolve their fonts there, so output is
//! identical on every platform with no system fonts installed. Static weights
//! only — Typst does not support variable fonts.
//!
//! `Noto Emoji` (monochrome, OFL) is bundled as a *fallback* font: the text
//! families in `typst_gen::preamble` are chains like `("Lato", "Noto Emoji")`,
//! so any emoji a model puts in a heading renders as a line-glyph instead of a
//! missing-glyph box. Without it the trixie container ships no emoji font and
//! macOS's bitmap Apple Color Emoji is unusable by Typst, so emoji become tofu.

use std::io::Write;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use once_cell::sync::OnceCell;

const FONTS: &[(&str, &[u8])] = &[
    (
        "Lato-Regular.ttf",
        include_bytes!("../../../assets/fonts/Lato-Regular.ttf"),
    ),
    (
        "Lato-Bold.ttf",
        include_bytes!("../../../assets/fonts/Lato-Bold.ttf"),
    ),
    (
        "IBMPlexSerif-Regular.ttf",
        include_bytes!("../../../assets/fonts/IBMPlexSerif-Regular.ttf"),
    ),
    (
        "IBMPlexSerif-Bold.ttf",
        include_bytes!("../../../assets/fonts/IBMPlexSerif-Bold.ttf"),
    ),
    // Monochrome emoji fallback (family "Noto Emoji"). Regular + Bold so emoji
    // in bold section headings match the heading weight instead of being
    // synthetically emboldened.
    (
        "NotoEmoji-Regular.ttf",
        include_bytes!("../../../assets/fonts/NotoEmoji-Regular.ttf"),
    ),
    (
        "NotoEmoji-Bold.ttf",
        include_bytes!("../../../assets/fonts/NotoEmoji-Bold.ttf"),
    ),
];

/// Raw bytes of the bundled Lato-Bold face — the meme/caption renderer parses
/// these with `ttf-parser` to measure glyph advances for word-wrapping and
/// auto-sizing, so it never needs to read the materialised file from disk.
pub(crate) fn lato_bold_bytes() -> &'static [u8] {
    FONTS
        .iter()
        .find(|(name, _)| *name == "Lato-Bold.ttf")
        .map(|(_, bytes)| *bytes)
        .expect("Lato-Bold.ttf is bundled")
}

static FONT_DIR: OnceCell<PathBuf> = OnceCell::new();

/// Materialise the bundled fonts once and return the directory for
/// `typst --font-path`. Idempotent and concurrency-safe: each font is written
/// via a temp file + atomic rename, and skipped when already present at the
/// right size, so a concurrent render never reads a partial file.
pub fn font_path() -> Result<&'static Path> {
    if let Some(dir) = FONT_DIR.get() {
        return Ok(dir.as_path());
    }
    let dir = std::env::temp_dir().join("workforce-report-fonts");
    std::fs::create_dir_all(&dir).wrap_err("creating report font directory")?;
    for (name, bytes) in FONTS {
        let dest = dir.join(name);
        if dest.metadata().map(|m| m.len()).unwrap_or(0) == bytes.len() as u64 {
            continue;
        }
        let mut tmp = tempfile::NamedTempFile::new_in(&dir).wrap_err("staging font file")?;
        tmp.write_all(bytes).wrap_err("writing font bytes")?;
        tmp.persist(&dest)
            .map_err(|e| e.error)
            .wrap_err("persisting font file")?;
    }
    Ok(FONT_DIR.get_or_init(|| dir).as_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialises_all_bundled_fonts() {
        let dir = font_path().unwrap();
        for (name, bytes) in FONTS {
            let on_disk = std::fs::read(dir.join(name)).unwrap();
            assert_eq!(on_disk.len(), bytes.len(), "{name} fully written");
        }
        assert_eq!(font_path().unwrap(), dir, "path is stable across calls");
    }
}
