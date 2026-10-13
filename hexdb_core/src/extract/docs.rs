// Document metadata: PDF (and Illustrator files, which are PDFs inside), EPS,
// Office Open XML (Word, Excel, PowerPoint), OpenDocument, EPUB, fonts,
// DICOM, archives (ZIP, TAR, gzipped TAR), plain text, and C2PA content
// credentials.

use super::{decode_text, truncate, xml, Ctx, Source};
use serde_json::{json, Map, Value};
use std::io::Read;

/// Largest PDF parsed (lopdf reads the whole file).
const MAX_PDF: u64 = 64 * 1024 * 1024;
/// Uncompressed bytes read from one archive member.
const MAX_MEMBER: u64 = 8 * 1024 * 1024;
/// Names listed from an archive.
const MAX_LISTED: usize = 1000;

// ---------------------------------------------------------------------------
// PDF and Illustrator
// ---------------------------------------------------------------------------

/// PDF dates: `D:YYYYMMDDHHmmSSOHH'mm'` (any suffix may be missing).
fn pdf_date(text: &str) -> String {
    let t = text.trim().trim_start_matches("D:");
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() < 4 {
        return text.to_string();
    }
    let part = |range: std::ops::Range<usize>, default: &str| digits.get(range).map(String::from).unwrap_or_else(|| default.to_string());
    let rest = &t[digits.len()..];
    let zone = match rest.chars().next() {
        Some('Z') => "Z".to_string(),
        Some(sign @ ('+' | '-')) => {
            let z: String = rest[1..].chars().filter(|c| c.is_ascii_digit()).collect();
            format!("{}{}:{}", sign, z.get(0..2).unwrap_or("00"), z.get(2..4).unwrap_or("00"))
        }
        _ => String::new(),
    };
    format!("{}-{}-{}T{}:{}:{}{}", part(0..4, "0000"), part(4..6, "01"), part(6..8, "01"), part(8..10, "00"), part(10..12, "00"), part(12..14, "00"), zone)
}

pub(crate) fn pdf(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if !ctx.head.starts_with(b"%PDF-") && memstart(&ctx.head, b"%PDF-").is_none() {
        return Ok(false);
    }
    let bytes = ctx.source.all(MAX_PDF)?;
    let doc = lopdf::Document::load_mem(&bytes)?;
    let mut raw = Map::new();
    raw.insert("version".into(), json!(doc.version));
    raw.insert("pages".into(), json!(doc.get_pages().len()));
    ctx.norm("pages", json!(doc.get_pages().len()));
    if doc.is_encrypted() {
        raw.insert("encrypted".into(), json!(true));
    }
    let info = doc.trailer.get(b"Info").ok().and_then(|o| match o {
        lopdf::Object::Reference(id) => doc.get_object(*id).ok(),
        other => Some(other),
    });
    if let Some(lopdf::Object::Dictionary(dict)) = info {
        for (key, value) in dict.iter() {
            let key = String::from_utf8_lossy(key).into_owned();
            let Ok(text) = lopdf::decode_text_string(value) else { continue };
            let text = text.trim_matches('\0').trim().to_string();
            if text.is_empty() {
                continue;
            }
            let text = if key.ends_with("Date") { pdf_date(&text) } else { text };
            raw.insert(key, json!(text));
        }
    }
    let text = |k: &str| raw.get(k).and_then(Value::as_str).map(String::from);
    ctx.norm_str("title", text("Title").as_deref());
    ctx.norm_str("author", text("Author").as_deref());
    ctx.norm_str("subject", text("Subject").as_deref());
    ctx.norm_str("creator_tool", text("Creator").or_else(|| text("Producer")).as_deref());
    if let Some(words) = text("Keywords") {
        ctx.keywords(words.split([',', ';']).map(String::from));
    }
    // Illustrator files are PDFs that say so in their creator.
    if ctx.ext() == "ai" || text("Creator").is_some_and(|c| c.contains("Illustrator")) {
        raw.insert("illustrator".into(), json!(true));
    }
    ctx.raw("pdf", raw);
    Ok(true)
}

fn memstart(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    xml::memfind(&bytes[..bytes.len().min(1024)], needle)
}

// ---------------------------------------------------------------------------
// EPS / PostScript (DSC comments)
// ---------------------------------------------------------------------------

pub(crate) fn eps(ctx: &mut Ctx) -> anyhow::Result<bool> {
    // DOS EPS binary header: C5 D0 D3 C6, then the PostScript offset.
    let start = if ctx.head.starts_with(&[0xC5, 0xD0, 0xD3, 0xC6]) && ctx.head.len() >= 12 {
        u32::from_le_bytes(ctx.head[4..8].try_into().unwrap()) as usize
    } else {
        0
    };
    if !ctx.head.get(start..).is_some_and(|h| h.starts_with(b"%!PS")) {
        return Ok(false);
    }
    let text = String::from_utf8_lossy(&ctx.head[start..]).into_owned();
    let mut raw = Map::new();
    for line in text.lines().take(200) {
        let Some(rest) = line.strip_prefix("%%") else { continue };
        if rest.starts_with("EndComments") {
            break;
        }
        if let Some((key, value)) = rest.split_once(':') {
            let value = value.trim().trim_matches(|c| c == '(' || c == ')').to_string();
            if !value.is_empty() && matches!(key, "Title" | "Creator" | "CreationDate" | "For" | "BoundingBox" | "HiResBoundingBox" | "Pages" | "LanguageLevel" | "DocumentData") {
                raw.insert(key.to_string(), json!(value));
            }
        }
    }
    if raw.is_empty() {
        return Ok(false);
    }
    let t = |k: &str| raw.get(k).and_then(Value::as_str).map(String::from);
    ctx.norm_str("title", t("Title").as_deref());
    ctx.norm_str("creator_tool", t("Creator").as_deref());
    ctx.norm_str("author", t("For").as_deref());
    ctx.raw("eps", raw);
    Ok(true)
}

// ---------------------------------------------------------------------------
// ZIP-based formats
// ---------------------------------------------------------------------------

fn zip_member(archive: &mut zip::ZipArchive<&mut Source>, name: &str) -> Option<Vec<u8>> {
    let file = archive.by_name(name).ok()?;
    let mut out = Vec::new();
    file.take(MAX_MEMBER).read_to_end(&mut out).ok()?;
    Some(out)
}

fn is_zip(ctx: &Ctx) -> bool {
    ctx.head.starts_with(b"PK\x03\x04")
}

/// Dublin Core and friends from an OOXML `core.xml`, ODF `meta.xml` or EPUB OPF.
fn document_properties(root: &xml::Node, raw: &mut Map<String, Value>) {
    for node in std::iter::once(root).chain(root.children.iter()).flat_map(|n| n.children.iter().chain(std::iter::once(n))) {
        if node.children.is_empty() && !node.text.trim().is_empty() && node.name.contains(':') {
            let value = node.text.trim().to_string();
            match raw.get_mut(&node.name) {
                Some(Value::Array(list)) => list.push(json!(value)),
                Some(existing) if existing.as_str() != Some(value.as_str()) => {
                    let first = existing.take();
                    *existing = json!([first, value]);
                }
                Some(_) => {}
                None => {
                    raw.insert(node.name.clone(), json!(value));
                }
            }
        }
    }
}

fn first(raw: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| match raw.get(*k)? {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => items.first().and_then(Value::as_str).map(String::from),
        _ => None,
    })
}

fn apply_dc(ctx: &mut Ctx, raw: &Map<String, Value>) {
    ctx.norm_str("title", first(raw, &["dc:title"]).as_deref());
    ctx.norm_str("author", first(raw, &["dc:creator", "meta:initial-creator"]).as_deref());
    ctx.norm_str("subject", first(raw, &["dc:subject"]).as_deref());
    ctx.norm_str("description", first(raw, &["dc:description"]).as_deref());
    ctx.norm_str("language", first(raw, &["dc:language"]).as_deref());
    ctx.norm_str("publisher", first(raw, &["dc:publisher"]).as_deref());
    ctx.norm_str("creator_tool", first(raw, &["Application", "meta:generator"]).as_deref());
    for key in ["cp:keywords", "meta:keyword"] {
        if let Some(v) = raw.get(key) {
            let words: Vec<String> = match v {
                Value::String(s) => s.split([',', ';']).map(String::from).collect(),
                Value::Array(a) => a.iter().filter_map(Value::as_str).map(String::from).collect(),
                _ => Vec::new(),
            };
            ctx.keywords(words);
        }
    }
}

/// The text of every element with one of these local names, in document order.
fn body_text(root: &xml::Node, names: &[&str], paragraph: &[&str], out: &mut String) {
    if names.contains(&root.local()) {
        out.push_str(&root.text);
    }
    for child in &root.children {
        body_text(child, names, paragraph, out);
    }
    if paragraph.contains(&root.local()) && !out.ends_with('\n') {
        out.push('\n');
    }
}

pub(crate) fn office(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if !is_zip(ctx) || !ctx.content_type.contains("openxmlformats") {
        return Ok(false);
    }
    let mut archive = zip::ZipArchive::new(&mut *ctx.source)?;
    let mut raw = Map::new();
    if let Some(root) = zip_member(&mut archive, "docProps/core.xml").and_then(|b| xml::parse(&b)) {
        document_properties(&root, &mut raw);
    }
    if let Some(root) = zip_member(&mut archive, "docProps/app.xml").and_then(|b| xml::parse(&b)) {
        for node in &root.children {
            if node.children.is_empty() && !node.text.trim().is_empty() {
                raw.insert(node.local().to_string(), json!(node.text.trim()));
            }
        }
    }
    let mut text = String::new();
    if let Some(root) = zip_member(&mut archive, "word/document.xml").and_then(|b| xml::parse(&b)) {
        body_text(&root, &["t"], &["p"], &mut text);
    }
    let names: Vec<String> = archive.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect();
    let mut slides: Vec<&String> = names.iter().filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml")).collect();
    slides.sort_by_key(|n| n.trim_start_matches("ppt/slides/slide").trim_end_matches(".xml").parse::<u32>().unwrap_or(0));
    for slide in slides.into_iter().take(500) {
        if let Some(root) = zip_member(&mut archive, slide).and_then(|b| xml::parse(&b)) {
            body_text(&root, &["t"], &["p"], &mut text);
        }
        if text.len() > ctx.limits.max_text_bytes {
            break;
        }
    }
    if let Some(root) = zip_member(&mut archive, "xl/sharedStrings.xml").and_then(|b| xml::parse(&b)) {
        body_text(&root, &["t"], &["si"], &mut text);
    }
    for (key, norm) in [("Pages", "pages"), ("Words", "words"), ("Slides", "slides")] {
        if let Some(n) = raw.get(key).and_then(Value::as_str).and_then(|s| s.parse::<u64>().ok()) {
            ctx.norm(norm, json!(n));
        }
    }
    apply_dc(ctx, &raw);
    ctx.add_text(&text);
    let found = !raw.is_empty() || !text.is_empty();
    ctx.raw("office", raw);
    Ok(found)
}

pub(crate) fn odf(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if !is_zip(ctx) || !ctx.content_type.contains("opendocument") {
        return Ok(false);
    }
    let mut archive = zip::ZipArchive::new(&mut *ctx.source)?;
    let mut raw = Map::new();
    let mut counts = Vec::new();
    if let Some(root) = zip_member(&mut archive, "meta.xml").and_then(|b| xml::parse(&b)) {
        if let Some(meta) = root.find("meta") {
            document_properties(meta, &mut raw);
            if let Some(stats) = meta.find("document-statistic") {
                for (key, value) in &stats.attrs {
                    raw.insert(key.clone(), json!(value));
                }
                for (attr, norm) in [("meta:page-count", "pages"), ("meta:word-count", "words")] {
                    if let Some(n) = stats.attr(attr).and_then(|v| v.parse::<u64>().ok()) {
                        counts.push((norm, n));
                    }
                }
            }
        }
    }
    let mut text = String::new();
    if let Some(root) = zip_member(&mut archive, "content.xml").and_then(|b| xml::parse(&b)) {
        if let Some(body) = root.find("body") {
            let mut out = String::new();
            body.all_text("\n", &mut out);
            text = out;
        }
    }
    drop(archive);
    for (norm, n) in counts {
        ctx.norm(norm, json!(n));
    }
    apply_dc(ctx, &raw);
    ctx.add_text(&text);
    let found = !raw.is_empty() || !text.is_empty();
    ctx.raw("odf", raw);
    Ok(found)
}

pub(crate) fn epub(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if !is_zip(ctx) || ctx.content_type != "application/epub+zip" {
        return Ok(false);
    }
    let mut archive = zip::ZipArchive::new(&mut *ctx.source)?;
    let Some(container) = zip_member(&mut archive, "META-INF/container.xml").and_then(|b| xml::parse(&b)) else { return Ok(false) };
    let Some(opf_path) = container.find("rootfile").and_then(|r| r.attr("full-path")).map(String::from) else { return Ok(false) };
    let Some(opf) = zip_member(&mut archive, &opf_path).and_then(|b| xml::parse(&b)) else { return Ok(false) };
    let mut raw = Map::new();
    if let Some(metadata) = opf.find("metadata") {
        document_properties(metadata, &mut raw);
    }
    apply_dc(ctx, &raw);
    if let Some(subjects) = raw.get("dc:subject") {
        let words: Vec<String> = match subjects {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).map(String::from).collect(),
            _ => Vec::new(),
        };
        ctx.keywords(words);
    }
    let found = !raw.is_empty();
    ctx.raw("epub", raw);
    Ok(found)
}

// ---------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------

pub(crate) fn font(ctx: &mut Ctx) -> anyhow::Result<bool> {
    let h = &ctx.head;
    let is_font = h.starts_with(&[0, 1, 0, 0]) || h.starts_with(b"OTTO") || h.starts_with(b"true") || h.starts_with(b"ttcf");
    if !is_font {
        return Ok(false);
    }
    let bytes = ctx.source.all(32 * 1024 * 1024)?;
    let raw = font_names(&bytes).ok_or_else(|| anyhow::anyhow!("not a readable TrueType/OpenType font"))?;
    let t = |k: &str| raw.get(k).and_then(Value::as_str).map(String::from);
    ctx.norm_str("title", t("full_name").or_else(|| t("family")).as_deref());
    ctx.norm_str("author", t("designer").or_else(|| t("manufacturer")).as_deref());
    ctx.norm_str("copyright", t("copyright").as_deref());
    ctx.raw("font", raw);
    Ok(true)
}

/// The `name` table (and glyph count) of a TrueType/OpenType font, or the
/// first font of a collection. Unicode (platform 0, or Windows 3/1 and 3/10)
/// records only, in UTF-16BE.
fn font_names(bytes: &[u8]) -> Option<Map<String, Value>> {
    let u16_at = |at: usize| bytes.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let u32_at = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let font = if bytes.starts_with(b"ttcf") { u32_at(12)? as usize } else { 0 };
    let tables = u16_at(font + 4)? as usize;
    let mut name_table = None;
    let mut glyphs = None;
    for i in 0..tables.min(512) {
        let record = font + 12 + i * 16;
        let tag = bytes.get(record..record + 4)?;
        let offset = u32_at(record + 8)? as usize;
        match tag {
            b"name" => name_table = Some(offset),
            b"maxp" => glyphs = u16_at(offset + 4),
            _ => {}
        }
    }
    let table = name_table?;
    let count = u16_at(table + 2)? as usize;
    let strings = table + u16_at(table + 4)? as usize;
    let mut raw = Map::new();
    for i in 0..count.min(4096) {
        let record = table + 6 + i * 12;
        let (platform, encoding) = (u16_at(record)?, u16_at(record + 2)?);
        let (name_id, length, offset) = (u16_at(record + 6)?, u16_at(record + 8)? as usize, u16_at(record + 10)? as usize);
        if !(platform == 0 || (platform == 3 && (encoding == 1 || encoding == 10))) {
            continue;
        }
        let label = match name_id {
            0 => "copyright",
            1 => "family",
            2 => "subfamily",
            4 => "full_name",
            5 => "version",
            6 => "postscript_name",
            8 => "manufacturer",
            9 => "designer",
            10 => "description",
            13 => "license",
            _ => continue,
        };
        if raw.contains_key(label) {
            continue;
        }
        let Some(data) = bytes.get(strings + offset..strings + offset + length) else { continue };
        let units: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        let text = String::from_utf16_lossy(&units).trim().to_string();
        if !text.is_empty() {
            raw.insert(label.into(), json!(text));
        }
    }
    if let Some(n) = glyphs {
        raw.insert("glyphs".into(), json!(n));
    }
    Some(raw)
}

// ---------------------------------------------------------------------------
// DICOM (identifying tags are left out)
// ---------------------------------------------------------------------------

const DICOM_TAGS: &[((u16, u16), &str)] = &[
    ((0x0008, 0x0020), "StudyDate"),
    ((0x0008, 0x0021), "SeriesDate"),
    ((0x0008, 0x0030), "StudyTime"),
    ((0x0008, 0x0060), "Modality"),
    ((0x0008, 0x0070), "Manufacturer"),
    ((0x0008, 0x1030), "StudyDescription"),
    ((0x0008, 0x103E), "SeriesDescription"),
    ((0x0008, 0x1090), "ManufacturerModelName"),
    ((0x0018, 0x0015), "BodyPartExamined"),
    ((0x0018, 0x0050), "SliceThickness"),
    ((0x0028, 0x0010), "Rows"),
    ((0x0028, 0x0011), "Columns"),
    ((0x0028, 0x0100), "BitsAllocated"),
];

pub(crate) fn dicom(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if ctx.head.len() < 140 || &ctx.head[128..132] != b"DICM" {
        return Ok(false);
    }
    let data = ctx.head.clone();
    let mut raw = Map::new();
    let mut i = 132;
    // Group 0002 is always explicit VR little endian; assume the rest is too
    // (the common transfer syntaxes), falling back to implicit VR on nonsense.
    while i + 8 <= data.len() {
        let group = u16::from_le_bytes([data[i], data[i + 1]]);
        let element = u16::from_le_bytes([data[i + 2], data[i + 3]]);
        if group == 0x7FE0 {
            break; // pixel data
        }
        let vr = &data[i + 4..i + 6];
        let explicit = vr.iter().all(|b| b.is_ascii_uppercase());
        let (len, value_at) = if explicit {
            if matches!(vr, b"OB" | b"OW" | b"OF" | b"SQ" | b"UT" | b"UN" | b"UC" | b"UR" | b"OD" | b"OL" | b"OV") {
                if i + 12 > data.len() {
                    break;
                }
                (u32::from_le_bytes(data[i + 8..i + 12].try_into().unwrap()), i + 12)
            } else {
                (u16::from_le_bytes([data[i + 6], data[i + 7]]) as u32, i + 8)
            }
        } else {
            (u32::from_le_bytes(data[i + 4..i + 8].try_into().unwrap()), i + 8)
        };
        if len == u32::MAX {
            break; // undefined length (sequences): stop rather than parse items
        }
        let end = value_at + len as usize;
        if end > data.len() {
            break;
        }
        if let Some((_, name)) = DICOM_TAGS.iter().find(|((g, e), _)| *g == group && *e == element) {
            let value = &data[value_at..end];
            let json = if matches!(vr, b"US") && value.len() >= 2 {
                json!(u16::from_le_bytes([value[0], value[1]]))
            } else {
                json!(decode_text(value))
            };
            raw.insert(name.to_string(), json);
        }
        i = end;
    }
    if raw.is_empty() {
        return Ok(false);
    }
    let t = |k: &str| raw.get(k).and_then(Value::as_str).map(String::from);
    ctx.norm_str("title", t("StudyDescription").or_else(|| t("SeriesDescription")).as_deref());
    ctx.norm_in("camera", "make", json!(t("Manufacturer")));
    ctx.norm_in("camera", "model", json!(t("ManufacturerModelName")));
    if let (Some(w), Some(h)) = (raw.get("Columns").cloned(), raw.get("Rows").cloned()) {
        ctx.norm("width", w);
        ctx.norm("height", h);
    }
    ctx.raw("dicom", raw);
    Ok(true)
}

// ---------------------------------------------------------------------------
// Archives
// ---------------------------------------------------------------------------

pub(crate) fn archive(ctx: &mut Ctx) -> anyhow::Result<bool> {
    let t = ctx.content_type.as_str();
    let tar_at_257 = ctx.head.get(257..262) == Some(b"ustar");
    let gz = ctx.head.starts_with(&[0x1F, 0x8B]);
    let zip_like = is_zip(ctx) && (t == "application/zip" || t == "application/java-archive" || t == "application/vnd.android.package-archive");
    if !zip_like && !tar_at_257 && !(gz && (ctx.name.ends_with(".tar.gz") || ctx.ext() == "tgz")) {
        return Ok(false);
    }
    let mut names = Vec::new();
    let mut files = 0u64;
    let mut bytes = 0u64;
    if zip_like {
        let archive = zip::ZipArchive::new(&mut *ctx.source)?;
        files = archive.len() as u64;
        names = archive.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).take(MAX_LISTED).collect();
        let comment = String::from_utf8_lossy(archive.comment()).trim().to_string();
        let mut raw = Map::new();
        raw.insert("format".into(), json!("zip"));
        if !comment.is_empty() {
            raw.insert("comment".into(), json!(comment));
        }
        raw.insert("entries".into(), json!(files));
        raw.insert("names".into(), json!(names));
        ctx.add_text(&names.join("\n"));
        ctx.raw("archive", raw);
        return Ok(true);
    }
    let reader: Box<dyn Read + '_> = if gz {
        Box::new(flate2::read::GzDecoder::new(&mut *ctx.source).take(1 << 30))
    } else {
        Box::new(&mut *ctx.source)
    };
    let mut tar = tar::Archive::new(reader);
    for entry in tar.entries()?.take(100_000) {
        let Ok(entry) = entry else { break };
        files += 1;
        bytes += entry.header().size().unwrap_or(0);
        if names.len() < MAX_LISTED {
            if let Ok(path) = entry.path() {
                names.push(path.to_string_lossy().into_owned());
            }
        }
    }
    drop(tar);
    let mut raw = Map::new();
    raw.insert("format".into(), json!(if gz { "tar.gz" } else { "tar" }));
    raw.insert("entries".into(), json!(files));
    raw.insert("unpacked_bytes".into(), json!(bytes));
    raw.insert("names".into(), json!(names));
    ctx.add_text(&names.join("\n"));
    ctx.raw("archive", raw);
    Ok(files > 0)
}

// ---------------------------------------------------------------------------
// Plain text
// ---------------------------------------------------------------------------

const TEXT_TYPES: &[&str] = &["application/json", "application/xml", "application/javascript", "application/x-sh", "application/toml", "application/yaml", "application/x-yaml", "application/sql"];

pub(crate) fn text(ctx: &mut Ctx) -> anyhow::Result<bool> {
    let t = ctx.content_type.clone();
    let t = t.as_str();
    if t == "image/svg+xml" || !(t.starts_with("text/") || TEXT_TYPES.contains(&t)) {
        return Ok(false);
    }
    let head = ctx.head.clone();
    let sample = &head[..head.len().min(ctx.limits.max_text_bytes + 4)];
    // Not text after all: NULs in the first bytes.
    if sample.iter().take(8192).filter(|&&b| b == 0).count() > 0 && !sample.starts_with(&[0xFF, 0xFE]) && !sample.starts_with(&[0xFE, 0xFF]) {
        return Ok(false);
    }
    let text = decode_text(sample);
    let lines = text.lines().count();
    let max = ctx.limits.max_text_bytes;
    ctx.add_text(truncate(&text, max));
    if ctx.source.len() <= head.len() as u64 {
        ctx.norm("lines", json!(lines));
        ctx.norm("words", json!(text.split_whitespace().count()));
    }
    // A first Markdown heading makes a good title.
    if t == "text/markdown" || ctx.ext() == "md" {
        if let Some(heading) = text.lines().find_map(|l| l.strip_prefix("# ")) {
            ctx.norm("title", json!(heading.trim()));
        }
    }
    Ok(!text.is_empty())
}

// ---------------------------------------------------------------------------
// C2PA content credentials
// ---------------------------------------------------------------------------

pub(crate) fn c2pa(ctx: &mut Ctx) -> anyhow::Result<bool> {
    let head = &ctx.head;
    // A JUMBF superbox labelled "c2pa" (JPEG APP11, PNG caBX, ISO BMFF uuid box, ...).
    let Some(at) = xml::memfind(head, b"jumb").and_then(|_| xml::memfind(head, b"c2pa")) else { return Ok(false) };
    let mut raw = Map::new();
    raw.insert("present".into(), json!(true));
    // The claim generator is a CBOR text string after its key.
    let region = &head[at..head.len().min(at + 64 * 1024)];
    for key in [&b"claim_generator"[..], b"claim_generator_info"] {
        if let Some(k) = xml::memfind(region, key) {
            let after = &region[k + key.len()..];
            if let Some(&major) = after.first() {
                if major >> 5 == 3 {
                    let (len, skip) = match major & 0x1F {
                        n @ 0..=23 => (n as usize, 1),
                        24 => (*after.get(1).unwrap_or(&0) as usize, 2),
                        25 => (after.get(1..3).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize).unwrap_or(0), 3),
                        _ => (0, 0),
                    };
                    if let Some(text) = after.get(skip..skip + len) {
                        raw.insert("claim_generator".into(), json!(String::from_utf8_lossy(text)));
                        break;
                    }
                }
            }
        }
    }
    ctx.raw("c2pa", raw);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_name_tables() {
        // Offset table with two tables (maxp, name), then the tables.
        let name_text: Vec<u8> = "Hexa Sans".encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        let mut name = Vec::new();
        name.extend(0u16.to_be_bytes()); // format
        name.extend(1u16.to_be_bytes()); // count
        name.extend(18u16.to_be_bytes()); // string storage offset
        for v in [3u16, 1, 0x409, 4, name_text.len() as u16, 0] {
            name.extend(v.to_be_bytes());
        }
        name.extend(&name_text);
        let maxp = [0u8, 0, 0x50, 0, 0x01, 0x2C]; // version 0.5, 300 glyphs
        let mut font = vec![0, 1, 0, 0];
        font.extend(2u16.to_be_bytes());
        font.extend([0u8; 6]);
        let (maxp_at, name_at) = (12 + 32, 12 + 32 + maxp.len());
        for (tag, at, len) in [(b"maxp", maxp_at, maxp.len()), (b"name", name_at, name.len())] {
            font.extend(tag);
            font.extend([0u8; 4]);
            font.extend((at as u32).to_be_bytes());
            font.extend((len as u32).to_be_bytes());
        }
        font.extend(maxp);
        font.extend(&name);
        let raw = font_names(&font).unwrap();
        assert_eq!(raw["full_name"], "Hexa Sans");
        assert_eq!(raw["glyphs"], 300);
        assert!(font_names(b"OTTO").is_none());
    }

    #[test]
    fn pdf_dates() {
        assert_eq!(pdf_date("D:20240315093000+01'00'"), "2024-03-15T09:30:00+01:00");
        assert_eq!(pdf_date("D:2024"), "2024-01-01T00:00:00");
        assert_eq!(pdf_date("D:20240315093000Z"), "2024-03-15T09:30:00Z");
        assert_eq!(pdf_date("yesterday"), "yesterday");
    }
}
