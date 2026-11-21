//! Photos in a format no model reads, made into one they do.
//!
//! An iPhone posts HEIC. The vision providers accept PNG, JPEG, GIF and WebP,
//! and a ChatGPT backend that is sent anything else fails the whole turn. So a
//! HEIC is decoded here, fitted to [`MAX_LONG_EDGE`] and written again as a
//! JPEG, which carries no EXIF, GPS or ICC data: only pixels are encoded. What
//! an image is is judged by its bytes, and its label only when they say
//! nothing. Any other format is refused by name rather than sent.

use std::borrow::Cow;
use std::sync::LazyLock;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use heic_rs::{DecodeOptions, PixelLayout};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::{resize, FilterType};
use image::RgbImage;
use tokio::sync::Semaphore;
use workforce_common::session::SessionAttachment;
use workforce_common::settings::VideoFramesConfig;

/// The image types every vision provider takes as they are.
pub const MODEL_IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The longest side of a converted photo, in pixels: the ceiling the video
/// frames already keep to, where providers start scaling images down anyway.
const MAX_LONG_EDGE: u32 = VideoFramesConfig::LONG_EDGE_CEILING;

/// The most pixels a HEIC may decode to, so a hostile header cannot claim
/// gigabytes: an RGB8 frame of this size is 192 MB.
const MAX_PIXELS: u64 = 64_000_000;

/// HEICs decoded at once, and threads each decode uses: a decode holds the
/// frame, its tiles and the resized copy.
const CONCURRENT_DECODES: usize = 2;
const DECODE_THREADS: usize = 2;

static DECODES: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(CONCURRENT_DECODES));

const JPEG_QUALITY: u8 = 85;

/// Brands of an ISO base media file that hold HEIF stills.
const HEIF_BRANDS: [&[u8]; 8] = [
    b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"mif1", b"msf1",
];

/// The HEVC brands among them: an AVIF also lists `mif1` as compatible, but
/// never one of these.
const HEVC_BRANDS: [&[u8]; 6] = [b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx"];

#[derive(Debug, thiserror::Error)]
pub enum PhotoError {
    #[error("{0} is not an image format models read; PNG, JPEG, GIF, WebP and HEIC are")]
    Unsupported(String),
    #[error("it could not be read as a HEIC image: {0}")]
    Undecodable(String),
}

/// Whether `mime` is a type every vision provider takes.
pub fn is_model_image_type(mime: &str) -> bool {
    MODEL_IMAGE_TYPES
        .iter()
        .any(|t| mime.eq_ignore_ascii_case(t))
}

/// Whether `head` (a file's first bytes) opens with a HEIF `ftyp` box whose
/// major brand, or any compatible brand but an AVIF's, is a HEIC one.
pub fn is_heif(head: &[u8]) -> bool {
    let Some((major, rest)) = head
        .get(4..16)
        .filter(|b| &b[..4] == b"ftyp")
        .map(|b| (&b[4..8], &head[16..]))
    else {
        return false;
    };
    if HEIF_BRANDS.contains(&major) {
        return true;
    }
    let boxed = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
    !matches!(major, b"avif" | b"avis")
        && rest
            .chunks_exact(4)
            .take(boxed.saturating_sub(16) / 4)
            .any(|brand| HEVC_BRANDS.contains(&brand))
}

fn is_heif_mime(mime: &str) -> bool {
    ["image/heic", "image/heif", "image/heic-sequence"]
        .iter()
        .any(|m| mime.eq_ignore_ascii_case(m))
}

/// What an image is, for the purpose of sending it to a model.
enum Looks {
    Heif,
    Model(&'static str),
    Other,
}

/// What `head` shows, and when it shows nothing, what `label` says.
fn looks(head: &[u8], label: &str) -> Looks {
    if is_heif(head) {
        return Looks::Heif;
    }
    if let Some(shown) = crate::llm::image::detect_image_media_type(head) {
        return Looks::Model(shown);
    }
    if is_heif_mime(label) {
        return Looks::Heif;
    }
    MODEL_IMAGE_TYPES
        .iter()
        .find(|t| label.eq_ignore_ascii_case(t))
        .map_or(Looks::Other, |t| Looks::Model(t))
}

/// The image media type `head` shows: a type models read, or `image/heic`.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    crate::llm::image::detect_image_media_type(head)
        .or_else(|| is_heif(head).then_some("image/heic"))
}

/// `heic` decoded, oriented as it was shot, fitted to [`MAX_LONG_EDGE`] and
/// encoded as a JPEG.
pub fn heic_to_jpeg(heic: &[u8]) -> Result<Vec<u8>, PhotoError> {
    heic_to_jpeg_within(heic, MAX_PIXELS)
}

fn heic_to_jpeg_within(heic: &[u8], max_pixels: u64) -> Result<Vec<u8>, PhotoError> {
    let undecodable = |e: &dyn std::fmt::Display| PhotoError::Undecodable(e.to_string());
    let decoded = heic_rs::decode(
        heic,
        &DecodeOptions {
            layout: PixelLayout::Rgb8,
            max_pixels: Some(max_pixels),
            apply_transforms: true,
            threads: Some(DECODE_THREADS),
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

/// `job` run on the blocking pool once a decode slot is free; a panic in it
/// is an undecodable image, not a crashed turn.
async fn decoding<T: Send + 'static>(
    job: impl FnOnce() -> Result<T, PhotoError> + Send + 'static,
) -> Result<T, PhotoError> {
    let _slot = DECODES
        .acquire()
        .await
        .map_err(|e| PhotoError::Undecodable(e.to_string()))?;
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|e| PhotoError::Undecodable(format!("the decoder failed: {e}")))?
}

/// An image's bytes as a model can read them, with their media type: as they
/// are when the bytes show a type models take (the type they show, whatever
/// the label says), a JPEG when they are a HEIC, an error for any other format.
pub async fn ready(bytes: Vec<u8>, label: &str) -> Result<(Vec<u8>, String), PhotoError> {
    match looks(&bytes, label) {
        Looks::Heif => Ok((
            decoding(move || heic_to_jpeg(&bytes)).await?,
            "image/jpeg".to_string(),
        )),
        Looks::Model(media_type) => Ok((bytes, media_type.to_string())),
        Looks::Other => Err(PhotoError::Unsupported(named(label))),
    }
}

fn named(label: &str) -> String {
    match label {
        "" => "this file".to_string(),
        label => format!("`{label}`"),
    }
}

/// The first bytes of a base64 payload, enough to tell what the file is.
pub(crate) fn head_of(data: &str) -> Vec<u8> {
    let prefix = data.get(..data.len().min(96) / 4 * 4).unwrap_or_default();
    STANDARD.decode(prefix).unwrap_or_default()
}

/// `att`, an image, as a model can read it (see [`ready`]), the attachment
/// itself when it already is. Its bytes are decoded only for a HEIC.
pub async fn model_ready(
    att: &SessionAttachment,
) -> Result<Cow<'_, SessionAttachment>, PhotoError> {
    match looks(&head_of(&att.data), &att.mime_type) {
        Looks::Heif => {
            let heic = STANDARD
                .decode(&att.data)
                .map_err(|e| PhotoError::Undecodable(e.to_string()))?;
            let jpeg = decoding(move || heic_to_jpeg(&heic)).await?;
            Ok(Cow::Owned(SessionAttachment {
                filename: att.filename.clone(),
                mime_type: "image/jpeg".to_string(),
                data: STANDARD.encode(jpeg),
                artifact_id: att.artifact_id.clone(),
            }))
        }
        Looks::Model(media_type) if att.mime_type.eq_ignore_ascii_case(media_type) => {
            Ok(Cow::Borrowed(att))
        }
        Looks::Model(media_type) => Ok(Cow::Owned(SessionAttachment {
            mime_type: media_type.to_string(),
            ..att.clone()
        })),
        Looks::Other => Err(PhotoError::Unsupported(named(&att.mime_type))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GPS_HEIC: &[u8] = include_bytes!("testdata/gps.heic");
    const GRID_HEIC: &[u8] = include_bytes!("testdata/grid.heic");
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    const TIFF: &[u8] = b"II*\0\x08\0\0\0";

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn ftyp(major: &[u8; 4], compatible: &[&[u8; 4]]) -> Vec<u8> {
        let size = (16 + 4 * compatible.len()) as u32;
        let mut bytes = size.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(major);
        bytes.extend_from_slice(&[0; 4]);
        compatible.iter().for_each(|b| bytes.extend_from_slice(*b));
        bytes
    }

    fn dimensions(jpeg: &[u8]) -> (u32, u32) {
        let decoded = image::load_from_memory(jpeg).unwrap();
        (decoded.width(), decoded.height())
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
        assert_eq!(
            dimensions(&jpeg),
            (448, 640),
            "the photo's rotation is in its pixels"
        );
    }

    #[test]
    fn a_tiled_photo_is_decoded_whole_and_fitted_to_the_long_edge() {
        assert!(heic_rs::probe(GRID_HEIC).unwrap().is_grid, "not a grid");
        assert_eq!(dimensions(&heic_to_jpeg(GRID_HEIC).unwrap()), (1568, 1176));
        let small = fitted(RgbImage::new(640, 448));
        assert_eq!((small.width(), small.height()), (640, 448));
    }

    #[test]
    fn what_cannot_be_decoded_is_said_so() {
        let corrupt = [&ftyp(b"heic", &[b"mif1"])[..], &[0xAB; 64]].concat();
        for broken in [
            &GPS_HEIC[..40],
            &corrupt[..],
            &GPS_HEIC[..GPS_HEIC.len() / 2],
        ] {
            assert!(
                matches!(heic_to_jpeg(broken), Err(PhotoError::Undecodable(_))),
                "{} bytes",
                broken.len()
            );
        }
        assert!(matches!(
            heic_to_jpeg_within(GPS_HEIC, 1000),
            Err(PhotoError::Undecodable(m)) if !m.is_empty()
        ));
    }

    #[test]
    fn a_hevc_brand_anywhere_in_the_box_makes_a_heic_but_an_avif_stays_one() {
        assert!(is_heif(&ftyp(b"mif1", &[b"miaf", b"heic"])));
        assert!(is_heif(&ftyp(b"heic", &[])));
        assert!(!is_heif(&ftyp(b"avif", &[b"mif1", b"miaf"])));
        assert!(!is_heif(&ftyp(b"mp42", &[b"isom"])));
        assert!(!is_heif(&ftyp(b"isom", &[b"mif1", b"miaf"])));
        assert!(!is_heif(PNG));
    }

    #[tokio::test]
    async fn a_decoder_that_panics_is_an_undecodable_image() {
        let err = decoding::<()>(|| panic!("the decoder broke"))
            .await
            .unwrap_err();
        assert!(matches!(err, PhotoError::Undecodable(m) if m.contains("failed")));
    }

    fn attachment(mime: &str, bytes: &[u8]) -> SessionAttachment {
        SessionAttachment::from_bytes("IMG_0001.HEIC", mime, bytes)
    }

    #[tokio::test]
    async fn a_heic_attachment_is_converted_whatever_its_label() {
        for mime in [
            "image/heic",
            "image/heif",
            "image/heic-sequence",
            "image/jpeg",
        ] {
            let att = attachment(mime, GPS_HEIC);
            let ready = model_ready(&att).await.unwrap();
            assert_eq!(ready.mime_type, "image/jpeg", "{mime}");
            assert_eq!(ready.filename, "IMG_0001.HEIC");
            let jpeg = STANDARD.decode(&ready.data).unwrap();
            assert_eq!(&jpeg[..3], [0xFF, 0xD8, 0xFF]);
        }
    }

    #[tokio::test]
    async fn the_bytes_decide_the_type_and_the_label_only_when_they_cannot() {
        let jpeg = heic_to_jpeg(GPS_HEIC).unwrap();
        let mislabelled = attachment("image/heic", &jpeg);
        let ready = model_ready(&mislabelled).await.unwrap();
        assert_eq!(ready.mime_type, "image/jpeg");
        assert_eq!(ready.data, mislabelled.data, "the bytes were not touched");

        let png_as_jpeg = attachment("image/jpeg", PNG);
        assert_eq!(
            model_ready(&png_as_jpeg).await.unwrap().mime_type,
            "image/png"
        );

        let unreadable = [0x5A; 32];
        for label in ["image/heic", "image/heif", "image/heic-sequence"] {
            assert!(
                matches!(
                    model_ready(&attachment(label, &unreadable)).await,
                    Err(PhotoError::Undecodable(_))
                ),
                "{label}"
            );
        }
        let inconclusive = attachment("image/gif", &unreadable);
        assert!(matches!(
            model_ready(&inconclusive).await.unwrap(),
            Cow::Borrowed(_)
        ));
    }

    #[tokio::test]
    async fn types_models_read_pass_untouched() {
        let png = attachment("image/png", PNG);
        assert!(matches!(model_ready(&png).await.unwrap(), Cow::Borrowed(_)));
    }

    #[tokio::test]
    async fn other_formats_are_refused_by_name() {
        let err = model_ready(&attachment("image/tiff", TIFF))
            .await
            .unwrap_err();
        assert!(matches!(&err, PhotoError::Unsupported(m) if m == "`image/tiff`"));
        assert!(err.to_string().contains("HEIC"));
    }

    #[tokio::test]
    async fn bytes_alone_are_made_ready_the_same_way() {
        let (jpeg, media_type) = ready(GPS_HEIC.to_vec(), "image/png").await.unwrap();
        assert_eq!(media_type, "image/jpeg");
        assert!(!contains(&jpeg, b"Exif"));
        let (png, media_type) = ready(PNG.to_vec(), "").await.unwrap();
        assert_eq!((png.as_slice(), media_type.as_str()), (PNG, "image/png"));
        assert!(matches!(
            ready(TIFF.to_vec(), "").await,
            Err(PhotoError::Unsupported(m)) if m == "this file"
        ));
    }
}
