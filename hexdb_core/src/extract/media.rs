// Audio and video metadata: every tag family lofty reads (ID3v1, ID3v2,
// APEv1/v2, Vorbis comments, MP4 ilst, RIFF INFO, AIFF text), Broadcast WAV
// `bext` and iXML chunks, ASF/WMA, QuickTime/MP4/Matroska track metadata, and
// Matroska Tags.

use super::{decode_text, xml, Ctx};
use lofty::config::ParseOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{ItemValue, TagType};
use nom_exif::{EntryValue, MediaKind, MediaParser, MediaSource, TrackInfoTag};
use serde_json::{json, Map, Value};

fn family(tag_type: TagType) -> &'static str {
    match tag_type {
        TagType::Id3v1 => "id3v1",
        TagType::Id3v2 => "id3v2",
        TagType::Ape => "ape",
        TagType::VorbisComments => "vorbis",
        TagType::Mp4Ilst => "mp4",
        TagType::RiffInfo => "riff_info",
        TagType::AiffText => "aiff",
        _ => "tags",
    }
}

/// Normalized fields for lofty's generic item keys.
const NORMALIZED: &[(&str, &str)] = &[
    ("TrackTitle", "title"),
    ("TrackArtist", "artist"),
    ("AlbumTitle", "album"),
    ("AlbumArtist", "album_artist"),
    ("Composer", "composer"),
    ("Genre", "genre"),
    ("RecordingDate", "year"),
    ("Year", "year"),
    ("TrackNumber", "track"),
    ("DiscNumber", "disc"),
    ("Comment", "comment"),
    ("CopyrightMessage", "copyright"),
    ("Label", "label"),
    ("Isrc", "isrc"),
    ("Bpm", "bpm"),
    ("IntegerBpm", "bpm"),
    ("Lyricist", "lyricist"),
    ("Conductor", "conductor"),
    ("EncoderSoftware", "creator_tool"),
    ("Description", "description"),
];

pub(crate) fn audio(ctx: &mut Ctx) -> anyhow::Result<bool> {
    let video = ctx.content_type.starts_with("video/");
    if !ctx.content_type.starts_with("audio/") && !video && ctx.content_type != "application/ogg" {
        return Ok(false);
    }
    let mut options = ParseOptions::new();
    options.read_cover_art(false);
    let probe = Probe::new(std::io::BufReader::new(&mut *ctx.source)).options(options).guess_file_type()?;
    if probe.file_type().is_none() {
        return Ok(false);
    }
    let file = match probe.read() {
        Ok(file) => file,
        // A video lofty doesn't understand: the track extractor covers it.
        Err(_) if video => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let properties = file.properties();
    let duration = properties.duration().as_millis() as u64;
    if duration > 0 {
        ctx.norm("duration_ms", json!(duration));
    }
    if let Some(v) = properties.audio_bitrate() {
        ctx.norm("bitrate_kbps", json!(v));
    }
    if let Some(v) = properties.sample_rate() {
        ctx.norm("sample_rate", json!(v));
    }
    if let Some(v) = properties.channels() {
        ctx.norm("channels", json!(v));
    }
    if let Some(v) = properties.bit_depth() {
        ctx.norm("bit_depth", json!(v));
    }
    let mut found = duration > 0;
    for tag in file.tags() {
        let tag_type = tag.tag_type();
        let mut raw = Map::new();
        for item in tag.items() {
            let text = match item.value() {
                ItemValue::Text(t) | ItemValue::Locator(t) => t.clone(),
                ItemValue::Binary(_) => continue,
            };
            let generic = format!("{:?}", item.key());
            let name = item.key().map_key(tag_type).map(String::from).unwrap_or_else(|| generic.clone());
            if let Some((_, norm)) = NORMALIZED.iter().find(|(k, _)| *k == generic) {
                ctx.norm(norm, json!(text));
            }
            match raw.get_mut(&name) {
                Some(Value::Array(list)) => list.push(json!(text)),
                Some(existing) => {
                    let first = existing.take();
                    *existing = json!([first, text]);
                }
                None => {
                    raw.insert(name, json!(text));
                }
            }
        }
        if !tag.pictures().is_empty() {
            raw.insert("pictures".into(), json!(tag.pictures().len()));
        }
        if !raw.is_empty() {
            found = true;
            ctx.raw(family(tag_type), raw);
        }
    }
    Ok(found)
}

// ---------------------------------------------------------------------------
// RIFF chunks lofty doesn't read: Broadcast WAV `bext`, and iXML
// ---------------------------------------------------------------------------

fn fixed_text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    decode_text(&bytes[..end])
}

/// The Broadcast Wave Format extension chunk (EBU Tech 3285).
fn bext(data: &[u8]) -> Map<String, Value> {
    let mut map = Map::new();
    if data.len() < 348 {
        return map;
    }
    for (name, range) in [
        ("description", 0..256),
        ("originator", 256..288),
        ("originator_reference", 288..320),
        ("origination_date", 320..330),
        ("origination_time", 330..338),
    ] {
        let text = fixed_text(&data[range]);
        if !text.is_empty() {
            map.insert(name.into(), json!(text));
        }
    }
    let low = u32::from_le_bytes(data[338..342].try_into().unwrap()) as u64;
    let high = u32::from_le_bytes(data[342..346].try_into().unwrap()) as u64;
    map.insert("time_reference".into(), json!((high << 32) | low));
    let version = u16::from_le_bytes([data[346], data[347]]);
    map.insert("version".into(), json!(version));
    if data.len() >= 412 && data[348..412].iter().any(|&b| b != 0) {
        map.insert("umid".into(), json!(hex::encode(&data[348..412])));
    }
    if version >= 2 && data.len() >= 422 {
        let loudness = |i: usize| i16::from_le_bytes([data[i], data[i + 1]]) as f64 / 100.0;
        for (name, at) in [
            ("loudness_value", 412),
            ("loudness_range", 414),
            ("max_true_peak_level", 416),
            ("max_momentary_loudness", 418),
            ("max_short_term_loudness", 420),
        ] {
            map.insert(name.into(), json!(loudness(at)));
        }
    }
    if data.len() > 602 {
        let history = fixed_text(&data[602..]);
        if !history.is_empty() {
            map.insert("coding_history".into(), json!(history));
        }
    }
    map
}

pub(crate) fn riff_chunks(ctx: &mut Ctx) -> anyhow::Result<bool> {
    let h = &ctx.head;
    if h.len() < 12 || !matches!(&h[..4], b"RIFF" | b"RF64" | b"BW64") || &h[8..12] != b"WAVE" {
        return Ok(false);
    }
    let len = ctx.source.len();
    let mut offset = 12u64;
    let mut found = false;
    for _ in 0..10_000 {
        if offset + 8 > len {
            break;
        }
        let header = ctx.source.range(offset, 8)?;
        let id = [header[0], header[1], header[2], header[3]];
        let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as u64;
        // An RF64 "data" chunk gives 0xFFFFFFFF here; nothing we want follows reliably.
        if size == u32::MAX as u64 {
            break;
        }
        match &id {
            b"bext" if size <= 1024 * 1024 => {
                let map = bext(&ctx.source.range(offset + 8, size as usize)?);
                if !map.is_empty() {
                    let text = |k: &str| map.get(k).and_then(Value::as_str).map(String::from);
                    ctx.norm_str("description", text("description").as_deref());
                    if let (Some(date), Some(time)) = (text("origination_date"), text("origination_time")) {
                        ctx.norm("recorded_at", json!(format!("{}T{}", date, time.replace([':', '-', '.'], ":"))));
                    }
                    ctx.raw("bext", map);
                    found = true;
                }
            }
            b"iXML" if size <= 4 * 1024 * 1024 => {
                let map = xml::ixml(&ctx.source.range(offset + 8, size as usize)?);
                if !map.is_empty() {
                    ctx.norm_str("description", map.get("note").and_then(Value::as_str));
                    ctx.raw("ixml", map);
                    found = true;
                }
            }
            _ => {}
        }
        offset += 8 + size + (size % 2);
    }
    Ok(found)
}

// ---------------------------------------------------------------------------
// ASF (WMA, WMV)
// ---------------------------------------------------------------------------

const ASF_HEADER: [u8; 16] = [0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C];
const ASF_CONTENT: [u8; 16] = [0x33, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C];
const ASF_EXTENDED: [u8; 16] = [0x40, 0xA4, 0xD0, 0xD2, 0x07, 0xE3, 0xD2, 0x11, 0x97, 0xF0, 0x00, 0xA0, 0xC9, 0x5E, 0xA8, 0x50];
const ASF_FILE_PROPERTIES: [u8; 16] = [0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65];

fn utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units).trim_end_matches('\0').trim().to_string()
}

pub(crate) fn asf(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if ctx.head.len() < 30 || ctx.head[..16] != ASF_HEADER {
        return Ok(false);
    }
    let header_size = u64::from_le_bytes(ctx.head[16..24].try_into().unwrap()).min(16 * 1024 * 1024);
    let header = if header_size as usize <= ctx.head.len() { ctx.head[..header_size as usize].to_vec() } else { ctx.source.range(0, header_size as usize)? };
    let mut raw = Map::new();
    let mut i = 30;
    while i + 24 <= header.len() {
        let guid = &header[i..i + 16];
        let size = u64::from_le_bytes(header[i + 16..i + 24].try_into().unwrap()) as usize;
        if size < 24 || i + size > header.len() {
            break;
        }
        let body = &header[i + 24..i + size];
        if guid == ASF_CONTENT && body.len() >= 10 {
            let lengths: Vec<usize> = (0..5).map(|k| u16::from_le_bytes([body[k * 2], body[k * 2 + 1]]) as usize).collect();
            let mut at = 10;
            for (name, len) in ["Title", "Author", "Copyright", "Description", "Rating"].iter().zip(lengths) {
                if let Some(bytes) = body.get(at..at + len) {
                    let text = utf16le(bytes);
                    if !text.is_empty() {
                        raw.insert(name.to_string(), json!(text));
                    }
                }
                at += len;
            }
        } else if guid == ASF_EXTENDED && body.len() >= 2 {
            let count = u16::from_le_bytes([body[0], body[1]]);
            let mut at = 2;
            for _ in 0..count {
                let Some(n) = body.get(at..at + 2) else { break };
                let name_len = u16::from_le_bytes([n[0], n[1]]) as usize;
                let Some(name) = body.get(at + 2..at + 2 + name_len) else { break };
                let name = utf16le(name);
                let at_type = at + 2 + name_len;
                let Some(t) = body.get(at_type..at_type + 4) else { break };
                let (kind, value_len) = (u16::from_le_bytes([t[0], t[1]]), u16::from_le_bytes([t[2], t[3]]) as usize);
                let Some(value) = body.get(at_type + 4..at_type + 4 + value_len) else { break };
                let value = match kind {
                    0 => Some(json!(utf16le(value))),
                    2 if value.len() >= 4 => Some(json!(u32::from_le_bytes(value[..4].try_into().unwrap()) != 0)),
                    3 if value.len() >= 4 => Some(json!(u32::from_le_bytes(value[..4].try_into().unwrap()))),
                    4 if value.len() >= 8 => Some(json!(u64::from_le_bytes(value[..8].try_into().unwrap()))),
                    5 if value.len() >= 2 => Some(json!(u16::from_le_bytes([value[0], value[1]]))),
                    _ => None,
                };
                if let Some(value) = value {
                    if !name.is_empty() {
                        raw.insert(name, value);
                    }
                }
                at = at_type + 4 + value_len;
            }
        } else if guid == ASF_FILE_PROPERTIES && body.len() >= 80 {
            let play = u64::from_le_bytes(body[40..48].try_into().unwrap());
            let preroll = u64::from_le_bytes(body[56..64].try_into().unwrap());
            let ms = (play / 10_000).saturating_sub(preroll);
            if ms > 0 {
                ctx.norm("duration_ms", json!(ms));
            }
        }
        i += size;
    }
    if raw.is_empty() {
        return Ok(false);
    }
    let text = |k: &str| raw.get(k).and_then(|v| v.as_str().map(String::from).or_else(|| v.as_u64().map(|n| n.to_string())));
    ctx.norm_str("title", text("Title").as_deref());
    ctx.norm_str("artist", text("Author").as_deref());
    ctx.norm_str("copyright", text("Copyright").as_deref());
    ctx.norm_str("description", text("Description").as_deref());
    for (key, norm) in [("WM/AlbumTitle", "album"), ("WM/AlbumArtist", "album_artist"), ("WM/Genre", "genre"), ("WM/Year", "year"), ("WM/TrackNumber", "track"), ("WM/Composer", "composer")] {
        ctx.norm_str(norm, text(key).as_deref());
    }
    ctx.raw("asf", raw);
    Ok(true)
}

// ---------------------------------------------------------------------------
// QuickTime / MP4 / Matroska tracks
// ---------------------------------------------------------------------------

pub(crate) fn track(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if !ctx.content_type.starts_with("video/") && !ctx.content_type.starts_with("audio/") {
        return Ok(false);
    }
    let Ok(source) = MediaSource::seekable(&mut *ctx.source) else { return Ok(false) };
    if source.kind() != MediaKind::Track {
        return Ok(false);
    }
    let info = match MediaParser::new().parse_track(source) {
        Ok(info) => info,
        Err(_) => return Ok(false),
    };
    let mut raw = Map::new();
    for (tag, value) in info.iter() {
        let json = match value {
            EntryValue::Text(s) => json!(s),
            EntryValue::U32(n) => json!(n),
            EntryValue::U64(n) => json!(n),
            EntryValue::DateTime(t) => json!(t.to_rfc3339()),
            EntryValue::NaiveDateTime(t) => json!(t.format("%Y-%m-%dT%H:%M:%S").to_string()),
            other => json!(other.to_string()),
        };
        let norm = match tag {
            TrackInfoTag::DurationMs => Some("duration_ms"),
            TrackInfoTag::Width => Some("width"),
            TrackInfoTag::Height => Some("height"),
            TrackInfoTag::CreateDate => Some("taken_at"),
            TrackInfoTag::Author => Some("author"),
            TrackInfoTag::Software => Some("creator_tool"),
            _ => None,
        };
        match tag {
            TrackInfoTag::Make => ctx.norm_in("camera", "make", json.clone()),
            TrackInfoTag::Model => ctx.norm_in("camera", "model", json.clone()),
            _ => {}
        }
        if let Some(norm) = norm {
            ctx.norm(norm, json.clone());
        }
        raw.insert(format!("{:?}", tag), json);
    }
    if let Some(gps) = info.gps_info() {
        if let (Some(lat), Some(lon)) = (gps.latitude_decimal(), gps.longitude_decimal()) {
            if lat.is_finite() && lon.is_finite() && (lat != 0.0 || lon != 0.0) {
                let mut point = json!({ "lat": (lat * 1e7).round() / 1e7, "lon": (lon * 1e7).round() / 1e7 });
                if let Some(alt) = gps.altitude.meters() {
                    point["alt"] = json!((alt * 100.0).round() / 100.0);
                }
                ctx.norm("gps", point);
            }
        }
    }
    let found = !raw.is_empty();
    ctx.raw("quicktime", raw);
    Ok(found)
}

// ---------------------------------------------------------------------------
// Matroska / WebM (EBML)
// ---------------------------------------------------------------------------

const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const TAGS: u32 = 0x1254_C367;

/// An EBML element ID (marker bits kept) and its length in bytes.
fn ebml_id(b: &[u8]) -> Option<(u32, usize)> {
    let first = *b.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 || b.len() < len {
        return None;
    }
    Some((b[..len].iter().fold(0u32, |acc, &x| (acc << 8) | x as u32), len))
}

/// An EBML size (marker removed; `None` = unknown size) and its length in bytes.
fn ebml_size(b: &[u8]) -> Option<(Option<u64>, usize)> {
    let first = *b.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || b.len() < len {
        return None;
    }
    let mask = if len == 8 { 0 } else { 0xFFu8 >> len };
    let mut value = (first & mask) as u64;
    for &x in &b[1..len] {
        value = (value << 8) | x as u64;
    }
    let unknown = value == (1u64 << (7 * len)) - 1;
    Some((if unknown { None } else { Some(value) }, len))
}

/// Child elements of a master element's data: (ID, data).
fn ebml_children(mut data: &[u8]) -> Vec<(u32, &[u8])> {
    let mut out = Vec::new();
    while let Some((id, id_len)) = ebml_id(data) {
        let Some((Some(size), size_len)) = ebml_size(&data[id_len..]) else { break };
        let start = id_len + size_len;
        let end = start + size as usize;
        if end > data.len() {
            break;
        }
        out.push((id, &data[start..end]));
        data = &data[end..];
    }
    out
}

fn ebml_uint(b: &[u8]) -> u64 {
    b.iter().take(8).fold(0, |acc, &x| (acc << 8) | x as u64)
}

fn ebml_float(b: &[u8]) -> Option<f64> {
    match b.len() {
        4 => Some(f32::from_be_bytes(b.try_into().ok()?) as f64),
        8 => Some(f64::from_be_bytes(b.try_into().ok()?)),
        _ => None,
    }
}

fn ebml_text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim_end_matches('\0').trim().to_string()
}

fn simple_tags(data: &[u8], out: &mut Map<String, Value>) {
    let mut name = String::new();
    let mut value = String::new();
    for (id, child) in ebml_children(data) {
        match id {
            0x45A3 => name = ebml_text(child),
            0x4487 => value = ebml_text(child),
            0x67C8 => simple_tags(child, out),
            _ => {}
        }
    }
    if !name.is_empty() && !value.is_empty() {
        out.entry(name).or_insert(json!(value));
    }
}

pub(crate) fn matroska(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if !ctx.head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Ok(false);
    }
    let len = ctx.source.len();
    // Skip the EBML header, then find the Segment.
    let (_, id_len) = ebml_id(&ctx.head).ok_or_else(|| anyhow::anyhow!("bad EBML header"))?;
    let (Some(header_size), size_len) = ebml_size(&ctx.head[id_len..]).ok_or_else(|| anyhow::anyhow!("bad EBML header"))? else {
        return Ok(false);
    };
    let mut at = (id_len + size_len) as u64 + header_size;
    let segment = ctx.source.range(at, 12)?;
    let Some((SEGMENT, seg_id_len)) = ebml_id(&segment) else { return Ok(false) };
    let Some((_, seg_size_len)) = ebml_size(&segment[seg_id_len..]) else { return Ok(false) };
    let segment_data = at + (seg_id_len + seg_size_len) as u64;
    at = segment_data;

    // Top-level elements we want, by offset: the SeekHead says where they are,
    // and walking the top level (skipping clusters by their sizes) finds the
    // rest. A cluster of unknown size (a live recording) ends the walk.
    let mut wanted: Vec<(u32, u64)> = Vec::new();
    for _ in 0..200_000 {
        if at + 2 > len {
            break;
        }
        let head = ctx.source.range(at, 12)?;
        let Some((id, id_len)) = ebml_id(&head) else { break };
        let Some((size, size_len)) = ebml_size(&head[id_len..]) else { break };
        let data_at = at + (id_len + size_len) as u64;
        if id == SEEK_HEAD {
            if let Some(size) = size.filter(|s| *s <= 1024 * 1024) {
                let data = ctx.source.range(data_at, size as usize)?;
                for (_, seek) in ebml_children(&data).into_iter().filter(|(id, _)| *id == 0x4DBB) {
                    let mut target = 0u32;
                    let mut position = None;
                    for (id, value) in ebml_children(seek) {
                        match id {
                            0x53AB => target = value.iter().fold(0u32, |acc, &x| (acc << 8) | x as u32),
                            0x53AC => position = Some(ebml_uint(value)),
                            _ => {}
                        }
                    }
                    if let (true, Some(position)) = (matches!(target, INFO | TRACKS | TAGS), position) {
                        wanted.push((target, segment_data + position));
                    }
                }
            }
        } else if matches!(id, INFO | TRACKS | TAGS) {
            wanted.push((id, at));
        }
        match size {
            Some(size) => at = data_at + size,
            None => break,
        }
    }
    wanted.sort();
    wanted.dedup();

    let mut raw = Map::new();
    let mut tags = Map::new();
    let mut tracks = Vec::new();
    for (expected, offset) in wanted {
        let head = ctx.source.range(offset, 12)?;
        let Some((id, id_len)) = ebml_id(&head) else { continue };
        let Some((Some(size), size_len)) = ebml_size(&head[id_len..]) else { continue };
        if id != expected || size > 4 * 1024 * 1024 {
            continue;
        }
        let data = ctx.source.range(offset + (id_len + size_len) as u64, size as usize)?;
        match id {
            INFO => {
                let mut scale = 1_000_000f64;
                let mut duration = None;
                for (id, value) in ebml_children(&data) {
                    match id {
                        0x7BA9 => {
                            raw.insert("title".into(), json!(ebml_text(value)));
                        }
                        0x4D80 => {
                            raw.insert("muxing_app".into(), json!(ebml_text(value)));
                        }
                        0x5741 => {
                            raw.insert("writing_app".into(), json!(ebml_text(value)));
                        }
                        0x2AD7B1 => scale = ebml_uint(value) as f64,
                        0x4489 => duration = ebml_float(value),
                        0x4461 if value.len() == 8 => {
                            // Nanoseconds since 2001-01-01T00:00:00Z.
                            let ns = i64::from_be_bytes(value.try_into().unwrap());
                            if let Some(at) = chrono::DateTime::from_timestamp(978_307_200 + ns / 1_000_000_000, 0) {
                                raw.insert("date_utc".into(), json!(at.to_rfc3339()));
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(d) = duration {
                    let ms = (d * scale / 1_000_000.0).round() as u64;
                    raw.insert("duration_ms".into(), json!(ms));
                    ctx.norm("duration_ms", json!(ms));
                }
            }
            TRACKS => {
                for (_, entry) in ebml_children(&data).into_iter().filter(|(id, _)| *id == 0xAE) {
                    let mut track = Map::new();
                    for (id, value) in ebml_children(entry) {
                        match id {
                            0x83 => {
                                let kind = match ebml_uint(value) {
                                    1 => "video",
                                    2 => "audio",
                                    17 => "subtitle",
                                    _ => "other",
                                };
                                track.insert("type".into(), json!(kind));
                            }
                            0x86 => {
                                track.insert("codec".into(), json!(ebml_text(value)));
                            }
                            0x22B59C => {
                                track.insert("language".into(), json!(ebml_text(value)));
                            }
                            0x536E => {
                                track.insert("name".into(), json!(ebml_text(value)));
                            }
                            0xE0 => {
                                for (id, v) in ebml_children(value) {
                                    match id {
                                        0xB0 => {
                                            track.insert("width".into(), json!(ebml_uint(v)));
                                        }
                                        0xBA => {
                                            track.insert("height".into(), json!(ebml_uint(v)));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            0xE1 => {
                                for (id, v) in ebml_children(value) {
                                    match id {
                                        0xB5 => {
                                            track.insert("sample_rate".into(), json!(ebml_float(v)));
                                        }
                                        0x9F => {
                                            track.insert("channels".into(), json!(ebml_uint(v)));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    if track.get("type").and_then(Value::as_str) == Some("video") {
                        if let (Some(w), Some(h)) = (track.get("width").cloned(), track.get("height").cloned()) {
                            ctx.norm("width", w);
                            ctx.norm("height", h);
                        }
                    }
                    if let Some(codec) = track.get("codec").and_then(Value::as_str) {
                        ctx.norm("codec", json!(codec));
                    }
                    tracks.push(Value::Object(track));
                }
            }
            TAGS => {
                for (_, tag) in ebml_children(&data).into_iter().filter(|(id, _)| *id == 0x7373) {
                    for (id, value) in ebml_children(tag) {
                        if id == 0x67C8 {
                            simple_tags(value, &mut tags);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let text = |m: &Map<String, Value>, k: &str| m.get(k).and_then(Value::as_str).map(String::from);
    ctx.norm_str("title", text(&tags, "TITLE").or_else(|| text(&raw, "title")).as_deref());
    for (key, norm) in [("ARTIST", "artist"), ("ALBUM", "album"), ("GENRE", "genre"), ("DATE_RELEASED", "year"), ("COMMENT", "comment"), ("DESCRIPTION", "description"), ("COPYRIGHT", "copyright"), ("DIRECTOR", "director")] {
        ctx.norm_str(norm, text(&tags, key).as_deref());
    }
    ctx.norm_str("creator_tool", text(&raw, "writing_app").as_deref());
    if !tracks.is_empty() {
        raw.insert("tracks".into(), Value::Array(tracks));
    }
    if !tags.is_empty() {
        raw.insert("tags".into(), Value::Object(tags));
    }
    let found = !raw.is_empty();
    ctx.raw("matroska", raw);
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ebml_numbers() {
        assert_eq!(ebml_id(&[0x1A, 0x45, 0xDF, 0xA3]), Some((0x1A45DFA3, 4)));
        assert_eq!(ebml_id(&[0x83]), Some((0x83, 1)));
        assert_eq!(ebml_size(&[0x81]), Some((Some(1), 1)));
        assert_eq!(ebml_size(&[0x40, 0x02]), Some((Some(2), 2)));
        assert_eq!(ebml_size(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]), Some((None, 8)));
        assert_eq!(ebml_size(&[0xFF]), Some((None, 1)));
    }
}
