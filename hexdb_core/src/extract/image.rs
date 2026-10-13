// Image metadata: EXIF (with GPS) for JPEG, TIFF, HEIC/AVIF, PNG, WebP and
// camera RAW formats; PNG text chunks; IPTC-IIM (JPEG APP13, PSD, TIFF);
// Photoshop documents; and dimensions for every common image format.

use super::{decode_text, xml, Ctx};
use nom_exif::{EntryValue, Exif, IfdKind, MediaKind, MediaParser, MediaSource, TagOrCode};
use serde_json::{json, Map, Value};
use std::io::Read;

/// EXIF tags that hold binary data or offsets rather than information.
const SKIPPED_TAGS: &[&str] = &[
    "MakerNote",
    "JPEGInterchangeFormat",
    "JPEGInterchangeFormatLength",
    "StripOffsets",
    "StripByteCounts",
    "TileOffsets",
    "TileByteCounts",
    "ExifOffset",
    "GPSInfo",
    "InteropOffset",
    "PrintImageMatching",
    "ComponentsConfiguration",
    "UserComment",
];

const RAW_EXTENSIONS: &[&str] = &["cr2", "cr3", "nef", "arw", "dng", "raf", "orf", "rw2", "pef", "srw", "iiq", "3fr", "heic", "heif", "avif"];

fn is_image(ctx: &Ctx) -> bool {
    ctx.content_type.starts_with("image/") || RAW_EXTENSIONS.contains(&ctx.ext().as_str())
}

/// An EXIF value as JSON; `None` for binary data and long arrays.
fn exif_json(value: &EntryValue) -> Option<Value> {
    let rational = |n: Option<f64>| n.map(|f| json!((f * 1_000_000.0).round() / 1_000_000.0));
    Some(match value {
        EntryValue::Text(s) => json!(s.trim_matches(char::from(0)).trim()),
        EntryValue::URational(r) => rational(r.to_f64())?,
        EntryValue::IRational(r) => rational(r.to_f64())?,
        EntryValue::U8(n) => json!(n),
        EntryValue::U16(n) => json!(n),
        EntryValue::U32(n) => json!(n),
        EntryValue::U64(n) => json!(n),
        EntryValue::I8(n) => json!(n),
        EntryValue::I16(n) => json!(n),
        EntryValue::I32(n) => json!(n),
        EntryValue::I64(n) => json!(n),
        EntryValue::F32(n) => json!(n),
        EntryValue::F64(n) => json!(n),
        EntryValue::DateTime(t) => json!(t.to_rfc3339()),
        EntryValue::NaiveDateTime(t) => json!(t.format("%Y-%m-%dT%H:%M:%S").to_string()),
        EntryValue::Undefined(bytes) => {
            if bytes.len() <= 64 && bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ' || *b == 0) {
                json!(decode_text(bytes))
            } else {
                return None;
            }
        }
        EntryValue::URationalArray(items) if items.len() <= 16 => Value::Array(items.iter().filter_map(|r| rational(r.to_f64())).collect()),
        EntryValue::IRationalArray(items) if items.len() <= 16 => Value::Array(items.iter().filter_map(|r| rational(r.to_f64())).collect()),
        EntryValue::U16Array(items) if items.len() <= 16 => json!(items),
        EntryValue::U32Array(items) if items.len() <= 16 => json!(items),
        _ => return None,
    })
}

/// Read EXIF from a parsed directory into the context.
fn apply_exif(ctx: &mut Ctx, exif: &Exif) -> bool {
    let mut raw = Map::new();
    let mut gps = Map::new();
    for entry in exif.entries() {
        let name = match entry.tag() {
            TagOrCode::Tag(t) => t.to_string(),
            TagOrCode::Unknown(code) => {
                // IPTC-NAA in a TIFF.
                if code == 33723 {
                    if let EntryValue::Undefined(bytes) | EntryValue::U8Array(bytes) = entry.value() {
                        let iim = parse_iim(bytes);
                        apply_iptc(ctx, iim);
                    }
                }
                format!("0x{:04x}", code)
            }
        };
        if SKIPPED_TAGS.contains(&name.as_str()) || name.starts_with("0x") {
            continue;
        }
        let Some(value) = exif_json(entry.value()) else { continue };
        match entry.ifd_kind() {
            IfdKind::Gps => {
                gps.insert(name, value);
            }
            // Later TIFF directories (IFD1) describe the thumbnail.
            IfdKind::Tiff if entry.ifd().as_usize() > 0 => {}
            _ => {
                raw.entry(name).or_insert(value);
            }
        }
    }
    let text = |raw: &Map<String, Value>, k: &str| raw.get(k).and_then(Value::as_str).map(String::from);
    let number = |raw: &Map<String, Value>, k: &str| raw.get(k).filter(|v| v.is_number()).cloned();
    for (key, field) in [("Make", "make"), ("Model", "model"), ("LensModel", "lens"), ("LensMake", "lens_make"), ("BodySerialNumber", "serial")] {
        if let Some(v) = text(&raw, key) {
            ctx.norm_in("camera", field, json!(v));
        }
    }
    for (key, field) in [("FNumber", "f_number"), ("ExposureTime", "exposure_time"), ("FocalLength", "focal_length"), ("ISOSpeedRatings", "iso"), ("PhotographicSensitivity", "iso"), ("ExposureBiasValue", "exposure_bias"), ("Flash", "flash")] {
        if let Some(v) = number(&raw, key) {
            ctx.norm_in("exposure", field, v);
        }
    }
    if let Some(v) = text(&raw, "DateTimeOriginal").or_else(|| text(&raw, "CreateDate")).or_else(|| text(&raw, "ModifyDate")) {
        ctx.norm("taken_at", json!(v));
    }
    ctx.norm_str("description", text(&raw, "ImageDescription").as_deref());
    ctx.norm_str("author", text(&raw, "Artist").as_deref());
    ctx.norm_str("copyright", text(&raw, "Copyright").as_deref());
    ctx.norm_str("creator_tool", text(&raw, "Software").as_deref());
    if let Some(v) = number(&raw, "Orientation") {
        ctx.norm("orientation", v);
    }
    for (keys, norm) in [(["ExifImageWidth", "PixelXDimension", "ImageWidth"], "width"), (["ExifImageHeight", "PixelYDimension", "ImageHeight"], "height")] {
        if let Some(v) = keys.iter().find_map(|k| number(&raw, k)) {
            ctx.norm(norm, v);
        }
    }
    if let Some(info) = exif.gps_info() {
        if let (Some(lat), Some(lon)) = (info.latitude_decimal(), info.longitude_decimal()) {
            if lat.is_finite() && lon.is_finite() && (lat != 0.0 || lon != 0.0) {
                let mut point = json!({ "lat": (lat * 1e7).round() / 1e7, "lon": (lon * 1e7).round() / 1e7 });
                if let Some(alt) = info.altitude.meters() {
                    point["alt"] = json!((alt * 100.0).round() / 100.0);
                }
                ctx.norm("gps", point);
            }
        }
    }
    let found = !raw.is_empty() || !gps.is_empty();
    if !gps.is_empty() {
        raw.insert("GPS".into(), Value::Object(gps));
    }
    ctx.raw("exif", raw);
    found
}

pub(crate) fn exif(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if !is_image(ctx) {
        return Ok(false);
    }
    let mut parser = MediaParser::new();
    let Ok(source) = MediaSource::seekable(&mut *ctx.source) else { return Ok(false) };
    if source.kind() != MediaKind::Image {
        return Ok(false);
    }
    let exif: Exif = match parser.parse_exif(source) {
        Ok(iter) => iter.into(),
        // No EXIF in this image: not an error.
        Err(_) => return Ok(false),
    };
    Ok(apply_exif(ctx, &exif))
}

/// EXIF from a bare TIFF stream (as embedded in PSD files).
fn exif_from_tiff(ctx: &mut Ctx, tiff: Vec<u8>) -> bool {
    let mut parser = MediaParser::new();
    let Ok(source) = MediaSource::from_memory(tiff) else { return false };
    match parser.parse_exif(source) {
        Ok(iter) => {
            let exif: Exif = iter.into();
            apply_exif(ctx, &exif)
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// PNG text chunks
// ---------------------------------------------------------------------------

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

pub(crate) fn png_text(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if !ctx.head.starts_with(PNG_SIGNATURE) {
        return Ok(false);
    }
    let mut raw = Map::new();
    let mut offset = 8u64;
    let len = ctx.source.len();
    for _ in 0..10_000 {
        if offset + 8 > len {
            break;
        }
        let header = ctx.source.range(offset, 8)?;
        let size = u32::from_be_bytes(header[..4].try_into().unwrap()) as u64;
        let kind = &header[4..8];
        if kind == b"IEND" {
            break;
        }
        if matches!(kind, b"tEXt" | b"zTXt" | b"iTXt" | b"iCCP") && size <= 4 * 1024 * 1024 {
            let data = ctx.source.range(offset + 8, size as usize)?;
            if let Some((key, value)) = png_chunk_text(kind, &data) {
                if key == "XML:com.adobe.xmp" {
                    if let Some(map) = xml::xmp_packet(value.as_bytes()) {
                        ctx.raw("xmp", map);
                    }
                } else if kind == b"iCCP" {
                    raw.insert("ICC Profile".into(), json!(key));
                } else {
                    raw.insert(key, json!(value));
                }
            }
        }
        offset += 12 + size;
    }
    let text = |k: &str| raw.get(k).and_then(Value::as_str).map(String::from);
    ctx.norm_str("title", text("Title").as_deref());
    ctx.norm_str("author", text("Author").as_deref());
    ctx.norm_str("description", text("Description").or_else(|| text("Comment")).as_deref());
    ctx.norm_str("copyright", text("Copyright").as_deref());
    ctx.norm_str("creator_tool", text("Software").as_deref());
    let found = !raw.is_empty();
    ctx.raw("png", raw);
    Ok(found)
}

fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data).take(4 * 1024 * 1024).read_to_end(&mut out).ok()?;
    Some(out)
}

fn png_chunk_text(kind: &[u8], data: &[u8]) -> Option<(String, String)> {
    let nul = data.iter().position(|&b| b == 0)?;
    let key: String = data[..nul].iter().map(|&b| b as char).collect();
    let rest = &data[nul + 1..];
    let value = match kind {
        b"tEXt" => rest.iter().map(|&b| b as char).collect(),
        // compression method, then zlib data
        b"zTXt" => inflate(rest.get(1..)?)?.iter().map(|&b| b as char).collect(),
        b"iCCP" => String::new(),
        b"iTXt" => {
            let (compressed, rest) = (*rest.first()? == 1, rest.get(2..)?);
            let lang_end = rest.iter().position(|&b| b == 0)?;
            let rest = &rest[lang_end + 1..];
            let translated_end = rest.iter().position(|&b| b == 0)?;
            let text = &rest[translated_end + 1..];
            let bytes = if compressed { inflate(text)? } else { text.to_vec() };
            String::from_utf8_lossy(&bytes).into_owned()
        }
        _ => return None,
    };
    Some((key, value))
}

// ---------------------------------------------------------------------------
// IPTC-IIM
// ---------------------------------------------------------------------------

/// IIM application record (2) datasets and their names.
const IIM_NAMES: &[(u8, &str)] = &[
    (5, "ObjectName"),
    (7, "EditStatus"),
    (10, "Urgency"),
    (15, "Category"),
    (20, "SupplementalCategories"),
    (25, "Keywords"),
    (40, "SpecialInstructions"),
    (55, "DateCreated"),
    (60, "TimeCreated"),
    (65, "OriginatingProgram"),
    (80, "By-line"),
    (85, "By-lineTitle"),
    (90, "City"),
    (92, "Sub-location"),
    (95, "Province-State"),
    (100, "Country-PrimaryLocationCode"),
    (101, "Country-PrimaryLocationName"),
    (103, "OriginalTransmissionReference"),
    (105, "Headline"),
    (110, "Credit"),
    (115, "Source"),
    (116, "CopyrightNotice"),
    (118, "Contact"),
    (120, "Caption-Abstract"),
    (122, "Writer-Editor"),
];
const IIM_REPEATABLE: &[u8] = &[20, 25, 80, 85, 118, 122];

/// Parse IIM datasets: `0x1C | record | dataset | length (2) | data`.
pub(crate) fn parse_iim(bytes: &[u8]) -> Map<String, Value> {
    let mut map = Map::new();
    let mut i = 0;
    while i + 5 <= bytes.len() && bytes[i] == 0x1C {
        let (record, dataset) = (bytes[i + 1], bytes[i + 2]);
        let len = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as usize;
        if len & 0x8000 != 0 {
            break; // extended length: not used for text
        }
        let start = i + 5;
        let Some(data) = bytes.get(start..start + len) else { break };
        i = start + len;
        if record != 2 {
            continue;
        }
        let Some((_, name)) = IIM_NAMES.iter().find(|(d, _)| *d == dataset) else { continue };
        let value = decode_text(data);
        if value.is_empty() {
            continue;
        }
        if IIM_REPEATABLE.contains(&dataset) {
            let list = map.entry(name.to_string()).or_insert_with(|| json!([]));
            if let Value::Array(items) = list {
                items.push(json!(value));
            }
        } else {
            map.insert(name.to_string(), json!(value));
        }
    }
    map
}

fn apply_iptc(ctx: &mut Ctx, map: Map<String, Value>) -> bool {
    if map.is_empty() {
        return false;
    }
    let text = |k: &str| map.get(k).and_then(|v| v.as_str().map(String::from).or_else(|| v.as_array().and_then(|a| a.first()).and_then(Value::as_str).map(String::from)));
    ctx.norm_str("title", text("ObjectName").or_else(|| text("Headline")).as_deref());
    ctx.norm_str("description", text("Caption-Abstract").as_deref());
    ctx.norm_str("author", text("By-line").as_deref());
    ctx.norm_str("copyright", text("CopyrightNotice").as_deref());
    for (key, field) in [("City", "city"), ("Sub-location", "sublocation"), ("Province-State", "state"), ("Country-PrimaryLocationName", "country")] {
        if let Some(v) = text(key) {
            ctx.norm_in("location", field, json!(v));
        }
    }
    if let Some(Value::Array(words)) = map.get("Keywords") {
        let words: Vec<String> = words.iter().filter_map(Value::as_str).map(String::from).collect();
        ctx.keywords(words);
    }
    if let Some(date) = text("DateCreated").filter(|d| d.len() == 8) {
        ctx.norm("taken_at", json!(format!("{}-{}-{}", &date[..4], &date[4..6], &date[6..8])));
    }
    ctx.raw("iptc", map);
    true
}

/// Photoshop image resources: `8BIM | id (2) | Pascal name (padded) | size (4) | data (padded)`.
pub(crate) fn photoshop_resources(bytes: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 12 <= bytes.len() && &bytes[i..i + 4] == b"8BIM" {
        let id = u16::from_be_bytes([bytes[i + 4], bytes[i + 5]]);
        let name_len = bytes[i + 6] as usize;
        let mut j = i + 7 + name_len;
        if (name_len + 1) % 2 == 1 {
            j += 1;
        }
        let Some(size_bytes) = bytes.get(j..j + 4) else { break };
        let size = u32::from_be_bytes(size_bytes.try_into().unwrap()) as usize;
        let start = j + 4;
        let Some(data) = bytes.get(start..start + size) else { break };
        out.push((id, data));
        i = start + size + (size % 2);
    }
    out
}

pub(crate) fn iptc(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if !ctx.head.starts_with(&[0xFF, 0xD8]) {
        return Ok(false);
    }
    // Walk JPEG segments to the first scan, looking for APP13 (Photoshop IRB).
    let head = ctx.head.clone();
    let mut i = 2;
    while i + 4 <= head.len() {
        if head[i] != 0xFF {
            break;
        }
        let marker = head[i + 1];
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        let len = u16::from_be_bytes([head[i + 2], head[i + 3]]) as usize;
        let segment = head.get(i + 4..i + 2 + len).unwrap_or(&[]);
        if marker == 0xED && segment.starts_with(b"Photoshop 3.0\0") {
            for (id, data) in photoshop_resources(&segment[14..]) {
                if id == 0x0404 {
                    return Ok(apply_iptc(ctx, parse_iim(data)));
                }
            }
        }
        i += 2 + len;
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Photoshop documents
// ---------------------------------------------------------------------------

const COLOR_MODES: &[&str] = &["Bitmap", "Grayscale", "Indexed", "RGB", "CMYK", "", "", "Multichannel", "Duotone", "Lab"];

pub(crate) fn psd(ctx: &mut Ctx) -> anyhow::Result<bool> {
    let h = ctx.head.get(..30).map(<[u8]>::to_vec).unwrap_or_default();
    if h.len() < 30 || &h[..4] != b"8BPS" {
        return Ok(false);
    }
    let version = u16::from_be_bytes([h[4], h[5]]);
    let channels = u16::from_be_bytes([h[12], h[13]]);
    let height = u32::from_be_bytes(h[14..18].try_into().unwrap());
    let width = u32::from_be_bytes(h[18..22].try_into().unwrap());
    let depth = u16::from_be_bytes([h[22], h[23]]);
    let mode = u16::from_be_bytes([h[24], h[25]]) as usize;
    let mut raw = Map::new();
    raw.insert("format".into(), json!(if version == 2 { "PSB" } else { "PSD" }));
    raw.insert("channels".into(), json!(channels));
    raw.insert("depth".into(), json!(depth));
    let mode_name = COLOR_MODES.get(mode).copied().unwrap_or("");
    if !mode_name.is_empty() {
        raw.insert("color_mode".into(), json!(mode_name));
        ctx.norm("color_mode", json!(mode_name));
    }
    ctx.norm("width", json!(width));
    ctx.norm("height", json!(height));

    let color_len = u32::from_be_bytes(h[26..30].try_into().unwrap()) as u64;
    let resources_at = 30 + color_len;
    let resources_len = u32::from_be_bytes(ctx.source.range(resources_at, 4)?.try_into().map_err(|_| anyhow::anyhow!("truncated"))?) as u64;
    if resources_len <= 32 * 1024 * 1024 {
        let resources = ctx.source.range(resources_at + 4, resources_len as usize)?;
        for (id, data) in photoshop_resources(&resources) {
            match id {
                0x0404 => {
                    apply_iptc(ctx, parse_iim(data));
                }
                0x0422 => {
                    exif_from_tiff(ctx, data.to_vec());
                }
                0x0424 => {
                    if let Some(map) = xml::xmp_packet(data) {
                        ctx.raw("xmp", map);
                    }
                }
                0x0421 if data.len() > 9 => {
                    // Version info: version (4), has real merged data (1), writer name (Unicode string).
                    let count = u32::from_be_bytes(data[5..9].try_into().unwrap()) as usize;
                    if let Some(units) = data.get(9..9 + count * 2) {
                        let writer: Vec<u16> = units.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                        let writer = String::from_utf16_lossy(&writer);
                        raw.insert("writer".into(), json!(writer));
                        ctx.norm("creator_tool", json!(writer));
                    }
                }
                _ => {}
            }
        }
    }
    // Layer and mask section: its length, then the layer info length, then the layer count.
    let layers_at = resources_at + 4 + resources_len;
    let wide = version == 2;
    let skip = if wide { 16 } else { 8 };
    if let Ok(count) = ctx.source.range(layers_at + skip, 2) {
        if count.len() == 2 {
            let layers = i16::from_be_bytes([count[0], count[1]]).unsigned_abs();
            raw.insert("layers".into(), json!(layers));
            ctx.norm("layers", json!(layers));
        }
    }
    ctx.raw("psd", raw);
    Ok(true)
}

// ---------------------------------------------------------------------------
// Dimensions
// ---------------------------------------------------------------------------

pub(crate) fn dimensions(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if !is_image(ctx) && !ctx.content_type.contains("photoshop") {
        return Ok(false);
    }
    let Ok(kind) = imagesize::image_type(&ctx.head) else { return Ok(false) };
    let size = match imagesize::blob_size(&ctx.head) {
        Ok(size) => size,
        Err(_) => imagesize::reader_size(std::io::BufReader::new(&mut *ctx.source))?,
    };
    ctx.norm("width", json!(size.width));
    ctx.norm("height", json!(size.height));
    let mut raw = Map::new();
    raw.insert("format".into(), json!(format!("{:?}", kind)));
    raw.insert("width".into(), json!(size.width));
    raw.insert("height".into(), json!(size.height));
    ctx.raw("image", raw);
    Ok(true)
}
