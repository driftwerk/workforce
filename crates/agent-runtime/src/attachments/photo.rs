//! Photos in a format no model reads, made into one they do.
//!
//! An iPhone posts HEIC. The vision providers accept PNG, JPEG, GIF and WebP,
//! and a ChatGPT backend that is sent anything else fails the whole turn. So a
//! HEIC is decoded here, fitted to [`MAX_LONG_EDGE`] and written again as a
//! JPEG, which carries no EXIF, GPS or ICC data: only pixels are encoded. Any
//! other format is refused by name rather than sent.

use std::borrow::Cow;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use heic_rs::{DecodeOptions, PixelLayout};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{resize, FilterType};
use image::RgbImage;
use workforce_common::session::SessionAttachment;
use workforce_common::settings::VideoFramesConfig;

/// The image types every vision provider takes as they are.
pub const MODEL_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The longest side of a converted photo, in pixels: the ceiling the video
/// frames already keep to, where providers start scaling images down anyway.
const MAX_LONG_EDGE: u32 = VideoFramesConfig::LONG_EDGE_CEILING;

/// The most pixels a HEIC may decode to, so a hostile header cannot claim
/// gigabytes.
const MAX_PIXELS: u64 = 100_000_000;

const JPEG_QUALITY: u8 = 85;

/// Major brands of an ISO base media file that holds HEIF stills.
const HEIF_BRANDS: [&[u8]; 8] = [
    b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"mif1", b"msf1",
];

#[derive(Debug, thiserror::Error)]
pub enum PhotoError {
    #[error("`{0}` is not an image format models read; PNG, JPEG, GIF, WebP and HEIC are")]
    Unsupported(String),
    #[error("it could not be read as a HEIC image: {0}")]
    Undecodable(String),
}

/// Whether `bytes` open with a HEIF `ftyp` box.
pub fn is_heif(bytes: &[u8]) -> bool {
    bytes
        .get(4..12)
        .is_some_and(|b| &b[..4] == b"ftyp" && HEIF_BRANDS.contains(&&b[4..]))
}

fn is_heif_mime(mime: &str) -> bool {
    ["image/heic", "image/heif", "image/heic-sequence"]
        .iter()
        .any(|m| mime.eq_ignore_ascii_case(m))
}

/// The image media type `head` (a file's first bytes) shows: a type models
/// read, or `image/heic`.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    crate::llm::image::detect_image_media_type(head)
        .or_else(|| is_heif(head).then_some("image/heic"))
}

/// `heic` decoded, oriented as it was shot, fitted to [`MAX_LONG_EDGE`] and
/// encoded as a JPEG.
pub fn heic_to_jpeg(heic: &[u8]) -> Result<Vec<u8>, PhotoError> {
    let undecodable = |e: &dyn std::fmt::Display| PhotoError::Undecodable(e.to_string());
    let decoded = heic_rs::decode(
        heic,
        &DecodeOptions {
            layout: PixelLayout::Rgb8,
            max_pixels: Some(MAX_PIXELS),
            apply_transforms: true,
            ..DecodeOptions::default()
        },
    )
    .map_err(|e| undecodable(&e))?;
    let rgb = RgbImage::from_raw(decoded.width, decoded.height, decoded.data)
        .ok_or_else(|| undecodable(&"its pixels do not fill its size"))?;
    let rgb = fitted(rgb);
    let mut jpeg = Vec::new();
    rgb.write_with_encoder(JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY))
        .map_err(|e| undecodable(&e))?;
    Ok(jpeg)
}

fn fitted(rgb: RgbImage) -> RgbImage {
    let long = rgb.width().max(rgb.height());
    if long <= MAX_LONG_EDGE {
        return rgb;
    }
    let scaled =
        |side: u32| ((u64::from(side) * u64::from(MAX_LONG_EDGE)) / u64::from(long)).max(1) as u32;
    resize(
        &rgb,
        scaled(rgb.width()),
        scaled(rgb.height()),
        FilterType::Lanczos3,
    )
}

/// `att`, an image, as a model can read it: itself when its type is one
/// models take, a JPEG when it is a HEIC (by its bytes or its type), an error
/// for any other format.
pub async fn model_ready(
    att: &SessionAttachment,
) -> Result<Cow<'_, SessionAttachment>, PhotoError> {
    let head = att
        .data
        .get(..16)
        .and_then(|prefix| STANDARD.decode(prefix).ok());
    let heif = head.as_deref().is_some_and(is_heif) || is_heif_mime(&att.mime_type);
    if heif {
        let data = att.data.clone();
        let jpeg = tokio::task::spawn_blocking(move || {
            let heic = STANDARD
                .decode(data)
                .map_err(|e| PhotoError::Undecodable(e.to_string()))?;
            heic_to_jpeg(&heic)
        })
        .await
        .map_err(|e| PhotoError::Undecodable(e.to_string()))??;
        return Ok(Cow::Owned(SessionAttachment {
            mime_type: "image/jpeg".to_string(),
            data: STANDARD.encode(jpeg),
            ..att.clone()
        }));
    }
    if MODEL_IMAGE_TYPES
        .iter()
        .any(|t| att.mime_type.eq_ignore_ascii_case(t))
    {
        return Ok(Cow::Borrowed(att));
    }
    Err(PhotoError::Unsupported(att.mime_type.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GPS_HEIC: &[u8] = include_bytes!("testdata/gps.heic");

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn the_fixture_carries_the_location_it_is_stripped_of() {
        assert!(is_heif(GPS_HEIC));
        assert!(contains(GPS_HEIC, b"Exif"));
    }

    #[test]
    fn a_heic_becomes_a_jpeg_without_metadata() {
        let jpeg = heic_to_jpeg(GPS_HEIC).unwrap();
        assert_eq!(&jpeg[..3], [0xFF, 0xD8, 0xFF]);
        assert!(!contains(&jpeg, b"Exif"), "EXIF survived");
        assert!(
            !contains(&jpeg, b"heic-fixture"),
            "the EXIF comment survived"
        );
        let decoded = image::load_from_memory(&jpeg).unwrap();
        assert_eq!(
            (decoded.width(), decoded.height()),
            (448, 640),
            "the photo's rotation is in its pixels"
        );
    }

    #[test]
    fn a_large_photo_is_fitted_to_the_long_edge() {
        let fitted = fitted(RgbImage::new(4032, 3024));
        assert_eq!((fitted.width(), fitted.height()), (1568, 1176));
        let small = fitted_of(640, 448);
        assert_eq!(small, (640, 448));
    }

    fn fitted_of(width: u32, height: u32) -> (u32, u32) {
        let image = fitted(RgbImage::new(width, height));
        (image.width(), image.height())
    }

    #[test]
    fn bytes_that_are_not_a_heic_are_not_one() {
        assert!(!is_heif(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"));
        assert!(!is_heif(b"\0\0\0\x20ftypavif\0\0\0\0"));
        assert!(matches!(
            heic_to_jpeg(&GPS_HEIC[..40]),
            Err(PhotoError::Undecodable(_))
        ));
    }

    fn attachment(mime: &str, bytes: &[u8]) -> SessionAttachment {
        SessionAttachment::from_bytes("IMG_0001.HEIC", mime, bytes)
    }

    #[tokio::test]
    async fn a_heic_attachment_is_converted_whatever_its_label() {
        for mime in ["image/heic", "image/heif", "image/jpeg"] {
            let att = attachment(mime, GPS_HEIC);
            let ready = model_ready(&att).await.unwrap();
            assert_eq!(ready.mime_type, "image/jpeg", "{mime}");
            assert_eq!(ready.filename, "IMG_0001.HEIC");
            let jpeg = STANDARD.decode(&ready.data).unwrap();
            assert_eq!(&jpeg[..3], [0xFF, 0xD8, 0xFF]);
        }
    }

    #[tokio::test]
    async fn types_models_read_pass_untouched() {
        let png = attachment("image/png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        assert!(matches!(model_ready(&png).await.unwrap(), Cow::Borrowed(_)));
    }

    #[tokio::test]
    async fn other_formats_are_refused_by_name() {
        let tiff = attachment("image/tiff", b"II*\0\x08\0\0\0");
        let err = model_ready(&tiff).await.unwrap_err();
        assert!(matches!(&err, PhotoError::Unsupported(m) if m == "image/tiff"));
        assert!(err.to_string().contains("HEIC"));
    }
}
