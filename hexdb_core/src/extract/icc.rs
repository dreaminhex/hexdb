// ICC color profiles embedded in images: JPEG (APP2 ICC_PROFILE segments),
// PNG (iCCP), WebP (ICCP), TIFF (tag 34675), Photoshop (image resource 1039)
// and HEIF/AVIF (a `colr` box of type `prof`). The profile's header and its
// description, copyright, manufacturer and model texts (v2 `desc`/`text` and
// v4 `mluc` types) go under `icc`; the description is `color_profile`
// ("sRGB IEC61966-2.1", "Display P3", "Adobe RGB (1998)", ...).
//
// Written from the ICC specification (ICC.1:2022); no library.

use super::{xml, Ctx};
use serde_json::{json, Map, Value};
use std::io::Read;

/// Largest profile read.
const MAX_PROFILE: usize = 4 * 1024 * 1024;

fn be32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}

fn signature(b: &[u8], at: usize) -> Option<String> {
    let s = b.get(at..at + 4)?;
    if s.iter().all(|&c| c == 0) {
        return None;
    }
    let text: String = s.iter().map(|&c| if c.is_ascii_graphic() || c == b' ' { c as char } else { '?' }).collect();
    Some(text.trim_end().to_string())
}

/// A text tag: `desc` (v2 textDescriptionType), `text` (textType) or `mluc` (v4, the first record).
fn tag_text(profile: &[u8], offset: usize, size: usize) -> Option<String> {
    let data = profile.get(offset..offset.checked_add(size)?)?;
    let text = match data.get(0..4)? {
        b"desc" => {
            let count = be32(data, 8)? as usize;
            let bytes = data.get(12..12 + count.min(data.len().saturating_sub(12)))?;
            String::from_utf8_lossy(bytes).into_owned()
        }
        b"text" => String::from_utf8_lossy(data.get(8..)?).into_owned(),
        b"mluc" => {
            let records = be32(data, 8)? as usize;
            let record_size = be32(data, 12)? as usize;
            if records == 0 || record_size < 12 {
                return None;
            }
            // Prefer English; else the first record.
            let record = (0..records.min(64)).map(|i| 16 + i * record_size).find(|&r| data.get(r..r + 2) == Some(b"en")).unwrap_or(16);
            let len = be32(data, record + 4)? as usize;
            let at = be32(data, record + 8)? as usize;
            let units: Vec<u16> = data.get(at..at.checked_add(len)?)?.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
            String::from_utf16_lossy(&units)
        }
        _ => return None,
    };
    let text = text.trim_matches(char::from(0)).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Describe a profile: its header and text tags.
pub(crate) fn parse(profile: &[u8]) -> Option<Map<String, Value>> {
    if profile.len() < 132 || profile.get(36..40) != Some(b"acsp") {
        return None;
    }
    let mut out = Map::new();
    out.insert("size".into(), json!(be32(profile, 0)?));
    if let Some(cmm) = signature(profile, 4) {
        out.insert("cmm".into(), json!(cmm));
    }
    out.insert("version".into(), json!(format!("{}.{}.{}", profile[8], profile[9] >> 4, profile[9] & 0xF)));
    let class = match profile.get(12..16)? {
        b"scnr" => "input",
        b"mntr" => "display",
        b"prtr" => "output",
        b"link" => "device link",
        b"spac" => "color space",
        b"abst" => "abstract",
        b"nmcl" => "named color",
        _ => "other",
    };
    out.insert("device_class".into(), json!(class));
    if let Some(space) = signature(profile, 16) {
        out.insert("color_space".into(), json!(space));
    }
    if let Some(pcs) = signature(profile, 20) {
        out.insert("connection_space".into(), json!(pcs));
    }
    let n = |at: usize| u16::from_be_bytes([profile[at], profile[at + 1]]);
    if n(24) > 0 {
        out.insert("created".into(), json!(format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", n(24), n(26), n(28), n(30), n(32), n(34))));
    }
    if let Some(platform) = signature(profile, 40) {
        out.insert("platform".into(), json!(platform));
    }
    if let Some(maker) = signature(profile, 48) {
        out.insert("device_manufacturer".into(), json!(maker));
    }
    let intent = match be32(profile, 64)? {
        0 => "perceptual",
        1 => "relative colorimetric",
        2 => "saturation",
        3 => "absolute colorimetric",
        _ => "other",
    };
    out.insert("rendering_intent".into(), json!(intent));
    let tags = be32(profile, 128)? as usize;
    for i in 0..tags.min(256) {
        let at = 132 + i * 12;
        let (Some(sig), Some(offset), Some(size)) = (profile.get(at..at + 4), be32(profile, at + 4), be32(profile, at + 8)) else { break };
        let key = match sig {
            b"desc" => "description",
            b"cprt" => "copyright",
            b"dmnd" => "manufacturer",
            b"dmdd" => "model",
            _ => continue,
        };
        if let Some(text) = tag_text(profile, offset as usize, size as usize) {
            out.insert(key.into(), json!(text.chars().take(500).collect::<String>()));
        }
    }
    Some(out)
}

/// JPEG APP2 `ICC_PROFILE` segments, joined in sequence order.
fn from_jpeg(head: &[u8]) -> Option<Vec<u8>> {
    if !head.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut parts: Vec<(u8, &[u8])> = Vec::new();
    let mut at = 2;
    while at + 4 <= head.len() && head[at] == 0xFF {
        let marker = head[at + 1];
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        let len = u16::from_be_bytes([head[at + 2], head[at + 3]]) as usize;
        let segment = head.get(at + 4..at + 2 + len)?;
        if marker == 0xE2 && segment.starts_with(b"ICC_PROFILE\0") && segment.len() > 14 {
            parts.push((segment[12], &segment[14..]));
        }
        at += 2 + len;
    }
    if parts.is_empty() {
        return None;
    }
    parts.sort_by_key(|(seq, _)| *seq);
    Some(parts.into_iter().flat_map(|(_, data)| data.iter().copied()).collect())
}

/// PNG `iCCP`: name, NUL, compression method, zlib data.
fn from_png(head: &[u8]) -> Option<Vec<u8>> {
    let at = xml::memfind(head, b"iCCP")?;
    let size = be32(head, at.checked_sub(4)?)? as usize;
    let data = head.get(at + 4..at + 4 + size)?;
    let nul = data.iter().position(|&b| b == 0)?;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data.get(nul + 2..)?).take(MAX_PROFILE as u64).read_to_end(&mut out).ok()?;
    Some(out)
}

/// WebP `ICCP` chunk (little-endian size).
fn from_webp(head: &[u8]) -> Option<Vec<u8>> {
    if head.get(0..4) != Some(b"RIFF") || head.get(8..12) != Some(b"WEBP") {
        return None;
    }
    let mut at = 12;
    while at + 8 <= head.len() {
        let size = u32::from_le_bytes(head[at + 4..at + 8].try_into().unwrap()) as usize;
        if &head[at..at + 4] == b"ICCP" {
            return head.get(at + 8..at + 8 + size).map(<[u8]>::to_vec);
        }
        at += 8 + size + (size & 1);
    }
    None
}

/// HEIF/AVIF (and other ISO media): a `colr` box of type `prof`.
fn from_isobmff(head: &[u8]) -> Option<Vec<u8>> {
    if head.get(4..8) != Some(b"ftyp") {
        return None;
    }
    let mut from = 0;
    while let Some(found) = xml::memfind(&head[from..], b"colrprof") {
        let at = from + found;
        if let Some(size) = at.checked_sub(4).and_then(|s| be32(head, s)) {
            if let Some(profile) = head.get(at + 8..at - 4 + size as usize) {
                return Some(profile.to_vec());
            }
        }
        from = at + 8;
    }
    None
}

/// TIFF: tag 34675 (InterColorProfile) in the first IFD.
fn from_tiff(ctx: &mut Ctx) -> Option<Vec<u8>> {
    let head = &ctx.head;
    let little = match head.get(0..4)? {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return None,
    };
    let u16_at = |b: &[u8], at: usize| b.get(at..at + 2).map(|s| if little { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) });
    let u32_at = |b: &[u8], at: usize| b.get(at..at + 4).map(|s| if little { u32::from_le_bytes(s.try_into().unwrap()) } else { u32::from_be_bytes(s.try_into().unwrap()) });
    let ifd = u32_at(head, 4)? as usize;
    let entries = u16_at(head, ifd)? as usize;
    for i in 0..entries.min(1000) {
        let entry = ifd + 2 + i * 12;
        if u16_at(head, entry)? == 34675 {
            let count = u32_at(head, entry + 4)? as usize;
            let offset = u32_at(head, entry + 8)? as u64;
            if count > MAX_PROFILE {
                return None;
            }
            return ctx.source.range(offset, count).ok();
        }
    }
    None
}

/// Photoshop image resource 1039 (in PSD files).
fn from_psd(ctx: &Ctx) -> Option<Vec<u8>> {
    let head = &ctx.head;
    if !head.starts_with(b"8BPS") || head.len() < 30 {
        return None;
    }
    let color_len = be32(head, 26)? as usize;
    let resources_at = 30 + color_len;
    let len = be32(head, resources_at)? as usize;
    let resources = head.get(resources_at + 4..resources_at + 4 + len)?;
    super::image::photoshop_resources(resources).into_iter().find(|(id, _)| *id == 1039).map(|(_, data)| data.to_vec())
}

pub(crate) fn icc(ctx: &mut Ctx) -> anyhow::Result<bool> {
    let profile = from_jpeg(&ctx.head)
        .or_else(|| from_png(&ctx.head))
        .or_else(|| from_webp(&ctx.head))
        .or_else(|| from_isobmff(&ctx.head))
        .or_else(|| from_psd(ctx))
        .or_else(|| from_tiff(ctx));
    let Some(map) = profile.filter(|p| p.len() <= MAX_PROFILE).as_deref().and_then(parse) else { return Ok(false) };
    if let Some(description) = map.get("description").and_then(Value::as_str) {
        ctx.norm("color_profile", json!(description));
    }
    ctx.raw("icc", map);
    Ok(true)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal v4 display profile with an `mluc` description and a v2-style `desc` copyright.
    pub(crate) fn profile(description: &str) -> Vec<u8> {
        let mut desc = b"mluc\0\0\0\0".to_vec();
        desc.extend(1u32.to_be_bytes());
        desc.extend(12u32.to_be_bytes());
        desc.extend(b"enUS");
        let text: Vec<u8> = description.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        desc.extend((text.len() as u32).to_be_bytes());
        desc.extend(28u32.to_be_bytes());
        desc.extend(&text);
        let mut cprt = b"desc\0\0\0\0".to_vec();
        cprt.extend(10u32.to_be_bytes());
        cprt.extend(b"Public Do\0");
        let mut p = vec![0u8; 128];
        p[4..8].copy_from_slice(b"lcms");
        p[8] = 4;
        p[9] = 0x30;
        p[12..16].copy_from_slice(b"mntr");
        p[16..20].copy_from_slice(b"RGB ");
        p[20..24].copy_from_slice(b"XYZ ");
        p[24..26].copy_from_slice(&2024u16.to_be_bytes());
        p[26..28].copy_from_slice(&3u16.to_be_bytes());
        p[28..30].copy_from_slice(&15u16.to_be_bytes());
        p[36..40].copy_from_slice(b"acsp");
        p[40..44].copy_from_slice(b"APPL");
        p.extend(2u32.to_be_bytes());
        let tags_end = 132 + 2 * 12;
        let desc_at = tags_end;
        let cprt_at = desc_at + desc.len();
        for (sig, at, len) in [(b"desc", desc_at, desc.len()), (b"cprt", cprt_at, cprt.len())] {
            p.extend(sig);
            p.extend((at as u32).to_be_bytes());
            p.extend((len as u32).to_be_bytes());
        }
        p.extend(&desc);
        p.extend(&cprt);
        let size = p.len() as u32;
        p[0..4].copy_from_slice(&size.to_be_bytes());
        p
    }

    #[test]
    fn profiles_are_described() {
        let map = parse(&profile("Display P3")).unwrap();
        assert_eq!(map["description"], "Display P3");
        assert_eq!(map["copyright"], "Public Do");
        assert_eq!(map["device_class"], "display");
        assert_eq!(map["color_space"], "RGB");
        assert_eq!(map["version"], "4.3.0");
        assert_eq!(map["created"], "2024-03-15T00:00:00Z");
        assert!(parse(b"not a profile").is_none());
        // Truncated tag data is skipped, not a panic.
        let p = profile("Display P3");
        assert!(parse(&p[..140]).is_some_and(|m| m.get("description").is_none()));
    }

    #[test]
    fn jpeg_segments_are_joined_in_order() {
        let p = profile("sRGB IEC61966-2.1");
        let (a, b) = p.split_at(100);
        let segment = |seq: u8, data: &[u8]| {
            let mut s = vec![0xFF, 0xE2];
            s.extend(((data.len() + 16) as u16).to_be_bytes());
            s.extend(b"ICC_PROFILE\0");
            s.extend([seq, 2]);
            s.extend(data);
            s
        };
        let jpeg = [vec![0xFF, 0xD8], segment(2, b), segment(1, a), vec![0xFF, 0xDA]].concat();
        assert_eq!(from_jpeg(&jpeg).unwrap(), p);
    }
}
