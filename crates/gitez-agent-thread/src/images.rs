//! Images given to an agent with a prompt: made fit to send
//! ([`prepare_image`]), described in the turn that sends them
//! ([`TurnImage`]), and shrunk for showing ([`thumbnail`]).
//!
//! The limits follow Anthropic's vision documentation: PNG, JPEG, GIF and
//! WebP are read by the models, at most 8000 px on a side (less once a
//! request holds more than 20 images, which a long thread can reach: the
//! documentation advises at most 3000 px then), and at most 5 MB of base64
//! per image on some platforms (Amazon Bedrock; 10 MB on the Claude API). Images are scaled down to a long edge of 2576 px, the
//! largest any model reads before downscaling itself, and kept under 5 MB as
//! base64.
//!
//! Everything here is CPU work on whole images: run it off the UI thread.

use std::io::Cursor;

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView as _, ImageFormat, ImageReader, Limits};
use serde::{Deserialize, Serialize};

/// The most images one message (a prompt or a reply) can carry.
pub const MAX_IMAGES: usize = 5;
/// The longest side of an image as sent, in pixels; larger ones are scaled
/// down.
pub const MAX_EDGE: u32 = 2576;
/// The most bytes an image may have as sent: 5 MiB once base64-encoded.
pub const MAX_BYTES: usize = 5 * 1024 * 1024 / 4 * 3;
/// Input larger than this is refused before it is read.
pub const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;
/// Images with more pixels than this are refused before they are decoded.
const MAX_INPUT_PIXELS: u64 = 120_000_000;
/// The shortest long edge an image is shrunk to while it does not fit
/// [`MAX_BYTES`]; below it the image is refused.
const MIN_EDGE: u32 = 512;
/// JPEG quality for a large photo that does not fit [`MAX_BYTES`] as PNG.
const JPEG_QUALITY: u8 = 85;

/// The formats an agent is given images in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ImageType {
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    Jpeg,
    #[serde(rename = "image/gif")]
    Gif,
    #[serde(rename = "image/webp")]
    Webp,
}

impl ImageType {
    /// Its media type, such as `image/png`.
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    /// The extension of its files, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }

    /// "PNG", "JPEG", … for the user.
    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Gif => "GIF",
            Self::Webp => "WebP",
        }
    }

    fn of(format: ImageFormat) -> Option<Self> {
        match format {
            ImageFormat::Png => Some(Self::Png),
            ImageFormat::Jpeg => Some(Self::Jpeg),
            ImageFormat::Gif => Some(Self::Gif),
            ImageFormat::WebP => Some(Self::Webp),
            _ => None,
        }
    }

    fn format(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpeg => ImageFormat::Jpeg,
            Self::Gif => ImageFormat::Gif,
            Self::Webp => ImageFormat::WebP,
        }
    }
}

/// An image sent with a turn, as `thread.json` lists it. The file is
/// `images/<name>` in the thread's folder ([`crate::Store::image_path`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnImage {
    /// A file name GitEZ made, such as `2026-11-13T09-41-31Z-1.png`.
    pub name: String,
    pub media_type: ImageType,
    /// Bytes.
    pub size: u64,
    pub width: u32,
    pub height: u32,
}

/// An image ready to send: in a format the models read, within the limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImage {
    pub bytes: Vec<u8>,
    pub media_type: ImageType,
    pub width: u32,
    pub height: u32,
    /// How it was changed to fit, if it was.
    pub changed: Option<Changed>,
}

/// How [`prepare_image`] changed an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Changed {
    /// The format it came in, such as "TIFF".
    pub from_format: &'static str,
    pub from_width: u32,
    pub from_height: u32,
}

/// Why an image cannot be sent, said for the user.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    #[error("This is not an image GitEZ can read: use PNG, JPEG, GIF, WebP, TIFF or BMP.")]
    Unsupported,
    #[error("GitEZ could not read the image: {0}")]
    Unreadable(String),
    #[error("The image is too large to read: {0}.")]
    TooLarge(String),
    #[error(
        "The image is {width} × {height} pixels: too long and thin to scale down to \
         {MAX_EDGE} pixels on its long side."
    )]
    TooThin { width: u32, height: u32 },
    #[error(
        "The image cannot be made smaller than 5 MB without shrinking it below \
         {MIN_EDGE} pixels. Crop it or save it as JPEG."
    )]
    CannotFit,
    #[error("GitEZ could not convert the image: {0}")]
    Encode(String),
}

/// `bytes` as an image the models read within the limits: PNG, JPEG, GIF
/// and WebP that fit are kept as they are; others (TIFF, BMP, or too
/// large) are decoded, scaled down to [`MAX_EDGE`], and encoded as PNG
/// (or JPEG for a large opaque image that does not fit as PNG). The format
/// is told from the bytes, never from a name.
pub fn prepare_image(bytes: &[u8]) -> Result<PreparedImage, ImageError> {
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(ImageError::TooLarge(format!(
            "it has {}, GitEZ reads at most {}",
            size_text(bytes.len() as u64),
            size_text(MAX_INPUT_BYTES)
        )));
    }
    let format = image::guess_format(bytes).map_err(|_| ImageError::Unsupported)?;
    if !matches!(
        format,
        ImageFormat::Png
            | ImageFormat::Jpeg
            | ImageFormat::Gif
            | ImageFormat::WebP
            | ImageFormat::Tiff
            | ImageFormat::Bmp
    ) {
        return Err(ImageError::Unsupported);
    }
    let (width, height) = reader(bytes, format)
        .into_dimensions()
        .map_err(|error| ImageError::Unreadable(error.to_string()))?;
    if u64::from(width) * u64::from(height) > MAX_INPUT_PIXELS {
        return Err(ImageError::TooLarge(format!(
            "{width} × {height} pixels is more than GitEZ decodes"
        )));
    }
    // Decoded even when kept as it is: a broken file would fail the turn.
    let mut decoder = reader(bytes, format)
        .into_decoder()
        .map_err(|error| ImageError::Unreadable(error.to_string()))?;
    let orientation = image::ImageDecoder::orientation(&mut decoder)
        .map_err(|error| ImageError::Unreadable(error.to_string()))?;
    let mut decoded = DynamicImage::from_decoder(decoder)
        .map_err(|error| ImageError::Unreadable(error.to_string()))?;
    let kept =
        ImageType::of(format).filter(|_| width.max(height) <= MAX_EDGE && bytes.len() <= MAX_BYTES);
    if let Some(media_type) = kept {
        return Ok(PreparedImage {
            bytes: bytes.to_vec(),
            media_type,
            width,
            height,
            changed: None,
        });
    }
    decoded.apply_orientation(orientation);
    let changed = Changed {
        from_format: format_label(format),
        from_width: decoded.width(),
        from_height: decoded.height(),
    };
    let mut edge = decoded.width().max(decoded.height()).min(MAX_EDGE);
    loop {
        let scaled = scale_to(&decoded, edge)?;
        let (png, opaque) = (
            encode(&scaled, ImageType::Png)?,
            !scaled.color().has_alpha(),
        );
        let fits = if png.len() <= MAX_BYTES {
            Some((png, ImageType::Png))
        } else if opaque {
            let jpeg = encode(&scaled, ImageType::Jpeg)?;
            (jpeg.len() <= MAX_BYTES).then_some((jpeg, ImageType::Jpeg))
        } else {
            None
        };
        if let Some((bytes, media_type)) = fits {
            return Ok(PreparedImage {
                bytes,
                media_type,
                width: scaled.width(),
                height: scaled.height(),
                changed: Some(changed),
            });
        }
        if edge <= MIN_EDGE {
            return Err(ImageError::CannotFit);
        }
        edge = (edge * 3 / 4).max(MIN_EDGE);
    }
}

/// A small copy of an image for showing it: BGRA pixels, as GPUI draws
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    /// `width × height` pixels, four bytes each, blue first.
    pub bgra: Vec<u8>,
}

/// A copy of the image in `bytes` whose short side is at most `short`
/// pixels (it is never enlarged) and whose long side is at most four times
/// that (the middle of a longer image), for a square tile that covers its
/// box.
pub fn thumbnail(bytes: &[u8], short: u32) -> Result<Thumbnail, ImageError> {
    let format = image::guess_format(bytes).map_err(|_| ImageError::Unsupported)?;
    let (width, height) = reader(bytes, format)
        .into_dimensions()
        .map_err(|error| ImageError::Unreadable(error.to_string()))?;
    if u64::from(width) * u64::from(height) > MAX_INPUT_PIXELS {
        return Err(ImageError::TooLarge(format!("{width} × {height} pixels")));
    }
    let decoded = reader(bytes, format)
        .decode()
        .map_err(|error| ImageError::Unreadable(error.to_string()))?;
    let short = short.max(1);
    let (width, height) = (decoded.width(), decoded.height());
    let scale = (f64::from(short) / f64::from(width.min(height).max(1))).min(1.0);
    let scaled_width = ((f64::from(width) * scale).round() as u32).max(1);
    let scaled_height = ((f64::from(height) * scale).round() as u32).max(1);
    let mut small = if scale < 1.0 {
        decoded.thumbnail_exact(scaled_width, scaled_height)
    } else {
        decoded
    };
    let long = short.saturating_mul(4);
    if small.width() > long || small.height() > long {
        let (crop_width, crop_height) = (small.width().min(long), small.height().min(long));
        small = small.crop_imm(
            (small.width() - crop_width) / 2,
            (small.height() - crop_height) / 2,
            crop_width,
            crop_height,
        );
    }
    let mut rgba = small.into_rgba8();
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok(Thumbnail {
        width: rgba.width(),
        height: rgba.height(),
        bgra: rgba.into_raw(),
    })
}

/// "1.2 MB", "340 KB", for the user.
pub fn size_text(bytes: u64) -> String {
    const KB: f64 = 1000.0;
    let bytes = bytes as f64;
    if bytes >= KB * KB {
        format!("{:.1} MB", bytes / KB / KB)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes / KB)
    } else {
        format!("{bytes:.0} bytes")
    }
}

/// A reader of `bytes` in `format`, with limits that keep a hostile file
/// from taking all memory.
fn reader(bytes: &[u8], format: ImageFormat) -> ImageReader<Cursor<&[u8]>> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_alloc = Some(1024 * 1024 * 1024);
    reader.limits(limits);
    reader
}

/// `image` scaled down so its long side is `edge` pixels.
fn scale_to(image: &DynamicImage, edge: u32) -> Result<DynamicImage, ImageError> {
    let (width, height) = image.dimensions();
    let long = width.max(height);
    if long <= edge {
        return Ok(image.clone());
    }
    let scale = f64::from(edge) / f64::from(long);
    let new_width = (f64::from(width) * scale).round() as u32;
    let new_height = (f64::from(height) * scale).round() as u32;
    if new_width == 0 || new_height == 0 {
        return Err(ImageError::TooThin { width, height });
    }
    Ok(image.resize_exact(new_width, new_height, FilterType::Lanczos3))
}

fn encode(image: &DynamicImage, media_type: ImageType) -> Result<Vec<u8>, ImageError> {
    let mut out = Cursor::new(Vec::new());
    let result = match media_type {
        ImageType::Jpeg => {
            let encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
            image.to_rgb8().write_with_encoder(encoder)
        }
        _ => image.write_to(&mut out, media_type.format()),
    };
    result.map_err(|error| ImageError::Encode(error.to_string()))?;
    Ok(out.into_inner())
}

fn format_label(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Tiff => "TIFF",
        ImageFormat::Bmp => "BMP",
        other => ImageType::of(other).map_or("image", ImageType::label),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8])
        });
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut out, ImageFormat::Png)
            .expect("encode");
        out.into_inner()
    }

    #[test]
    fn a_small_png_is_sent_as_it_is() {
        let bytes = png(40, 30);
        let prepared = prepare_image(&bytes).expect("prepared");
        assert_eq!(prepared.bytes, bytes);
        assert_eq!(prepared.media_type, ImageType::Png);
        assert_eq!((prepared.width, prepared.height), (40, 30));
        assert_eq!(prepared.changed, None);
    }

    #[test]
    fn tiff_and_bmp_become_png() {
        for format in [ImageFormat::Tiff, ImageFormat::Bmp] {
            let image = DynamicImage::ImageRgb8(image::RgbImage::new(20, 10));
            let mut bytes = Cursor::new(Vec::new());
            image.write_to(&mut bytes, format).expect("encode");
            let prepared = prepare_image(&bytes.into_inner()).expect("prepared");
            assert_eq!(prepared.media_type, ImageType::Png, "{format:?}");
            assert_eq!(
                image::guess_format(&prepared.bytes).expect("a format"),
                ImageFormat::Png
            );
            assert_eq!((prepared.width, prepared.height), (20, 10));
            assert_eq!(
                prepared.changed.map(|changed| changed.from_format),
                Some(format_label(format))
            );
        }
    }

    #[test]
    fn a_large_image_is_scaled_down_to_the_long_edge() {
        let prepared = prepare_image(&png(3000, 1500)).expect("prepared");
        assert_eq!((prepared.width, prepared.height), (MAX_EDGE, MAX_EDGE / 2));
        assert!(prepared.bytes.len() <= MAX_BYTES);
        let changed = prepared.changed.expect("changed");
        assert_eq!((changed.from_width, changed.from_height), (3000, 1500));
    }

    #[test]
    fn a_noisy_photo_too_big_as_png_becomes_jpeg() {
        // Noise does not compress: as PNG it is far over the limit.
        let mut seed = 7_u32;
        let image = image::RgbImage::from_fn(2400, 1800, |_, _| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let [a, b, c, _] = seed.to_le_bytes();
            image::Rgb([a, b, c])
        });
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("encode");
        let bytes = bytes.into_inner();
        assert!(bytes.len() > MAX_BYTES);
        let prepared = prepare_image(&bytes).expect("prepared");
        assert_eq!(prepared.media_type, ImageType::Jpeg);
        assert!(prepared.bytes.len() <= MAX_BYTES);
        assert!(prepared.width <= 2400);
    }

    #[test]
    fn what_is_not_an_image_is_refused() {
        assert_eq!(prepare_image(b"hello, world"), Err(ImageError::Unsupported));
        // A PNG signature with nothing after it.
        assert!(matches!(
            prepare_image(b"\x89PNG\r\n\x1a\n"),
            Err(ImageError::Unreadable(_))
        ));
        // An image too long and thin to scale down.
        assert!(matches!(
            prepare_image(&png(1, 6000)),
            Err(ImageError::TooThin { .. })
        ));
    }

    #[test]
    fn a_thumbnail_covers_a_square_and_is_bgra() {
        let image = image::RgbImage::from_pixel(400, 100, image::Rgb([10, 20, 30]));
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("encode");
        let thumbnail = thumbnail(&bytes.into_inner(), 20).expect("thumbnail");
        assert_eq!((thumbnail.width, thumbnail.height), (80, 20));
        assert_eq!(thumbnail.bgra.len(), 80 * 20 * 4);
        assert_eq!(thumbnail.bgra[..4], [30, 20, 10, 255]);
        // Never enlarged.
        let small = thumbnail_of_png(10, 10, 64);
        assert_eq!((small.width, small.height), (10, 10));
    }

    fn thumbnail_of_png(width: u32, height: u32, short: u32) -> Thumbnail {
        thumbnail(&png(width, height), short).expect("thumbnail")
    }

    #[test]
    fn media_types_are_stored_by_name() {
        assert_eq!(
            serde_json::to_string(&ImageType::Webp).expect("json"),
            r#""image/webp""#
        );
        assert_eq!(
            serde_json::from_str::<ImageType>(r#""image/jpeg""#).expect("json"),
            ImageType::Jpeg
        );
    }
}
