// Pictures of files that aren't pictures: an SVG drawn, a PDF page rendered,
// and the JPEG preview a camera stores in its RAW files. For thumbnails and,
// for PDF pages, OCR (`crate::ocr`). All pure Rust, run where the rest of
// parsing runs (the parser worker, when the sandbox is on).
//
//   SVG   resvg, with no system fonts and no external files: only images
//         embedded as data: URLs are drawn (an href to a file or URL is left out).
//   PDF   hayro, which embeds substitutes for the 14 standard PDF fonts.
//   RAW   the largest JPEG the file embeds: TIFF-based formats (CR2, NEF, ARW,
//         DNG, ORF, RW2, PEF, SRW, 3FR, IIQ) keep it in an IFD; Fujifilm RAF
//         names it in its header; Canon CR3 keeps it in a PRVW box.
//
// Every size from the file is checked before it's used, and pictures are
// capped in pixels, so a hostile file can't make these allocate without bound.

use super::Source;
use image::RgbaImage;

/// Longest side of a rendered picture, whatever the caller asks.
pub const MAX_SIDE: u32 = 4096;
/// Largest SVG or PDF read.
const MAX_DOCUMENT: u64 = 64 * 1024 * 1024;
/// Largest embedded preview taken from a RAW file.
const MAX_PREVIEW: u64 = 32 * 1024 * 1024;

/// RAW camera formats, by extension (their content types vary by tool: often `image/tiff`).
pub const RAW_EXTENSIONS: &[&str] = &["cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "dng", "raf", "orf", "rw2", "pef", "srw", "3fr", "iiq", "erf", "kdc", "mrw"];

/// True for a RAW camera file's name.
pub fn is_raw(name: &str) -> bool {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    RAW_EXTENSIONS.contains(&ext.as_str())
}

/// Scale so `(w, h)` fits in `max_px`, never enlarging past `max_px` either.
fn fit(w: f32, h: f32, max_px: u32) -> f32 {
    let side = w.max(h).max(1.0);
    max_px.min(MAX_SIDE) as f32 / side
}

/// An SVG drawn to fit `max_px` (on a transparent background).
pub fn svg(source: &mut Source, max_px: u32) -> anyhow::Result<Option<RgbaImage>> {
    use resvg::{tiny_skia, usvg};
    let data = source.all(MAX_DOCUMENT)?;
    let mut options = usvg::Options { resources_dir: None, ..Default::default() };
    // Only embedded images: a path or URL in an href is never followed.
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    let tree = usvg::Tree::from_data(&data, &options)?;
    let size = tree.size();
    let scale = fit(size.width(), size.height(), max_px);
    let (w, h) = (((size.width() * scale).round() as u32).max(1), ((size.height() * scale).round() as u32).max(1));
    let Some(mut pixmap) = tiny_skia::Pixmap::new(w, h) else { return Ok(None) };
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    // tiny-skia keeps premultiplied alpha.
    let mut rgba = pixmap.take();
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Ok(RgbaImage::from_raw(w, h, rgba))
}

/// A PDF, ready to render pages from.
pub struct Pdf {
    pdf: hayro::hayro_syntax::Pdf,
}

impl Pdf {
    pub fn open(source: &mut Source) -> anyhow::Result<Option<Pdf>> {
        let data = source.all(MAX_DOCUMENT)?;
        match hayro::hayro_syntax::Pdf::new(data) {
            Ok(pdf) => Ok(Some(Pdf { pdf })),
            // Encrypted, or not a PDF hayro can read: no picture, not an error.
            Err(_) => Ok(None),
        }
    }

    pub fn pages(&self) -> usize {
        self.pdf.pages().len()
    }

    /// Page `index` (from 0) on white, scaled so its longer side is `max_px`.
    pub fn render(&self, index: usize, max_px: u32) -> Option<image::RgbImage> {
        use hayro::{PixmapSettings, RenderCache, RenderSettings};
        let page = self.pdf.pages().get(index)?;
        let (w, h) = page.render_dimensions();
        if !(w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
            return None;
        }
        let scale = fit(w, h, max_px);
        let cache = RenderCache::new();
        let settings = PixmapSettings { x_scale: scale, y_scale: scale, bg_color: hayro::vello_cpu::color::palette::css::WHITE };
        let pixmap = hayro::render(page, &cache, &Default::default(), &RenderSettings::default(), &settings);
        let (pw, ph) = (pixmap.width() as u32, pixmap.height() as u32);
        if pw == 0 || ph == 0 {
            return None;
        }
        // Opaque (drawn on white), so premultiplied and straight alpha agree.
        let rgb: Vec<u8> = pixmap.data_as_u8_slice().as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        image::RgbImage::from_raw(pw, ph, rgb)
    }
}

/// The largest JPEG preview a RAW camera file embeds.
pub fn raw_preview(source: &mut Source) -> anyhow::Result<Option<Vec<u8>>> {
    let head = source.head(256)?;
    let candidate = if head.starts_with(b"FUJIFILMCCD-RAW") {
        raf_preview(&head)
    } else if head.len() >= 12 && &head[4..12] == b"ftypcrx " {
        cr3_preview(source)?
    } else {
        tiff_previews(source)?.into_iter().max_by_key(|(_, len)| *len)
    };
    let Some((offset, len)) = candidate else { return Ok(None) };
    if !(4..=MAX_PREVIEW).contains(&len) || offset.checked_add(len).is_none_or(|end| end > source.len()) {
        return Ok(None);
    }
    let jpeg = source.range(offset, len as usize)?;
    Ok(jpeg.starts_with(&[0xFF, 0xD8]).then_some(jpeg))
}

/// RAF: the JPEG's offset and length, big-endian, at 84 and 88.
fn raf_preview(head: &[u8]) -> Option<(u64, u64)> {
    let at = |i: usize| head.get(i..i + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()) as u64);
    Some((at(84)?, at(88)?))
}

/// CR3 (ISO base media): the PRVW box inside Canon's preview `uuid` box holds a JPEG.
fn cr3_preview(source: &mut Source) -> anyhow::Result<Option<(u64, u64)>> {
    // The preview is near the start; look in the first few MB.
    let window = source.head(4 * 1024 * 1024)?;
    let Some(at) = window.windows(4).position(|w| w == b"PRVW") else { return Ok(None) };
    // size (4) 'PRVW' (4), 4 bytes, 2 bytes, width (2), height (2), 2 bytes, JPEG length (4), the JPEG.
    let box_start = at.saturating_sub(4);
    let jpeg_len_at = box_start + 20;
    let Some(len) = window.get(jpeg_len_at..jpeg_len_at + 4) else { return Ok(None) };
    let len = u32::from_be_bytes(len.try_into().unwrap()) as u64;
    Ok(Some(((jpeg_len_at + 4) as u64, len)))
}

/// Every (offset, length) a TIFF-based RAW file names as a JPEG: in IFD0 and
/// its chain, SubIFDs and the EXIF IFD, by JPEGInterchangeFormat or by
/// JPEG-compressed strips.
fn tiff_previews(source: &mut Source) -> anyhow::Result<Vec<(u64, u64)>> {
    let head = source.head(8)?;
    let little = match head.get(..4) {
        Some([b'I', b'I', 42, 0]) | Some([b'I', b'I', 0x52, 0x4F]) | Some([b'I', b'I', 0x55, 0]) => true,
        Some([b'M', b'M', 0, 42]) => false,
        _ => return Ok(Vec::new()),
    };
    let u16_of = |b: &[u8]| if little { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) };
    let u32_of = |b: &[u8]| if little { u32::from_le_bytes(b[..4].try_into().unwrap()) } else { u32::from_be_bytes(b[..4].try_into().unwrap()) };
    let mut queue = vec![u32_of(&head[4..8]) as u64];
    let mut seen = std::collections::HashSet::new();
    let mut found = Vec::new();
    while let Some(ifd) = queue.pop() {
        if ifd == 0 || ifd >= source.len() || !seen.insert(ifd) || seen.len() > 64 {
            continue;
        }
        let count_bytes = source.range(ifd, 2)?;
        if count_bytes.len() < 2 {
            continue;
        }
        let count = u16_of(&count_bytes).min(512) as usize;
        let entries = source.range(ifd + 2, count * 12 + 4)?;
        if entries.len() < count * 12 {
            continue;
        }
        let (mut jif, mut jif_len, mut compression, mut strip, mut strip_len) = (None, None, None, None, None);
        for e in entries[..count * 12].as_chunks::<12>().0 {
            let (tag, kind, n) = (u16_of(&e[0..2]), u16_of(&e[2..4]), u32_of(&e[4..8]));
            let value = if kind == 3 { u16_of(&e[8..10]) as u64 } else { u32_of(&e[8..12]) as u64 };
            match tag {
                0x0201 => jif = Some(value),
                0x0202 => jif_len = Some(value),
                0x0103 => compression = Some(value),
                0x0111 if n == 1 => strip = Some(value),
                0x0117 if n == 1 => strip_len = Some(value),
                // SubIFDs (one or several offsets) and the EXIF IFD.
                0x014A | 0x8769 => {
                    if n == 1 {
                        queue.push(value);
                    } else if n <= 16 {
                        let list = source.range(u32_of(&e[8..12]) as u64, n as usize * 4)?;
                        queue.extend(list.as_chunks::<4>().0.iter().map(|c| u32_of(c) as u64));
                    }
                }
                _ => {}
            }
        }
        if let (Some(o), Some(l)) = (jif, jif_len) {
            found.push((o, l));
        }
        if let (Some(6 | 7), Some(o), Some(l)) = (compression, strip, strip_len) {
            found.push((o, l));
        }
        if entries.len() >= count * 12 + 4 {
            queue.push(u32_of(&entries[count * 12..]) as u64);
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Limits, Source};

    fn source(bytes: Vec<u8>) -> Source {
        Source::new(Box::new(bytes), &Limits::default())
    }

    #[test]
    fn svgs_draw_and_never_follow_hrefs() {
        let drawing = br##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="200" height="100">
            <rect width="200" height="100" fill="#ff0000"/>
            <image href="/etc/passwd" width="10" height="10"/><image xlink:href="https://example.com/x.png" width="10" height="10"/>
        </svg>"##;
        let picture = svg(&mut source(drawing.to_vec()), 64).unwrap().unwrap();
        assert_eq!(picture.dimensions(), (64, 32));
        assert_eq!(picture.get_pixel(10, 10).0, [255, 0, 0, 255]);
        assert!(svg(&mut source(b"<svg".to_vec()), 64).is_err());
    }

    /// A one-page PDF filled with blue, built by hand (with correct xref offsets).
    fn blue_pdf() -> Vec<u8> {
        let content = b"0 0 1 rg 0 0 300 150 re f";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 150] /Contents 4 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{}\nendstream", content.len(), String::from_utf8_lossy(content)),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend(format!("{} 0 obj\n{}\nendobj\n", i + 1, body).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for o in offsets {
            pdf.extend(format!("{:010} 00000 n \n", o).as_bytes());
        }
        pdf.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n", objects.len() + 1, xref).as_bytes());
        pdf
    }

    #[test]
    fn pdf_pages_render_on_white() {
        let pdf = Pdf::open(&mut source(blue_pdf())).unwrap().unwrap();
        assert_eq!(pdf.pages(), 1);
        let page = pdf.render(0, 120).unwrap();
        assert_eq!(page.dimensions(), (120, 60));
        let [r, g, b] = page.get_pixel(60, 30).0;
        assert!(b > 200 && r < 50 && g < 50, "blue: {:?}", [r, g, b]);
        assert!(pdf.render(1, 120).is_none());
        assert!(Pdf::open(&mut source(b"not a pdf".to_vec())).unwrap().is_none());
    }

    fn jpeg() -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbImage::from_pixel(40, 20, image::Rgb([0, 200, 0])).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg).unwrap();
        out
    }

    /// A little-endian TIFF whose IFD0 names a JPEG with JPEGInterchangeFormat, as CR2/NEF/DNG do.
    fn tiff_with_preview(preview: &[u8], offset_past_end: bool) -> Vec<u8> {
        let ifd_at = 8u32;
        let jpeg_at = ifd_at + 2 + 2 * 12 + 4;
        let mut t = b"II*\0".to_vec();
        t.extend(ifd_at.to_le_bytes());
        t.extend(2u16.to_le_bytes());
        let offset = if offset_past_end { 1 << 30 } else { jpeg_at };
        for (tag, value) in [(0x0201u16, offset), (0x0202, preview.len() as u32)] {
            t.extend(tag.to_le_bytes());
            t.extend(4u16.to_le_bytes());
            t.extend(1u32.to_le_bytes());
            t.extend(value.to_le_bytes());
        }
        t.extend(0u32.to_le_bytes());
        t.extend(preview);
        t
    }

    #[test]
    fn raw_files_give_their_embedded_preview() {
        let preview = jpeg();
        assert!(is_raw("IMG_0001.CR2") && is_raw("a.nef") && !is_raw("a.tif") && !is_raw("noext"));
        assert_eq!(raw_preview(&mut source(tiff_with_preview(&preview, false))).unwrap(), Some(preview.clone()));
        assert_eq!(raw_preview(&mut source(tiff_with_preview(&preview, true))).unwrap(), None, "an offset past the end is refused");
        let mut raf = b"FUJIFILMCCD-RAW 0201".to_vec();
        raf.resize(100, 0);
        raf[84..88].copy_from_slice(&100u32.to_be_bytes());
        raf[88..92].copy_from_slice(&(preview.len() as u32).to_be_bytes());
        raf.extend(&preview);
        assert_eq!(raw_preview(&mut source(raf)).unwrap(), Some(preview));
        assert_eq!(raw_preview(&mut source(b"garbage".to_vec())).unwrap(), None);
    }
}
