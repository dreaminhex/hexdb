// GoPro telemetry (GPMF) in MP4 files: the `gpmd` metadata track, one
// sample per second or so, each a tree of KLV records (a FourCC key, a type,
// a struct size and a repeat count, data padded to 4 bytes). Records the
// camera (DVNM), which sensors it logged (accelerometer, gyroscope, GPS, ...),
// and from the GPS (GPS5, or GPS9 on HERO11 and later): the first fix (the
// file's `gps` unless something else set it), start and end times, distance,
// top speed, altitude range and a track of up to 500 points under `gpmf`.
//
// Written from GoPro's published format (github.com/gopro/gpmf-parser); no
// library. Every size comes from the file, so all of them are checked.

use super::{video, Ctx};
use serde_json::{json, Map, Value};

/// Telemetry samples read (about one per second: an hour).
const MAX_SAMPLES: usize = 4000;
const MAX_SAMPLE: u64 = 1024 * 1024;
const MAX_TRACK_POINTS: usize = 500;
const MAX_DEPTH: usize = 8;

/// One KLV record.
struct Klv<'a> {
    key: [u8; 4],
    kind: u8,
    size: usize,
    repeat: usize,
    data: &'a [u8],
}

fn records(mut data: &[u8]) -> Vec<Klv<'_>> {
    let mut out = Vec::new();
    while data.len() >= 8 {
        let key: [u8; 4] = data[..4].try_into().unwrap();
        let (kind, size, repeat) = (data[4], data[5] as usize, u16::from_be_bytes([data[6], data[7]]) as usize);
        let len = size * repeat;
        let padded = (len + 3) & !3;
        if key == [0; 4] || data.len() < 8 + len {
            break;
        }
        out.push(Klv { key, kind, size, repeat, data: &data[8..8 + len] });
        data = &data[(8 + padded).min(data.len())..];
    }
    out
}

/// A record's numbers, struct by struct (`fields` per struct), as f64.
fn numbers(klv: &Klv, kinds: &[u8]) -> Vec<Vec<f64>> {
    let mut out = Vec::new();
    for i in 0..klv.repeat {
        let Some(s) = klv.data.get(i * klv.size..(i + 1) * klv.size) else { break };
        let mut at = 0;
        let mut row = Vec::new();
        for &kind in kinds {
            let (value, width) = match kind {
                b'b' => (s.get(at).map(|&v| v as i8 as f64), 1),
                b'B' => (s.get(at).map(|&v| v as f64), 1),
                b's' => (s.get(at..at + 2).map(|v| i16::from_be_bytes([v[0], v[1]]) as f64), 2),
                b'S' => (s.get(at..at + 2).map(|v| u16::from_be_bytes([v[0], v[1]]) as f64), 2),
                b'l' => (s.get(at..at + 4).map(|v| i32::from_be_bytes(v.try_into().unwrap()) as f64), 4),
                b'L' => (s.get(at..at + 4).map(|v| u32::from_be_bytes(v.try_into().unwrap()) as f64), 4),
                b'f' => (s.get(at..at + 4).map(|v| f32::from_be_bytes(v.try_into().unwrap()) as f64), 4),
                b'd' => (s.get(at..at + 8).map(|v| f64::from_be_bytes(v.try_into().unwrap())), 8),
                _ => (None, 0),
            };
            let Some(value) = value else { break };
            row.push(value);
            at += width;
        }
        out.push(row);
    }
    out
}

fn text(klv: &Klv) -> String {
    String::from_utf8_lossy(klv.data).trim_matches(char::from(0)).trim().to_string()
}

/// `yymmddhhmmss.sss` (GPSU) as RFC 3339.
fn gps_time(s: &str) -> Option<String> {
    let d = s.as_bytes();
    if d.len() < 12 || !d[..12].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let frac = s.get(12..).unwrap_or("").trim_start_matches('.');
    Some(format!("20{}-{}-{}T{}:{}:{}{}Z", &s[0..2], &s[2..4], &s[4..6], &s[6..8], &s[8..10], &s[10..12], if frac.is_empty() { String::new() } else { format!(".{}", frac) }))
}

#[derive(Default)]
struct Telemetry {
    device: Option<String>,
    sensors: Vec<String>,
    /// lat, lon, alt (m), 2D speed (m/s), and the fix's time when known.
    points: Vec<(f64, f64, f64, f64)>,
    first_time: Option<String>,
    last_time: Option<String>,
}

const SENSORS: &[(&[u8; 4], &str)] = &[
    (b"ACCL", "accelerometer"),
    (b"GYRO", "gyroscope"),
    (b"MAGN", "magnetometer"),
    (b"GRAV", "gravity"),
    (b"CORI", "camera orientation"),
    (b"IORI", "image orientation"),
    (b"GPS5", "gps"),
    (b"GPS9", "gps"),
    (b"ISOE", "iso"),
    (b"SHUT", "shutter"),
    (b"WBAL", "white balance"),
    (b"FACE", "faces"),
    (b"SCEN", "scene"),
    (b"HUES", "hues"),
    (b"YAVG", "luma"),
    (b"MWET", "microphone wet"),
    (b"WNDM", "wind"),
    (b"AALP", "audio levels"),
];

/// Read one telemetry sample (a DEVC tree).
fn read_sample(data: &[u8], depth: usize, t: &mut Telemetry) {
    if depth > MAX_DEPTH {
        return;
    }
    let list = records(data);
    // Within a stream, SCAL and GPSF/GPSU apply to the data record that follows.
    let mut scale: Vec<f64> = Vec::new();
    let mut fix: Option<f64> = None;
    for klv in &list {
        match (&klv.key, klv.kind) {
            (_, 0) => read_sample(klv.data, depth + 1, t),
            (b"DVNM", b'c') => {
                if t.device.is_none() {
                    t.device = Some(text(klv)).filter(|s| !s.is_empty());
                }
            }
            // One scale per field, as repeats or as one struct.
            (b"SCAL", kind) => scale = numbers(klv, &vec![kind; (klv.size / width(kind).max(1)).max(1)]).into_iter().flatten().collect(),
            (b"GPSF", _) => fix = numbers(klv, &[klv.kind]).first().and_then(|r| r.first()).copied(),
            (b"GPSU", _) => {
                if let Some(time) = gps_time(&text(klv)) {
                    t.first_time.get_or_insert_with(|| time.clone());
                    t.last_time = Some(time);
                }
            }
            (b"GPS5", b'l') | (b"GPS9", _) => {
                if fix.is_some_and(|f| f < 2.0) {
                    continue;
                }
                let gps9 = &klv.key == b"GPS9";
                // GPS9: lat, lon, alt, 2D speed, 3D speed, days, seconds (all 'l'), DOP, fix ('S').
                let kinds: &[u8] = if gps9 { b"lllllllSS" } else { b"lllll" };
                let rows = numbers(klv, kinds);
                // One point per sample is plenty (GPS logs at 10-18 Hz).
                if let Some(row) = rows.iter().find(|r| r.len() >= 4 && (!gps9 || r.get(8).is_some_and(|&f| f >= 2.0))) {
                    let s = |i: usize| scale.get(i).or(scale.first()).copied().filter(|&v| v != 0.0).unwrap_or(1.0);
                    let point = (row[0] / s(0), row[1] / s(1), row[2] / s(2), row[3] / s(3));
                    if point.0.abs() <= 90.0 && point.1.abs() <= 180.0 && (point.0 != 0.0 || point.1 != 0.0) {
                        t.points.push(point);
                    }
                }
            }
            _ => {}
        }
        if let Some((_, name)) = SENSORS.iter().find(|(key, _)| *key == &klv.key) {
            if !t.sensors.iter().any(|s| s == name) {
                t.sensors.push(name.to_string());
            }
        }
    }
}

fn width(kind: u8) -> usize {
    match kind {
        b'b' | b'B' => 1,
        b's' | b'S' => 2,
        b'l' | b'L' | b'f' => 4,
        b'd' => 8,
        _ => 0,
    }
}

/// Great-circle distance in metres.
fn haversine(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (la1, la2) = (a.0.to_radians(), b.0.to_radians());
    let (dla, dlo) = ((b.0 - a.0).to_radians(), (b.1 - a.1).to_radians());
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * h.sqrt().asin()
}

fn round(v: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (v * f).round() / f
}

/// The telemetry samples of a `gpmd` track.
fn telemetry(ctx: &mut Ctx) -> anyhow::Result<Option<Telemetry>> {
    let (Some(moov), _) = video::top_boxes(ctx.source)? else { return Ok(None) };
    for (kind, trak) in video::children(&moov) {
        if &kind != b"trak" {
            continue;
        }
        let Some(stbl) = video::path(trak, &[b"mdia", b"minf", b"stbl"]) else { continue };
        let Some(stsd) = video::child(stbl, b"stsd") else { continue };
        if !video::children(stsd.get(8..).unwrap_or_default()).iter().any(|(k, _)| k == b"gpmd") {
            continue;
        }
        let count = video::child(stbl, b"stsz").and_then(|b| video::u32_at(b, 8)).unwrap_or(0) as usize;
        let mut t = Telemetry::default();
        // Evenly spaced samples when there are more than MAX_SAMPLES.
        let step = count.div_ceil(MAX_SAMPLES).max(1);
        for sample in (0..count).step_by(step) {
            let Some((offset, size)) = video::sample_location(stbl, sample) else { break };
            if size == 0 || size > MAX_SAMPLE {
                continue;
            }
            let data = ctx.source.range(offset, size as usize)?;
            read_sample(&data, 0, &mut t);
        }
        return Ok(Some(t));
    }
    Ok(None)
}

pub(crate) fn gpmf(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if !video::TYPES.contains(&ctx.content_type.as_str()) || ctx.head.get(4..8) != Some(b"ftyp") {
        return Ok(false);
    }
    let Some(t) = telemetry(ctx)? else { return Ok(false) };
    let mut raw = Map::new();
    if let Some(device) = &t.device {
        raw.insert("device".into(), json!(device));
        ctx.norm_in("camera", "make", json!("GoPro"));
        ctx.norm_in("camera", "model", json!(device));
    }
    raw.insert("sensors".into(), json!(t.sensors));
    if let Some(first) = t.points.first() {
        let mut point = json!({ "lat": round(first.0, 7), "lon": round(first.1, 7) });
        point["alt"] = json!(round(first.2, 2));
        ctx.norm("gps", point);
        let distance: f64 = t.points.windows(2).map(|w| haversine((w[0].0, w[0].1), (w[1].0, w[1].1))).sum();
        let max_speed = t.points.iter().map(|p| p.3).fold(0.0, f64::max);
        let (min_alt, max_alt) = t.points.iter().fold((f64::MAX, f64::MIN), |(lo, hi), p| (lo.min(p.2), hi.max(p.2)));
        let step = t.points.len().div_ceil(MAX_TRACK_POINTS).max(1);
        let track: Vec<Value> = t.points.iter().step_by(step).map(|p| json!([round(p.0, 6), round(p.1, 6), round(p.2, 1)])).collect();
        raw.insert(
            "gps".into(),
            json!({
                "points": t.points.len(),
                "start": t.first_time,
                "end": t.last_time,
                "distance_m": round(distance, 1),
                "max_speed_mps": round(max_speed, 2),
                "min_altitude_m": round(min_alt, 1),
                "max_altitude_m": round(max_alt, 1),
                "track": track,
            }),
        );
    }
    if let Some(start) = &t.first_time {
        ctx.norm("taken_at", json!(start));
    }
    let found = t.device.is_some() || !t.sensors.is_empty();
    ctx.raw("gpmf", raw);
    Ok(found)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn klv(key: &[u8; 4], kind: u8, size: u8, repeat: u16, data: &[u8]) -> Vec<u8> {
        let mut out = key.to_vec();
        out.extend([kind, size]);
        out.extend(repeat.to_be_bytes());
        out.extend(data);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        out
    }

    /// One second of telemetry: a device with an accelerometer stream and a GPS5 stream.
    pub(crate) fn sample(lat: f64, lon: f64, time: &str) -> Vec<u8> {
        let gps: Vec<u8> = [lat * 1e7, lon * 1e7, 123_000.0, 5_500.0, 5_600.0].iter().flat_map(|&v| (v as i32).to_be_bytes()).collect();
        let scale: Vec<u8> = [10_000_000i32, 10_000_000, 1000, 1000, 100].iter().flat_map(|v| v.to_be_bytes()).collect();
        let gps_stream = [
            klv(b"STNM", b'c', 13, 1, b"GPS (Lat. Lon"),
            klv(b"GPSF", b'L', 4, 1, &3u32.to_be_bytes()),
            klv(b"GPSU", b'U', 16, 1, time.as_bytes()),
            klv(b"SCAL", b'l', 4, 5, &scale),
            klv(b"GPS5", b'l', 20, 1, &gps),
        ]
        .concat();
        let accl_stream = [klv(b"SCAL", b's', 2, 1, &418i16.to_be_bytes()), klv(b"ACCL", b's', 6, 2, &[0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6])].concat();
        let devc = [klv(b"DVID", b'L', 4, 1, &1u32.to_be_bytes()), klv(b"DVNM", b'c', 11, 1, b"HERO9 Black"), klv(b"STRM", 0, 1, accl_stream.len() as u16, &accl_stream), klv(b"STRM", 0, 1, gps_stream.len() as u16, &gps_stream)].concat();
        klv(b"DEVC", 0, 1, devc.len() as u16, &devc)
    }

    #[test]
    fn telemetry_samples_are_read() {
        let mut t = Telemetry::default();
        read_sample(&sample(47.6062, -122.3321, "240315093000.000"), 0, &mut t);
        read_sample(&sample(47.6072, -122.3321, "240315093001.000"), 0, &mut t);
        assert_eq!(t.device.as_deref(), Some("HERO9 Black"));
        assert_eq!(t.sensors, ["accelerometer", "gps"]);
        assert_eq!(t.points.len(), 2);
        assert!((t.points[0].0 - 47.6062).abs() < 1e-6 && (t.points[0].1 + 122.3321).abs() < 1e-6);
        assert!((t.points[0].2 - 123.0).abs() < 1e-9, "altitude in metres");
        assert!((t.points[0].3 - 5.5).abs() < 1e-9, "speed in m/s");
        assert_eq!(t.first_time.as_deref(), Some("2024-03-15T09:30:00.000Z"));
        assert!((haversine((t.points[0].0, t.points[0].1), (t.points[1].0, t.points[1].1)) - 111.2).abs() < 1.0);
    }

    #[test]
    fn hostile_records_are_bounded() {
        // A size that runs off the end, and a deeply nested container.
        let mut t = Telemetry::default();
        read_sample(&[b'G', b'P', b'S', b'5', b'l', 20, 0xFF, 0xFF, 1, 2, 3], 0, &mut t);
        let mut nested = klv(b"ACCL", b's', 2, 1, &[0, 1]);
        for _ in 0..50 {
            nested = klv(b"STRM", 0, 1, nested.len() as u16, &nested);
        }
        read_sample(&nested, 0, &mut t);
        assert!(t.points.is_empty());
        assert!(t.sensors.is_empty(), "past the depth limit");
    }
}
