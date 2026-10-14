// Thumbnails for files tessellations: a small JPEG (or PNG when the image has
// transparency) that fits in a square, made from images the `image` crate
// decodes (JPEG, PNG, GIF, WebP, BMP, TIFF), from the preview Photoshop
// embeds in PSD files, and from a keyframe of H.264 MP4 and QuickTime video
// (see `video`). EXIF orientation and video rotation are applied. Decoding has
// memory and size limits, and runs inside the same bounded `Source` as extraction.

use super::Source;
use image::{codecs::jpeg::JpegEncoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use std::io::Cursor;

/// Largest file decoded for a thumbnail.
const MAX_SOURCE: u64 = 64 * 1024 * 1024;

pub struct Thumbnail {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub content_type: &'static str,
}

const DECODED: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp", "image/bmp", "image/tiff", "image/x-ms-bmp"];

/// Whether a thumbnail can be made for this type.
pub fn supported(content_type: &str) -> bool {
    DECODED.contains(&content_type) || content_type == "image/vnd.adobe.photoshop" || (cfg!(feature = "video-thumbnails") && super::video::TYPES.contains(&content_type))
}

/// A thumbnail at most `max_px` on each side, or `None` for types without one.
pub fn make(source: &mut Source, content_type: &str, max_px: u32) -> anyhow::Result<Option<Thumbnail>> {
    if super::video::TYPES.contains(&content_type) {
        return match super::video::keyframe(source)? {
            Some(frame) => encode(DynamicImage::ImageRgb8(frame), max_px).map(Some),
            None => Ok(None),
        };
    }
    let encoded = if DECODED.contains(&content_type) {
        source.all(MAX_SOURCE)?
    } else if content_type == "image/vnd.adobe.photoshop" {
        match psd_preview(source)? {
            Some(jpeg) => jpeg,
            None => return Ok(None),
        }
    } else {
        return Ok(None);
    };
    let mut reader = ImageReader::new(Cursor::new(encoded)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(512 * 1024 * 1024);
    limits.max_image_width = Some(40_000);
    limits.max_image_height = Some(40_000);
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    encode(image, max_px).map(Some)
}

/// Candidate video thumbnails: `count` keyframes spread through the video
/// (fewer when keyframes repeat), each at most `max_px` on a side.
pub fn video_frames(source: &mut Source, count: usize, max_px: u32) -> anyhow::Result<Vec<Thumbnail>> {
    let fractions: Vec<f64> = (1..=count).map(|i| i as f64 / (count + 1) as f64).collect();
    super::video::keyframes(source, &fractions)?.into_iter().map(|f| encode(DynamicImage::ImageRgb8(f), max_px)).collect()
}

/// Shrink to fit `max_px` (never enlarged) and encode: PNG to keep transparency, JPEG otherwise.
fn encode(image: DynamicImage, max_px: u32) -> anyhow::Result<Thumbnail> {
    let small = if image.width() <= max_px && image.height() <= max_px { image } else { image.thumbnail(max_px, max_px) };
    let mut bytes = Vec::new();
    let content_type = if small.color().has_alpha() {
        small.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)?;
        "image/png"
    } else {
        small.to_rgb8().write_with_encoder(JpegEncoder::new_with_quality(&mut bytes, 80))?;
        "image/jpeg"
    };
    Ok(Thumbnail { bytes, width: small.width(), height: small.height(), content_type })
}

/// The JPEG preview Photoshop stores in image resource 0x040C (0x0409 in old files).
fn psd_preview(source: &mut Source) -> anyhow::Result<Option<Vec<u8>>> {
    let head = source.head(30)?;
    if head.len() < 30 || &head[..4] != b"8BPS" {
        return Ok(None);
    }
    let color_len = u32::from_be_bytes(head[26..30].try_into().unwrap()) as u64;
    let at = 30 + color_len;
    let len_bytes = source.range(at, 4)?;
    let Ok(len) = <[u8; 4]>::try_from(len_bytes.as_slice()) else { return Ok(None) };
    let len = u32::from_be_bytes(len) as usize;
    if len > 32 * 1024 * 1024 {
        return Ok(None);
    }
    let resources = source.range(at + 4, len)?;
    for (id, data) in super::image::photoshop_resources(&resources) {
        // Format (4: 1 = JFIF), width, height, widthbytes, total size, compressed size, bits, planes; then the JPEG.
        if (id == 0x040C || id == 0x0409) && data.len() > 28 && u32::from_be_bytes(data[..4].try_into().unwrap()) == 1 {
            return Ok(Some(data[28..].to_vec()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Limits, Source};

    fn source(bytes: Vec<u8>) -> Source {
        Source::new(Box::new(bytes), &Limits::default())
    }

    #[test]
    fn thumbnails_keep_aspect_and_transparency() {
        let mut photo = Vec::new();
        image::RgbImage::from_fn(1200, 600, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]))
            .write_to(&mut Cursor::new(&mut photo), ImageFormat::Jpeg)
            .unwrap();
        let thumb = make(&mut source(photo), "image/jpeg", 320).unwrap().unwrap();
        assert_eq!((thumb.width, thumb.height), (320, 160));
        assert_eq!(thumb.content_type, "image/jpeg");
        assert!(thumb.bytes.starts_with(&[0xFF, 0xD8]));

        let mut icon = Vec::new();
        image::RgbaImage::from_fn(64, 128, |_, _| image::Rgba([0, 0, 0, 0])).write_to(&mut Cursor::new(&mut icon), ImageFormat::Png).unwrap();
        let thumb = make(&mut source(icon), "image/png", 320).unwrap().unwrap();
        assert_eq!((thumb.width, thumb.height), (64, 128), "never enlarged");
        assert_eq!(thumb.content_type, "image/png");

        assert!(make(&mut source(b"not an image".to_vec()), "image/png", 320).is_err());
        assert!(make(&mut source(b"text".to_vec()), "text/plain", 320).unwrap().is_none());
    }
}
