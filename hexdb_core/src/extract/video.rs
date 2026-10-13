// One frame from an MP4 or QuickTime video, for its thumbnail.
//
// Finds the video track in the `moov` box, picks a keyframe about a third of
// the way in (past intros and fades from black), reads only that sample, and
// decodes it with OpenH264. Fragmented MP4 (browser recordings, DASH) has
// empty sample tables, so its first fragment's first sample is used instead.
// Only H.264 (`avc1`/`avc3`) is decoded; HEVC, VP9 and AV1 get no thumbnail.
//
// Everything is read through the extraction `Source`, so its byte budget and
// deadline apply, and every size and offset from the file is checked before
// it's used: the boxes are untrusted input.

use super::Source;
use anyhow::{anyhow, bail};
use image::RgbImage;

/// Video types this reads.
pub const TYPES: &[&str] = &["video/mp4", "video/quicktime", "video/x-m4v", "video/3gpp", "video/3gpp2"];

const MAX_MOOV: u64 = 32 * 1024 * 1024;
const MAX_MOOF: u64 = 4 * 1024 * 1024;
const MAX_SAMPLE: u64 = 16 * 1024 * 1024;
const MAX_TOP_BOXES: usize = 10_000;
const MAX_SIDE: usize = 8192;

/// A box inside a buffer: its type and contents (after the header).
fn children(mut data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    while data.len() >= 8 {
        let size = u32::from_be_bytes(data[..4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = data[4..8].try_into().unwrap();
        let (header, size) = match size {
            0 => (8, data.len() as u64),
            1 if data.len() >= 16 => (16, u64::from_be_bytes(data[8..16].try_into().unwrap())),
            1 => break,
            n => (8, n),
        };
        if size < header as u64 || size > data.len() as u64 {
            break;
        }
        out.push((kind, &data[header..size as usize]));
        data = &data[size as usize..];
    }
    out
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    children(data).into_iter().find(|(k, _)| k == kind).map(|(_, d)| d)
}

fn path<'a>(data: &'a [u8], kinds: &[&[u8; 4]]) -> Option<&'a [u8]> {
    kinds.iter().try_fold(data, |d, k| child(d, k))
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// A full box's table: `count` entries of `width` bytes after version, flags and the count.
fn table(b: &[u8], width: usize) -> Option<(usize, &[u8])> {
    let count = u32_at(b, 4)? as usize;
    let body = b.get(8..)?;
    if count.checked_mul(width)? > body.len() {
        return None;
    }
    Some((count, body))
}

/// What's needed to decode a sample: the parameter sets and how NAL lengths are written.
struct Avc {
    nal_length: usize,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
}

fn avc_config(stsd: &[u8]) -> Option<Avc> {
    // stsd: version/flags, entry count, then sample entries (boxes).
    let entries = stsd.get(8..)?;
    for (kind, entry) in children(entries) {
        if &kind != b"avc1" && &kind != b"avc3" {
            continue;
        }
        // VisualSampleEntry: 78 bytes of fields, then child boxes.
        let avcc = child(entry.get(78..)?, b"avcC")?;
        let nal_length = (*avcc.get(4)? & 0x03) as usize + 1;
        let mut at = 5;
        let sets = |count_mask: u8, at: &mut usize| -> Option<Vec<Vec<u8>>> {
            let count = (*avcc.get(*at)? & count_mask) as usize;
            *at += 1;
            let mut out = Vec::new();
            for _ in 0..count {
                let len = u16_at(avcc, *at)? as usize;
                out.push(avcc.get(*at + 2..*at + 2 + len)?.to_vec());
                *at += 2 + len;
            }
            Some(out)
        };
        let sps = sets(0x1F, &mut at)?;
        let pps = sets(0xFF, &mut at)?;
        return Some(Avc { nal_length, sps, pps });
    }
    None
}

/// Quarter turns clockwise from a track header's matrix (phones record portrait video sideways).
fn rotation(tkhd: &[u8]) -> u32 {
    let at = if tkhd.first() == Some(&1) { 4 + 8 + 8 + 4 + 4 + 8 } else { 4 + 4 + 4 + 4 + 4 + 4 } + 8 + 2 + 2 + 2 + 2;
    let (Some(a), Some(b)) = (u32_at(tkhd, at), u32_at(tkhd, at + 4)) else { return 0 };
    match (a as i32, b as i32) {
        (0, 0x10000) => 1,
        (-0x10000, 0) => 2,
        (0, -0x10000) => 3,
        _ => 0,
    }
}

fn track_id(tkhd: &[u8]) -> Option<u32> {
    u32_at(tkhd, if tkhd.first() == Some(&1) { 4 + 8 + 8 } else { 4 + 4 + 4 })
}

/// Where one sample lives: file offset and length.
fn sample_location(stbl: &[u8], sample: usize) -> Option<(u64, u64)> {
    let stsz = child(stbl, b"stsz")?;
    let fixed = u32_at(stsz, 4)? as u64;
    let count = u32_at(stsz, 8)? as usize;
    if sample >= count {
        return None;
    }
    let size_of = |i: usize| -> Option<u64> { if fixed != 0 { Some(fixed) } else { u32_at(stsz, 12 + i * 4).map(u64::from) } };

    let offsets: Vec<u64> = if let Some(stco) = child(stbl, b"stco") {
        let (n, body) = table(stco, 4)?;
        (0..n).map(|i| u32_at(body, i * 4).map(u64::from)).collect::<Option<_>>()?
    } else {
        let (n, body) = table(child(stbl, b"co64")?, 8)?;
        (0..n).map(|i| u64_at(body, i * 8)).collect::<Option<_>>()?
    };
    let (runs, body) = table(child(stbl, b"stsc")?, 12)?;

    // Walk the sample-to-chunk runs to the chunk holding `sample`.
    let mut first = 0usize;
    for r in 0..runs {
        let first_chunk = u32_at(body, r * 12)?.checked_sub(1)? as usize;
        let per_chunk = u32_at(body, r * 12 + 4)? as usize;
        let next_chunk = if r + 1 < runs { u32_at(body, (r + 1) * 12)?.checked_sub(1)? as usize } else { offsets.len() };
        if per_chunk == 0 || next_chunk < first_chunk {
            return None;
        }
        let in_run = (next_chunk - first_chunk).checked_mul(per_chunk)?;
        if sample < first + in_run {
            let chunk = first_chunk + (sample - first) / per_chunk;
            let first_in_chunk = sample - (sample - first) % per_chunk;
            let mut offset = *offsets.get(chunk)?;
            for i in first_in_chunk..sample {
                offset = offset.checked_add(size_of(i)?)?;
            }
            return Some((offset, size_of(sample)?));
        }
        first += in_run;
    }
    None
}

/// The keyframe about a third of the way through, by sample number.
fn pick_keyframe(stbl: &[u8]) -> Option<usize> {
    let count = u32_at(child(stbl, b"stsz")?, 8)? as usize;
    if count == 0 {
        return None;
    }
    let target = count / 3;
    let Some(stss) = child(stbl, b"stss") else { return Some(target) }; // every sample is a keyframe
    let (n, body) = table(stss, 4)?;
    let syncs: Vec<usize> = (0..n).filter_map(|i| u32_at(body, i * 4)).filter(|&s| s >= 1).map(|s| s as usize - 1).collect();
    syncs.iter().copied().filter(|&s| s <= target).max().or_else(|| syncs.first().copied())
}

/// In fragmented MP4: the first sample of `track` in the first fragment that has one.
fn first_fragment_sample(source: &mut Source, moofs: &[(u64, u64)], track: u32) -> anyhow::Result<Option<(u64, u64)>> {
    for &(start, size) in moofs.iter().take(8) {
        let moof = source.range(start, size.min(MAX_MOOF) as usize)?;
        let body = moof.get(8..).ok_or_else(|| anyhow!("short moof"))?;
        for (kind, traf) in children(body) {
            if &kind != b"traf" {
                continue;
            }
            let Some(tfhd) = child(traf, b"tfhd") else { continue };
            let flags = u32_at(tfhd, 0).unwrap_or(0) & 0x00FF_FFFF;
            if u32_at(tfhd, 4) != Some(track) {
                continue;
            }
            let mut at = 8;
            let mut base = start;
            if flags & 0x01 != 0 {
                base = u64_at(tfhd, at).ok_or_else(|| anyhow!("short tfhd"))?;
                at += 8;
            }
            for bit in [0x02, 0x08] {
                if flags & bit != 0 {
                    at += 4;
                }
            }
            let default_size = if flags & 0x10 != 0 { u32_at(tfhd, at) } else { None };
            let Some(trun) = child(traf, b"trun") else { continue };
            let tflags = u32_at(trun, 0).unwrap_or(0) & 0x00FF_FFFF;
            if u32_at(trun, 4).unwrap_or(0) == 0 {
                continue;
            }
            let mut at = 8;
            let data_offset = if tflags & 0x01 != 0 {
                let d = u32_at(trun, at).ok_or_else(|| anyhow!("short trun"))? as i32;
                at += 4;
                d as i64
            } else {
                0
            };
            if tflags & 0x04 != 0 {
                at += 4;
            }
            if tflags & 0x100 != 0 {
                at += 4;
            }
            let size = if tflags & 0x200 != 0 { u32_at(trun, at) } else { default_size };
            let Some(size) = size else { continue };
            let offset = (base as i64).checked_add(data_offset).filter(|&o| o >= 0).ok_or_else(|| anyhow!("bad data offset"))?;
            return Ok(Some((offset as u64, size as u64)));
        }
    }
    Ok(None)
}

/// A sample's NAL units (length-prefixed) as an Annex B stream, after the parameter sets.
fn annex_b(avc: &Avc, sample: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(sample.len() + 256);
    for nal in avc.sps.iter().chain(&avc.pps) {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    let mut at = 0;
    while at + avc.nal_length <= sample.len() {
        let len = sample[at..at + avc.nal_length].iter().fold(0usize, |n, &b| (n << 8) | b as usize);
        at += avc.nal_length;
        if len == 0 || at + len > sample.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&sample[at..at + len]);
        at += len;
    }
    out
}

#[cfg(feature = "video-thumbnails")]
fn to_rgb(yuv: &openh264::decoder::DecodedYUV<'_>) -> anyhow::Result<Option<RgbImage>> {
    use openh264::formats::YUVSource;
    let (w, h) = yuv.dimensions();
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        bail!("frame is {}x{}", w, h);
    }
    let mut rgb = vec![0u8; w * h * 3];
    yuv.write_rgb8(&mut rgb);
    Ok(RgbImage::from_raw(w as u32, h as u32, rgb))
}

#[cfg(feature = "video-thumbnails")]
fn decode(stream: &[u8]) -> anyhow::Result<Option<RgbImage>> {
    let mut decoder = openh264::decoder::Decoder::new().map_err(|e| anyhow!("H.264 decoder: {}", e))?;
    if let Some(yuv) = decoder.decode(stream).map_err(|e| anyhow!("H.264: {}", e))? {
        return to_rgb(&yuv);
    }
    if let Some(yuv) = decoder.flush_remaining().map_err(|e| anyhow!("H.264: {}", e))?.first() {
        return to_rgb(yuv);
    }
    Ok(None)
}

#[cfg(not(feature = "video-thumbnails"))]
fn decode(_stream: &[u8]) -> anyhow::Result<Option<RgbImage>> {
    Ok(None)
}

/// One decoded frame from the video, already turned upright, or `None` when
/// there's no H.264 video track (or this build has no decoder).
pub fn keyframe(source: &mut Source) -> anyhow::Result<Option<RgbImage>> {
    if !cfg!(feature = "video-thumbnails") {
        return Ok(None);
    }
    // Top-level boxes: find moov (anywhere: often at the end) and the fragments.
    let len = source.len();
    let mut at = 0u64;
    let mut moov = None;
    let mut moofs = Vec::new();
    for _ in 0..MAX_TOP_BOXES {
        if at + 8 > len {
            break;
        }
        let header = source.range(at, 16)?;
        let size32 = u32_at(&header, 0).unwrap_or(0) as u64;
        let size = match size32 {
            0 => len - at,
            1 => u64_at(&header, 8).ok_or_else(|| anyhow!("short box header"))?,
            n => n,
        };
        if size < 8 || at.checked_add(size).is_none_or(|end| end > len) {
            break;
        }
        match &header[4..8] {
            b"moov" => moov = Some((at, size, if size32 == 1 { 16 } else { 8 })),
            b"moof" => moofs.push((at, size)),
            _ => {}
        }
        if moov.is_some() && !moofs.is_empty() {
            break;
        }
        at += size;
    }
    let Some((moov_at, moov_size, moov_header)) = moov else { return Ok(None) };
    if moov_size > MAX_MOOV {
        bail!("the video's index is over {} MB", MAX_MOOV / (1024 * 1024));
    }
    let moov_bytes = source.range(moov_at, moov_size as usize)?;
    let moov = moov_bytes.get(moov_header..).unwrap_or_default();

    for (kind, trak) in children(moov) {
        if &kind != b"trak" {
            continue;
        }
        let Some(hdlr) = path(trak, &[b"mdia", b"hdlr"]) else { continue };
        if hdlr.get(8..12) != Some(b"vide") {
            continue;
        }
        let Some(stbl) = path(trak, &[b"mdia", b"minf", b"stbl"]) else { continue };
        let Some(avc) = child(stbl, b"stsd").and_then(avc_config) else { return Ok(None) }; // not H.264
        let tkhd = child(trak, b"tkhd").unwrap_or_default();

        let location = match pick_keyframe(stbl) {
            Some(sample) => sample_location(stbl, sample),
            None => match track_id(tkhd) {
                Some(id) => first_fragment_sample(source, &moofs, id)?,
                None => None,
            },
        };
        let Some((offset, size)) = location else { bail!("couldn't find a keyframe") };
        if size == 0 || size > MAX_SAMPLE {
            bail!("keyframe is {} bytes", size);
        }
        let sample = source.range(offset, size as usize)?;
        let Some(frame) = decode(&annex_b(&avc, &sample))? else { bail!("the keyframe didn't decode") };
        return Ok(Some(match rotation(tkhd) {
            1 => image::imageops::rotate90(&frame),
            2 => image::imageops::rotate180(&frame),
            3 => image::imageops::rotate270(&frame),
            _ => frame,
        }));
    }
    Ok(None)
}
