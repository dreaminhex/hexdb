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

/// The keyframe at or before `fraction` of the way through (0.0-1.0), by sample number.
fn pick_keyframe(stbl: &[u8], fraction: f64) -> Option<usize> {
    let count = u32_at(child(stbl, b"stsz")?, 8)? as usize;
    if count == 0 {
        return None;
    }
    let target = ((count as f64) * fraction.clamp(0.0, 1.0)) as usize;
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
    Ok(keyframes(source, &[1.0 / 3.0])?.into_iter().next())
}

/// Decoded keyframes near these points of the video (fractions of its length),
/// upright and without repeats; empty when there's no H.264 video track (or no
/// decoder). Fragmented MP4 gives its first keyframe only.
pub fn keyframes(source: &mut Source, fractions: &[f64]) -> anyhow::Result<Vec<RgbImage>> {
    if !cfg!(feature = "video-thumbnails") {
        return Ok(Vec::new());
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
    let Some((moov_at, moov_size, moov_header)) = moov else { return Ok(Vec::new()) };
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
        let Some(avc) = child(stbl, b"stsd").and_then(avc_config) else { return Ok(Vec::new()) }; // not H.264
        let tkhd = child(trak, b"tkhd").unwrap_or_default();

        let mut samples: Vec<usize> = fractions.iter().filter_map(|&f| pick_keyframe(stbl, f)).collect();
        samples.dedup();
        let locations: Vec<(u64, u64)> = if samples.is_empty() {
            match track_id(tkhd) {
                Some(id) => first_fragment_sample(source, &moofs, id)?.into_iter().collect(),
                None => Vec::new(),
            }
        } else {
            samples.iter().filter_map(|&s| sample_location(stbl, s)).collect()
        };
        if locations.is_empty() {
            bail!("couldn't find a keyframe");
        }
        let mut frames = Vec::new();
        for (offset, size) in locations {
            if size == 0 || size > MAX_SAMPLE {
                bail!("keyframe is {} bytes", size);
            }
            let sample = source.range(offset, size as usize)?;
            let Some(frame) = decode(&annex_b(&avc, &sample))? else { bail!("the keyframe didn't decode") };
            frames.push(match rotation(tkhd) {
                1 => image::imageops::rotate90(&frame),
                2 => image::imageops::rotate180(&frame),
                3 => image::imageops::rotate270(&frame),
                _ => frame,
            });
        }
        return Ok(frames);
    }
    Ok(Vec::new())
}

/// Builds small H.264 MP4 files for tests (here and in hexdb_tests).
#[cfg(feature = "video-thumbnails")]
#[doc(hidden)]
pub mod fixtures {
    use openh264::encoder::Encoder;
    use openh264::formats::{RgbSliceU8, YUVBuffer};

    fn nal_units(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= stream.len() {
            if stream[i..i + 3] == [0, 0, 1] {
                starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        starts
            .iter()
            .enumerate()
            .map(|(n, &s)| {
                let end = starts.get(n + 1).map(|&e| e - 3).unwrap_or(stream.len());
                let mut nal = stream[s..end].to_vec();
                while nal.last() == Some(&0) {
                    nal.pop();
                }
                nal
            })
            .filter(|n| !n.is_empty())
            .collect()
    }

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        mp4_box(kind, &[&[0u8; 4][..], body].concat())
    }

    /// An MP4 of `w` x `h` keyframes, one per color, in order.
    pub fn mp4(colors: &[[u8; 3]], w: usize, h: usize) -> Vec<u8> {
        let (mut sps, mut pps, mut samples) = (Vec::new(), Vec::new(), Vec::new());
        for color in colors {
            let rgb: Vec<u8> = (0..w * h).flat_map(|_| *color).collect();
            let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&rgb, (w, h)));
            let stream = Encoder::new().expect("an encoder").encode(&yuv).expect("encoded").to_vec();
            let mut sample = Vec::new();
            for nal in nal_units(&stream) {
                match nal[0] & 0x1F {
                    7 => sps = nal,
                    8 => pps = nal,
                    5 | 1 => {
                        sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                        sample.extend_from_slice(&nal);
                    }
                    _ => {}
                }
            }
            samples.push(sample);
        }
        let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&sps);
        avcc.push(1);
        avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&pps);
        let avc1 = mp4_box(b"avc1", &[&[0u8; 78][..], &mp4_box(b"avcC", &avcc)].concat());
        let stsd = full(b"stsd", &[&1u32.to_be_bytes()[..], &avc1].concat());
        let mut tkhd = vec![0u8; 80];
        tkhd[8..12].copy_from_slice(&1u32.to_be_bytes());
        for (i, v) in [0x10000i32, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000].iter().enumerate() {
            tkhd[36 + i * 4..40 + i * 4].copy_from_slice(&v.to_be_bytes());
        }
        let tkhd = full(b"tkhd", &tkhd);
        let mut stsz = [0u32.to_be_bytes(), (samples.len() as u32).to_be_bytes()].concat();
        for sample in &samples {
            stsz.extend_from_slice(&(sample.len() as u32).to_be_bytes());
        }
        let data: Vec<u8> = samples.concat();
        let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
        let build = |chunk_offset: u32| {
            let stbl = mp4_box(
                b"stbl",
                &[
                    stsd.clone(),
                    full(b"stsz", &stsz),
                    full(b"stsc", &[1u32.to_be_bytes(), 1u32.to_be_bytes(), (samples.len() as u32).to_be_bytes(), 1u32.to_be_bytes()].concat()),
                    full(b"stco", &[1u32.to_be_bytes(), chunk_offset.to_be_bytes()].concat()),
                ]
                .concat(),
            );
            let hdlr = full(b"hdlr", &[&[0u8; 4][..], b"vide", &[0u8; 13]].concat());
            let mdia = mp4_box(b"mdia", &[hdlr, mp4_box(b"minf", &stbl)].concat());
            mp4_box(b"moov", &mp4_box(b"trak", &[tkhd.clone(), mdia].concat()))
        };
        let offset = (ftyp.len() + build(0).len() + 8) as u32;
        [ftyp, build(offset), mp4_box(b"mdat", &data)].concat()
    }
}

#[cfg(all(test, feature = "video-thumbnails"))]
mod tests {
    use super::*;
    use crate::extract::{Limits, Source};
    use openh264::encoder::Encoder;
    use openh264::formats::{RgbSliceU8, YUVBuffer};

    /// One H.264 keyframe of a left-red, right-blue picture: (SPS, PPS, IDR NAL units).
    fn encoded_frame(w: usize, h: usize) -> (Vec<u8>, Vec<u8>, Vec<Vec<u8>>) {
        let rgb: Vec<u8> = (0..w * h).flat_map(|i| if i % w < w / 2 { [220, 30, 30] } else { [30, 30, 220] }).collect();
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&rgb, (w, h)));
        let stream = Encoder::new().unwrap().encode(&yuv).unwrap().to_vec();
        let (mut sps, mut pps, mut slices) = (Vec::new(), Vec::new(), Vec::new());
        for nal in split_annex_b(&stream) {
            match nal[0] & 0x1F {
                7 => sps = nal,
                8 => pps = nal,
                5 | 1 => slices.push(nal),
                _ => {}
            }
        }
        (sps, pps, slices)
    }

    fn split_annex_b(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= stream.len() {
            if stream[i..i + 3] == [0, 0, 1] {
                starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        starts
            .iter()
            .enumerate()
            .map(|(n, &s)| {
                let end = starts.get(n + 1).map(|&e| e - 3).unwrap_or(stream.len());
                let mut nal = stream[s..end].to_vec();
                while nal.last() == Some(&0) {
                    nal.pop();
                }
                nal
            })
            .filter(|n| !n.is_empty())
            .collect()
    }

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        mp4_box(kind, &[&[0u8; 4][..], body].concat())
    }

    fn tkhd(rotate_90: bool) -> Vec<u8> {
        let mut b = vec![0u8; 80];
        b[8..12].copy_from_slice(&1u32.to_be_bytes()); // track ID
        let (a, b2, c, d): (i32, i32, i32, i32) = if rotate_90 { (0, 0x10000, -0x10000, 0) } else { (0x10000, 0, 0, 0x10000) };
        for (i, v) in [a, b2, 0, c, d, 0, 0, 0, 0x4000_0000].iter().enumerate() {
            b[36 + i * 4..40 + i * 4].copy_from_slice(&v.to_be_bytes());
        }
        full(b"tkhd", &b)
    }

    fn stsd(sps: &[u8], pps: &[u8]) -> Vec<u8> {
        let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(sps);
        avcc.push(1);
        avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(pps);
        let avc1 = mp4_box(b"avc1", &[&[0u8; 78][..], &mp4_box(b"avcC", &avcc)].concat());
        full(b"stsd", &[&1u32.to_be_bytes()[..], &avc1].concat())
    }

    fn length_prefixed(slices: &[Vec<u8>]) -> Vec<u8> {
        slices.iter().flat_map(|n| [(n.len() as u32).to_be_bytes().to_vec(), n.clone()].concat()).collect()
    }

    /// A plain MP4: ftyp, then a moov whose stbl points at one sample in the mdat that follows.
    fn plain_mp4(w: usize, h: usize, rotate_90: bool) -> Vec<u8> {
        let (sps, pps, slices) = encoded_frame(w, h);
        let sample = length_prefixed(&slices);
        let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
        let build = |chunk_offset: u32| {
            let stbl = mp4_box(
                b"stbl",
                &[
                    stsd(&sps, &pps),
                    full(b"stsz", &[0u32.to_be_bytes(), 1u32.to_be_bytes(), (sample.len() as u32).to_be_bytes()].concat()),
                    full(b"stsc", &[1u32.to_be_bytes(), 1u32.to_be_bytes(), 1u32.to_be_bytes(), 1u32.to_be_bytes()].concat()),
                    full(b"stco", &[1u32.to_be_bytes(), chunk_offset.to_be_bytes()].concat()),
                ]
                .concat(),
            );
            let hdlr = full(b"hdlr", &[&[0u8; 4][..], b"vide", &[0u8; 13]].concat());
            let mdia = mp4_box(b"mdia", &[hdlr, mp4_box(b"minf", &stbl)].concat());
            mp4_box(b"moov", &mp4_box(b"trak", &[tkhd(rotate_90), mdia].concat()))
        };
        let moov_len = build(0).len();
        let offset = (ftyp.len() + moov_len + 8) as u32;
        [ftyp, build(offset), mp4_box(b"mdat", &sample)].concat()
    }

    fn frame(bytes: Vec<u8>) -> Option<RgbImage> {
        keyframe(&mut Source::new(Box::new(bytes), &Limits::default())).unwrap()
    }

    #[test]
    fn decodes_a_keyframe_from_a_plain_mp4() {
        let img = frame(plain_mp4(64, 48, false)).expect("a frame");
        assert_eq!(img.dimensions(), (64, 48));
        let (left, right) = (img.get_pixel(8, 24), img.get_pixel(56, 24));
        assert!(left[0] > 150 && left[2] < 100, "left is red: {:?}", left);
        assert!(right[2] > 150 && right[0] < 100, "right is blue: {:?}", right);
    }

    #[test]
    fn decodes_keyframes_spread_through_the_video() {
        let colors = [[220, 30, 30], [30, 200, 30], [30, 30, 220]];
        let mp4 = fixtures::mp4(&colors, 32, 32);
        let frames = keyframes(&mut Source::new(Box::new(mp4), &Limits::default()), &[0.1, 0.5, 0.9]).unwrap();
        assert_eq!(frames.len(), 3);
        for (frame, color) in frames.iter().zip(colors) {
            let px = frame.get_pixel(16, 16);
            let strongest = (0..3).max_by_key(|&c| px[c]).unwrap();
            assert_eq!(strongest, (0..3).max_by_key(|&c| color[c]).unwrap(), "{:?} for {:?}", px, color);
        }
        // Points that land on the same keyframe give it once.
        let mp4 = fixtures::mp4(&colors[..1], 32, 32);
        assert_eq!(keyframes(&mut Source::new(Box::new(mp4), &Limits::default()), &[0.2, 0.4, 0.6]).unwrap().len(), 1);
    }

    #[test]
    fn turns_rotated_video_upright() {
        let img = frame(plain_mp4(64, 48, true)).expect("a frame");
        assert_eq!(img.dimensions(), (48, 64));
    }

    #[test]
    fn reads_the_first_sample_of_a_fragmented_mp4() {
        let (sps, pps, slices) = encoded_frame(32, 32);
        let sample = length_prefixed(&slices);
        let stbl = mp4_box(
            b"stbl",
            &[
                stsd(&sps, &pps),
                full(b"stsz", &[0u32.to_be_bytes(), 0u32.to_be_bytes()].concat()),
                full(b"stsc", &0u32.to_be_bytes()),
                full(b"stco", &0u32.to_be_bytes()),
            ]
            .concat(),
        );
        let hdlr = full(b"hdlr", &[&[0u8; 4][..], b"vide", &[0u8; 13]].concat());
        let moov = mp4_box(b"moov", &mp4_box(b"trak", &[tkhd(false), mp4_box(b"mdia", &[hdlr, mp4_box(b"minf", &stbl)].concat())].concat()));
        // tfhd: default-base-is-moof, track 1. trun: data offset and one sample size.
        let tfhd = mp4_box(b"tfhd", &[0x0002_0000u32.to_be_bytes(), 1u32.to_be_bytes()].concat());
        let traf_len = |data_offset: u32| {
            let trun = mp4_box(b"trun", &[0x0000_0201u32.to_be_bytes(), 1u32.to_be_bytes(), data_offset.to_be_bytes(), (sample.len() as u32).to_be_bytes()].concat());
            mp4_box(b"moof", &mp4_box(b"traf", &[tfhd.clone(), trun].concat()))
        };
        let moof_len = traf_len(0).len() as u32;
        let file = [moov, traf_len(moof_len + 8), mp4_box(b"mdat", &sample)].concat();
        assert_eq!(frame(file).expect("a frame").dimensions(), (32, 32));
    }

    #[test]
    fn hostile_boxes_are_refused_quietly() {
        assert!(frame(b"not a video at all".to_vec()).is_none());
        // A moov claiming to be larger than the file.
        let mut lying = mp4_box(b"moov", &[0u8; 16]);
        lying[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(frame(lying).is_none());
        // A track whose sample table points past the end of the file.
        let mut mp4 = plain_mp4(32, 32, false);
        let at = mp4.windows(4).position(|w| w == b"stco").unwrap() + 12;
        mp4[at..at + 4].copy_from_slice(&0x7FFF_FFF0u32.to_be_bytes());
        assert!(keyframe(&mut Source::new(Box::new(mp4), &Limits::default())).ok().flatten().is_none());
    }
}
