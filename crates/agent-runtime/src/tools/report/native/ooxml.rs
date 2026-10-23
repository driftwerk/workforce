//! Primitives shared by the OOXML backends: a minimal ZIP package builder, XML
//! escaping (reusing the chart escaper), colour/geometry conversions, and
//! intrinsic image-size sniffing for embed extents.

use std::io::{Cursor, Write};

use eyre::Result;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::ImageFormat;

/// Printable column width of an A4 page with 1-inch margins, in EMU. Drives
/// image sizing in the Word backend.
pub(crate) const DOCX_CONTENT_WIDTH_EMU: i64 = 5_731_510;

/// Word and PowerPoint both ship this face, so a `code` block stays monospaced
/// without embedding a font in either package.
pub(crate) const MONO_FONT: &str = "Consolas";

/// Escape text/attribute content for XML, reusing the chart SVG escaper (same
/// rules: `& < > " '` plus control-character stripping).
pub(crate) fn esc(s: &str) -> String {
    crate::tools::report::chart::xml_escape(s)
}

/// Normalize a theme `#RRGGBB` colour to the bare uppercase `RRGGBB` OOXML wants,
/// falling back to black for anything malformed.
pub(crate) fn hex6(color: &str) -> String {
    let s = color.trim_start_matches('#');
    if s.len() == 6 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        s.to_ascii_uppercase()
    } else {
        "000000".into()
    }
}

/// Display extent in EMU for an image scaled to `pct` of `max_width_emu`, with
/// height derived from the intrinsic pixel aspect ratio.
pub(crate) fn emu_extent(width_px: u32, height_px: u32, max_width_emu: i64, pct: u8) -> (i64, i64) {
    let target_w = (max_width_emu * i64::from(pct.clamp(1, 100)) / 100).max(1);
    let target_h = if width_px == 0 {
        target_w
    } else {
        target_w * i64::from(height_px) / i64::from(width_px)
    };
    (target_w, target_h.max(1))
}

/// Intrinsic pixel dimensions of a PNG/JPEG/GIF, read from its header.
pub(crate) fn image_dimensions(bytes: &[u8], format: ImageFormat) -> Option<(u32, u32)> {
    match format {
        ImageFormat::Png => png_dimensions(bytes),
        ImageFormat::Gif => gif_dimensions(bytes),
        ImageFormat::Jpeg => jpeg_dimensions(bytes),
    }
}

fn png_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || &b[0..8] != b"\x89PNG\r\n\x1a\n" || &b[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
    let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
    (w > 0 && h > 0).then_some((w, h))
}

fn gif_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 10 || (&b[0..6] != b"GIF87a" && &b[0..6] != b"GIF89a") {
        return None;
    }
    let w = u16::from_le_bytes([b[6], b[7]]);
    let h = u16::from_le_bytes([b[8], b[9]]);
    (w > 0 && h > 0).then_some((u32::from(w), u32::from(h)))
}

fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 9 < b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        // A run of 0xFF fill bytes may precede a marker — consume one and retry
        // so the real marker on the next byte isn't skipped.
        if marker == 0xFF {
            i += 1;
            continue;
        }
        // Standalone markers (RSTn, TEM) carry no length segment.
        if (0xD0..=0xD9).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let len = usize::from(u16::from_be_bytes([b[i + 2], b[i + 3]]));
        // Start-of-frame markers carry the dimensions; skip the arithmetic/
        // huffman/restart variants that don't.
        let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            let h = u16::from_be_bytes([b[i + 5], b[i + 6]]);
            let w = u16::from_be_bytes([b[i + 7], b[i + 8]]);
            return (w > 0 && h > 0).then_some((u32::from(w), u32::from(h)));
        }
        i += 2 + len;
    }
    None
}

/// A ZIP package under construction. OOXML files are ZIP archives; XML parts are
/// deflated and already-compressed media is stored.
pub(crate) struct Package {
    zip: ZipWriter<Cursor<Vec<u8>>>,
}

impl Package {
    pub(crate) fn new() -> Self {
        Self {
            zip: ZipWriter::new(Cursor::new(Vec::new())),
        }
    }

    pub(crate) fn add_xml(&mut self, path: &str, content: &str) -> Result<()> {
        self.add(path, content.as_bytes(), CompressionMethod::Deflated)
    }

    pub(crate) fn add_media(&mut self, path: &str, bytes: &[u8]) -> Result<()> {
        self.add(path, bytes, CompressionMethod::Stored)
    }

    fn add(&mut self, path: &str, bytes: &[u8], method: CompressionMethod) -> Result<()> {
        self.zip.start_file(
            path,
            SimpleFileOptions::default().compression_method(method),
        )?;
        self.zip.write_all(bytes)?;
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<Vec<u8>> {
        Ok(self.zip.finish()?.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn hex6_normalizes_and_guards() {
        assert_eq!(hex6("#3b5266"), "3B5266");
        assert_eq!(hex6("3B5266"), "3B5266");
        assert_eq!(hex6("#xyz"), "000000");
        assert_eq!(hex6(""), "000000");
    }

    #[test]
    fn emu_extent_preserves_aspect_and_scales() {
        let (w, h) = emu_extent(800, 400, 6_000_000, 100);
        assert_eq!(w, 6_000_000);
        assert_eq!(h, 3_000_000);
        let (w, _) = emu_extent(800, 400, 6_000_000, 50);
        assert_eq!(w, 3_000_000);
        // Degenerate width never divides by zero.
        let (w, h) = emu_extent(0, 400, 6_000_000, 100);
        assert_eq!((w, h), (6_000_000, 6_000_000));
    }

    #[test]
    fn png_dimensions_reads_ihdr() {
        let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
        b.extend_from_slice(&[0, 0, 0, 13]); // length
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&720u32.to_be_bytes());
        b.extend_from_slice(&360u32.to_be_bytes());
        assert_eq!(png_dimensions(&b), Some((720, 360)));
        assert_eq!(png_dimensions(b"not a png"), None);
    }

    #[test]
    fn gif_dimensions_reads_logical_screen() {
        let mut b = b"GIF89a".to_vec();
        b.extend_from_slice(&100u16.to_le_bytes());
        b.extend_from_slice(&50u16.to_le_bytes());
        assert_eq!(gif_dimensions(&b), Some((100, 50)));
    }

    #[test]
    fn jpeg_dimensions_scans_to_sof() {
        // SOI, APP0 (len 4, skipped), a 0xFF fill byte, then SOF0 with 48x64.
        let mut b = vec![0xFF, 0xD8];
        b.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]);
        b.push(0xFF); // fill byte preceding the marker — must not skip the SOF
        b.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        b.extend_from_slice(&64u16.to_be_bytes()); // height
        b.extend_from_slice(&48u16.to_be_bytes()); // width
        b.extend_from_slice(&[0; 8]);
        assert_eq!(jpeg_dimensions(&b), Some((48, 64)));
        assert_eq!(jpeg_dimensions(b"not a jpeg"), None);
    }

    #[test]
    fn package_round_trips_parts() {
        let mut pkg = Package::new();
        pkg.add_xml("a/b.xml", "<x/>").unwrap();
        pkg.add_media("media/i.png", &[1, 2, 3]).unwrap();
        let bytes = pkg.finish().unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        assert_eq!(zip.len(), 2);
        let mut s = String::new();
        zip.by_name("a/b.xml")
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "<x/>");
    }
}
