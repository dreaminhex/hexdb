// The primary picture of a HEIC file, as an HEVC stream ffmpeg reads from a pipe.
//
// Phones store a photo as a grid of HEVC tiles (512 x 512 on an iPhone):
// ffmpeg can only assemble one from a file it can seek in, and HexDB never
// writes decrypted uploads to disk. So this reads the HEIF index itself (the
// `meta` box: the primary item, its type, where each item's data is, the grid
// and its tiles, the HEVC parameters and the rotation), and returns every
// tile's data, in order, as one Annex B stream with the grid's layout. ffmpeg
// decodes the tiles as frames and its `tile` filter puts them back together
// (see `crate::tools::ffmpeg_thumbnail`).
//
// Everything is read through the extraction `Source` and every count, size
// and offset from the file is checked before it's used.

use super::video::{child, children, u16_at, u32_at, CodedFrame, Grid};
use super::Source;
use anyhow::{bail, Result};
use std::collections::HashMap;

const MAX_META: u64 = 4 * 1024 * 1024;
const MAX_TILES: usize = 1024;
const MAX_TOTAL: u64 = 64 * 1024 * 1024;

/// Where an item's data is: (offset, length) extents, in the file (`in_idat`
/// false) or in the `meta` box's own `idat` (construction method 1).
#[derive(Debug, Clone, Default)]
struct Location {
    in_idat: bool,
    extents: Vec<(u64, u64)>,
}
/// Each item's properties: (box type, contents).
type Properties = HashMap<u32, Vec<([u8; 4], Vec<u8>)>>;

fn uint(b: &[u8], at: usize, size: usize) -> Option<u64> {
    match size {
        0 => Some(0),
        2 => u16_at(b, at).map(u64::from),
        4 => u32_at(b, at).map(u64::from),
        8 => Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?)),
        _ => None,
    }
}

/// `iloc`: each item's extents (construction methods 0, the file, and 1, `idat`).
fn locations(iloc: &[u8]) -> Option<HashMap<u32, Location>> {
    let version = *iloc.first()?;
    let sizes = *iloc.get(4)?;
    let (offset_size, length_size) = ((sizes >> 4) as usize, (sizes & 0x0F) as usize);
    let more = *iloc.get(5)?;
    let (base_size, index_size) = ((more >> 4) as usize, if version >= 1 { (more & 0x0F) as usize } else { 0 });
    let (count, mut at) = if version < 2 { (u16_at(iloc, 6)? as usize, 8) } else { (u32_at(iloc, 6)? as usize, 10) };
    let mut out = HashMap::new();
    for _ in 0..count.min(4096) {
        let id = if version < 2 { u16_at(iloc, at)? as u32 } else { u32_at(iloc, at)? };
        at += if version < 2 { 2 } else { 4 };
        let method = if version >= 1 {
            let m = u16_at(iloc, at)? & 0x0F;
            at += 2;
            m
        } else {
            0
        };
        at += 2; // data reference index
        let base = uint(iloc, at, base_size)?;
        at += base_size;
        let extents = u16_at(iloc, at)? as usize;
        at += 2;
        let mut list = Vec::new();
        for _ in 0..extents.min(64) {
            at += index_size;
            let offset = uint(iloc, at, offset_size)?;
            at += offset_size;
            let length = uint(iloc, at, length_size)?;
            at += length_size;
            list.push((base.checked_add(offset)?, length));
        }
        if method <= 1 {
            out.insert(id, Location { in_idat: method == 1, extents: list });
        }
    }
    Some(out)
}

/// `iinf`: each item's type.
fn item_types(iinf: &[u8]) -> Option<HashMap<u32, [u8; 4]>> {
    let skip = if *iinf.first()? == 0 { 6 } else { 8 };
    let mut out = HashMap::new();
    for (kind, infe) in children(iinf.get(skip..)?) {
        if &kind != b"infe" {
            continue;
        }
        let version = *infe.first()?;
        let (id, type_at) = match version {
            2 => (u16_at(infe, 4)? as u32, 8),
            3 => (u32_at(infe, 4)?, 10),
            _ => continue,
        };
        out.insert(id, infe.get(type_at..type_at + 4)?.try_into().ok()?);
    }
    Some(out)
}

/// `iref`'s `dimg` references: a grid's tiles, in order.
fn tiles_of(iref: &[u8], grid: u32) -> Option<Vec<u32>> {
    let wide = *iref.first()? != 0;
    for (kind, r) in children(iref.get(4..)?) {
        if &kind != b"dimg" {
            continue;
        }
        let id_size = if wide { 4 } else { 2 };
        let from = uint(r, 0, id_size)? as u32;
        if from != grid {
            continue;
        }
        let count = u16_at(r, id_size)? as usize;
        return (0..count.min(MAX_TILES)).map(|i| uint(r, id_size + 2 + i * id_size, id_size).map(|v| v as u32)).collect();
    }
    None
}

/// `iprp`: each item's properties (the boxes in `ipco` that `ipma` names for it).
fn properties(iprp: &[u8]) -> Option<Properties> {
    let ipco: Vec<([u8; 4], &[u8])> = children(child(iprp, b"ipco")?);
    let ipma = child(iprp, b"ipma")?;
    let (version, flags) = (*ipma.first()?, *ipma.get(3)?);
    let count = u32_at(ipma, 4)? as usize;
    let mut at = 8;
    let mut out = HashMap::new();
    for _ in 0..count.min(4096) {
        let id = if version < 1 { u16_at(ipma, at)? as u32 } else { u32_at(ipma, at)? };
        at += if version < 1 { 2 } else { 4 };
        let n = *ipma.get(at)? as usize;
        at += 1;
        let mut props = Vec::new();
        for _ in 0..n {
            let index = if flags & 1 == 1 {
                let v = u16_at(ipma, at)? & 0x7FFF;
                at += 2;
                v as usize
            } else {
                let v = ipma.get(at)? & 0x7F;
                at += 1;
                v as usize
            };
            if let Some((kind, body)) = index.checked_sub(1).and_then(|i| ipco.get(i)) {
                props.push((*kind, body.to_vec()));
            }
        }
        out.insert(id, props);
    }
    Some(out)
}

/// An item's data (at most `max` bytes), from the file or from `idat`.
fn item_bytes(source: &mut Source, idat: &[u8], location: &Location, max: u64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for &(offset, len) in &location.extents {
        let len = len.min(max.saturating_sub(out.len() as u64));
        if location.in_idat {
            let end = offset.checked_add(len).filter(|e| *e <= idat.len() as u64);
            let Some(end) = end else { bail!("an item points outside the photo's index") };
            out.extend_from_slice(&idat[offset as usize..end as usize]);
        } else {
            if offset.checked_add(len).is_none_or(|end| end > source.len()) {
                bail!("an item points past the end of the file");
            }
            out.extend(source.range(offset, len as usize)?);
        }
    }
    Ok(out)
}

/// The HEVC parameter sets and NAL length size from an `hvcC` property.
fn hevc_config(hvcc: &[u8]) -> Option<(usize, Vec<Vec<u8>>)> {
    super::video::hevc_config(hvcc)
}

/// The primary picture of a HEIC/HEIF file whose images are HEVC, as one
/// stream of its tiles (one tile when it isn't a grid); `None` for other files.
pub fn coded_image(source: &mut Source) -> Result<Option<CodedFrame>> {
    // The meta box is near the start, after ftyp.
    let head = source.head(MAX_META as usize)?;
    let Some(meta) = children(&head).into_iter().find(|(k, _)| k == b"meta").map(|(_, b)| b) else { return Ok(None) };
    let Some(meta) = meta.get(4..) else { return Ok(None) };
    let idat = child(meta, b"idat").unwrap_or_default();
    let Some(pitm) = child(meta, b"pitm") else { return Ok(None) };
    let primary = if pitm.first() == Some(&0) { u16_at(pitm, 4).map(u32::from) } else { u32_at(pitm, 4) };
    let (Some(primary), Some(types), Some(locs), Some(props)) =
        (primary, child(meta, b"iinf").and_then(item_types), child(meta, b"iloc").and_then(locations), child(meta, b"iprp").and_then(properties))
    else {
        return Ok(None);
    };
    let (tiles, grid) = match types.get(&primary) {
        Some(b"hvc1") => (vec![primary], None),
        Some(b"grid") => {
            let Some(tiles) = child(meta, b"iref").and_then(|r| tiles_of(r, primary)) else { return Ok(None) };
            // The grid's description is the item's own data (often in idat).
            let Some(location) = locs.get(&primary) else { return Ok(None) };
            let desc = item_bytes(source, idat, location, 32)?;
            let wide = desc.get(1).is_some_and(|f| f & 1 == 1);
            let (rows, cols) = (desc.get(2).map(|r| *r as u32 + 1), desc.get(3).map(|c| *c as u32 + 1));
            let (w, h) = if wide { (u32_at(&desc, 4), u32_at(&desc, 8)) } else { (u16_at(&desc, 4).map(u32::from), u16_at(&desc, 6).map(u32::from)) };
            let (Some(rows), Some(cols), Some(w), Some(h)) = (rows, cols, w, h) else { return Ok(None) };
            if tiles.len() != (rows * cols) as usize || w == 0 || h == 0 || w > 65_536 || h > 65_536 {
                bail!("the photo's grid doesn't add up");
            }
            (tiles, Some(Grid { cols, rows, width: w, height: h }))
        }
        _ => return Ok(None),
    };
    // Every tile is HEVC with the same parameters; the first tile's are used.
    let Some(tile_props) = props.get(&tiles[0]) else { return Ok(None) };
    let Some((nal_length, nals)) = tile_props.iter().find(|(k, _)| k == b"hvcC").and_then(|(_, b)| hevc_config(b)) else { return Ok(None) };
    if tiles.iter().any(|t| types.get(t).is_some_and(|k| k != b"hvc1")) {
        return Ok(None);
    }
    let mut data = Vec::new();
    let mut total = 0u64;
    for (i, tile) in tiles.iter().enumerate() {
        let Some(location) = locs.get(tile) else { bail!("a tile of the photo has no data") };
        total = total.saturating_add(location.extents.iter().map(|e| e.1).sum::<u64>());
        if total > MAX_TOTAL {
            bail!("the photo's tiles are over {} MB", MAX_TOTAL / (1024 * 1024));
        }
        let sample = item_bytes(source, idat, location, MAX_TOTAL)?;
        data.extend(super::video::to_annex_b(if i == 0 { &nals } else { &[] }, nal_length, &sample));
    }
    // irot: anticlockwise quarter turns, on the primary item.
    let ccw = props.get(&primary).and_then(|p| p.iter().find(|(k, _)| k == b"irot")).and_then(|(_, b)| b.first()).map_or(0, |a| (*a & 3) as u32);
    Ok(Some(CodedFrame { format: "hevc", data, rotation: (4 - ccw) % 4, grid }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{Limits, Source};

    fn coded(bytes: &[u8]) -> Option<CodedFrame> {
        coded_image(&mut Source::new(Box::new(bytes.to_vec()), &Limits::default())).unwrap()
    }

    /// Made by libheif's heif-enc from ffmpeg's 1280 x 960 test pattern: one with
    /// `--cut-tiles 512` (a 3 x 2 grid whose description is in idat), one whole.
    #[test]
    fn heic_photos_give_their_tiles_in_order() {
        let tiled = coded(include_bytes!("testdata/tiled.heic")).expect("a grid");
        assert_eq!(tiled.grid, Some(Grid { cols: 3, rows: 2, width: 1280, height: 960 }));
        assert_eq!((tiled.format, tiled.rotation), ("hevc", 0));
        assert!(tiled.data.starts_with(&[0, 0, 0, 1]));
        let plain = coded(include_bytes!("testdata/plain.heic")).expect("one picture");
        assert_eq!(plain.grid, None);
        assert!(plain.data.len() > 1000);
        // Not HEIF, or cut short: nothing, never a panic.
        assert!(coded(b"not a photo").is_none());
        let bytes = include_bytes!("testdata/tiled.heic");
        for cut in [100, 400, 800, 2000] {
            let _ = coded_image(&mut Source::new(Box::new(bytes[..cut].to_vec()), &Limits::default()));
        }
    }
}
