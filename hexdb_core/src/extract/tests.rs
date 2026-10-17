// Extraction tests: small synthetic files of each supported format, built
// byte by byte (or with the format's own crate), checked for both the
// normalized fields and the raw tag families.

use super::*;
use std::io::Write;

fn run(bytes: Vec<u8>, name: &str, content_type: &str) -> Extracted {
    let out = extract_bytes(bytes, name, content_type);
    eprintln!("{} -> {}\nerrors: {:?}", name, serde_json::to_string_pretty(&out.meta).unwrap(), out.errors);
    out
}

fn get<'a>(meta: &'a Map<String, Value>, path: &str) -> &'a Value {
    let mut value = meta.get(path.split('.').next().unwrap()).unwrap_or(&Value::Null);
    for part in path.split('.').skip(1) {
        value = value.get(part).unwrap_or(&Value::Null);
    }
    value
}

fn keywords(meta: &Map<String, Value>) -> Vec<String> {
    meta.get("keywords").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0xFF, marker];
    out.extend(((payload.len() + 2) as u16).to_be_bytes());
    out.extend(payload);
    out
}

fn tiff_entry(tag: u16, kind: u16, count: u32, value: &[u8], data: &mut Vec<u8>, data_at: u32) -> Vec<u8> {
    let mut e = Vec::new();
    e.extend(tag.to_be_bytes());
    e.extend(kind.to_be_bytes());
    e.extend(count.to_be_bytes());
    if value.len() <= 4 {
        let mut v = value.to_vec();
        v.resize(4, 0);
        e.extend(v);
    } else {
        e.extend((data_at + data.len() as u32).to_be_bytes());
        data.extend(value);
        if data.len() % 2 == 1 {
            data.push(0);
        }
    }
    e
}

fn ifd(entries: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = (entries.len() as u16).to_be_bytes().to_vec();
    for e in entries {
        out.extend(e);
    }
    out.extend([0, 0, 0, 0]);
    out
}

fn rationals(values: &[(u32, u32)]) -> Vec<u8> {
    values.iter().flat_map(|(n, d)| n.to_be_bytes().into_iter().chain(d.to_be_bytes())).collect()
}

/// A big-endian TIFF with Make, Model, DateTimeOriginal and GPS.
fn exif_tiff() -> Vec<u8> {
    // IFD0 at 8 (4 entries: 54 bytes), Exif IFD at 62 (18), GPS IFD at 80 (54), data at 134.
    let data_at = 134;
    let mut data = Vec::new();
    let ifd0 = ifd(vec![
        tiff_entry(0x010F, 2, 6, b"Canon\0", &mut data, data_at),
        tiff_entry(0x0110, 2, 7, b"EOS R5\0", &mut data, data_at),
        tiff_entry(0x8769, 4, 1, &62u32.to_be_bytes(), &mut data, data_at),
        tiff_entry(0x8825, 4, 1, &80u32.to_be_bytes(), &mut data, data_at),
    ]);
    let exif_ifd = ifd(vec![tiff_entry(0x9003, 2, 20, b"2024:06:01 12:30:45\0", &mut data, data_at)]);
    let gps_ifd = ifd(vec![
        tiff_entry(0x0001, 2, 2, b"N\0", &mut data, data_at),
        tiff_entry(0x0002, 5, 3, &rationals(&[(64, 1), (8, 1), (30, 1)]), &mut data, data_at),
        tiff_entry(0x0003, 2, 2, b"W\0", &mut data, data_at),
        tiff_entry(0x0004, 5, 3, &rationals(&[(21, 1), (56, 1), (24, 1)]), &mut data, data_at),
    ]);
    let mut tiff = b"MM\0\x2a\0\0\0\x08".to_vec();
    tiff.extend(ifd0);
    assert_eq!(tiff.len(), 62);
    tiff.extend(exif_ifd);
    assert_eq!(tiff.len(), 80);
    tiff.extend(gps_ifd);
    assert_eq!(tiff.len(), 134);
    tiff.extend(data);
    tiff
}

fn iim(datasets: &[(u8, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (dataset, value) in datasets {
        out.extend([0x1C, 2, *dataset]);
        out.extend((value.len() as u16).to_be_bytes());
        out.extend(value.as_bytes());
    }
    out
}

fn irb(id: u16, data: &[u8]) -> Vec<u8> {
    let mut out = b"8BIM".to_vec();
    out.extend(id.to_be_bytes());
    out.extend([0, 0]); // empty Pascal name, padded
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
    out
}

const XMP: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="4"><dc:title><rdf:Alt><rdf:li xml:lang="x-default">Glacier Lagoon</rdf:li></rdf:Alt></dc:title><dc:subject><rdf:Bag><rdf:li>ice</rdf:li><rdf:li>travel</rdf:li></rdf:Bag></dc:subject></rdf:Description></rdf:RDF></x:xmpmeta>"#;

fn sof(width: u16, height: u16) -> Vec<u8> {
    let mut payload = vec![8];
    payload.extend(height.to_be_bytes());
    payload.extend(width.to_be_bytes());
    payload.extend([3, 1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1]);
    segment(0xC0, &payload)
}

fn jpeg() -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8];
    out.extend(segment(0xE1, &[b"Exif\0\0".to_vec(), exif_tiff()].concat()));
    out.extend(segment(0xE1, &[b"http://ns.adobe.com/xap/1.0/\0".to_vec(), XMP.as_bytes().to_vec()].concat()));
    let iptc = iim(&[(5, "Lagoon"), (25, "iceland"), (25, "glacier"), (80, "Ada"), (90, "Höfn"), (116, "© Ada")]);
    out.extend(segment(0xED, &[b"Photoshop 3.0\0".to_vec(), irb(0x0404, &iptc)].concat()));
    out.extend(sof(640, 480));
    out.extend([0xFF, 0xD9]);
    out
}

fn riff_chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
    out
}

fn padded(text: &str, len: usize) -> Vec<u8> {
    let mut v = text.as_bytes().to_vec();
    v.resize(len, 0);
    v
}

fn wav() -> Vec<u8> {
    let mut fmt = Vec::new();
    fmt.extend(1u16.to_le_bytes());
    fmt.extend(2u16.to_le_bytes());
    fmt.extend(44100u32.to_le_bytes());
    fmt.extend(176_400u32.to_le_bytes());
    fmt.extend(4u16.to_le_bytes());
    fmt.extend(16u16.to_le_bytes());
    let mut bext = Vec::new();
    bext.extend(padded("Scene 12 take 3", 256));
    bext.extend(padded("ZOOM F8n", 32));
    bext.extend(padded("REF-0001", 32));
    bext.extend(padded("2024-05-02", 10));
    bext.extend(padded("14:03:00", 8));
    bext.extend(48_000u32.to_le_bytes());
    bext.extend(0u32.to_le_bytes());
    bext.extend(1u16.to_le_bytes());
    bext.resize(602, 0);
    bext.extend(b"A=PCM,F=48000,W=24\r\n");
    let ixml = br#"<?xml version="1.0"?><BWFXML><PROJECT>Night Shoot</PROJECT><SCENE>12</SCENE><TAKE>3</TAKE><NOTE>wind noise</NOTE><TRACK_LIST><TRACK><NAME>Boom</NAME></TRACK><TRACK><NAME>Lav</NAME></TRACK></TRACK_LIST></BWFXML>"#;
    let mut info = b"INFO".to_vec();
    info.extend(riff_chunk(b"INAM", b"Take Five\0"));
    info.extend(riff_chunk(b"IART", b"Quartet\0"));
    let mut body = b"WAVE".to_vec();
    body.extend(riff_chunk(b"fmt ", &fmt));
    body.extend(riff_chunk(b"bext", &bext));
    body.extend(riff_chunk(b"iXML", ixml));
    body.extend(riff_chunk(b"LIST", &info));
    body.extend(riff_chunk(b"data", &vec![0u8; 176_400]));
    let mut out = b"RIFF".to_vec();
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

fn flac() -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    out.extend([0x00, 0, 0, 34]);
    out.extend(4096u16.to_be_bytes());
    out.extend(4096u16.to_be_bytes());
    out.extend([0, 0, 0, 0, 0, 0]);
    let packed: u64 = (44_100u64 << 44) | (1 << 41) | (15 << 36) | 441_000;
    out.extend(packed.to_be_bytes());
    out.extend([0u8; 16]);
    let mut comments = Vec::new();
    comments.extend(5u32.to_le_bytes());
    comments.extend(b"hexdb");
    let fields = ["TITLE=Blue in Green", "ARTIST=Miles Davis", "ALBUM=Kind of Blue", "GENRE=Jazz", "DATE=1959", "TRACKNUMBER=3"];
    comments.extend((fields.len() as u32).to_le_bytes());
    for f in fields {
        comments.extend((f.len() as u32).to_le_bytes());
        comments.extend(f.as_bytes());
    }
    let len = comments.len() as u32;
    out.extend([0x84, (len >> 16) as u8, (len >> 8) as u8, len as u8]);
    out.extend(comments);
    out.extend(vec![0u8; 2000]);
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = (data.len() as u32).to_be_bytes().to_vec();
    out.extend(kind);
    out.extend(data);
    out.extend(crc32(&[kind.as_slice(), data].concat()).to_be_bytes());
    out
}

fn png() -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = 32u32.to_be_bytes().to_vec();
    ihdr.extend(16u32.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    out.extend(png_chunk(b"IHDR", &ihdr));
    out.extend(png_chunk(b"tEXt", b"Title\0Sunset"));
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(b"Grace").unwrap();
    out.extend(png_chunk(b"zTXt", &[b"Author\0\0".to_vec(), z.finish().unwrap()].concat()));
    out.extend(png_chunk(b"iTXt", &[b"XML:com.adobe.xmp\0\0\0\0\0".to_vec(), XMP.as_bytes().to_vec()].concat()));
    out.extend(png_chunk(b"IEND", b""));
    out
}

fn zip_of(files: &[(&str, &str)]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, content) in files {
        zip.start_file(*name, zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn ebml(id: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.push(0x01);
    out.extend(&(data.len() as u64).to_be_bytes()[1..]);
    out.extend(data);
    out
}

fn webm() -> Vec<u8> {
    let mut out = ebml(&[0x1A, 0x45, 0xDF, 0xA3], &ebml(&[0x42, 0x82], b"webm"));
    let info = [
        ebml(&[0x2A, 0xD7, 0xB1], &1_000_000u32.to_be_bytes()),
        ebml(&[0x44, 0x89], &12_345f64.to_be_bytes()),
        ebml(&[0x7B, 0xA9], b"Holiday"),
        ebml(&[0x57, 0x41], b"HexDB Test"),
    ]
    .concat();
    let video = [ebml(&[0xB0], &1920u16.to_be_bytes()), ebml(&[0xBA], &1080u16.to_be_bytes())].concat();
    let entry = [ebml(&[0x83], &[1]), ebml(&[0x86], b"V_VP9"), ebml(&[0xE0], &video)].concat();
    let tags = ebml(
        &[0x73, 0x73],
        &[
            ebml(&[0x63, 0xC0], b""),
            ebml(&[0x67, 0xC8], &[ebml(&[0x45, 0xA3], b"ARTIST"), ebml(&[0x44, 0x87], b"Jo")].concat()),
            ebml(&[0x67, 0xC8], &[ebml(&[0x45, 0xA3], b"TITLE"), ebml(&[0x44, 0x87], b"Holiday Cut")].concat()),
        ]
        .concat(),
    );
    let segment = [
        ebml(&[0x15, 0x49, 0xA9, 0x66], &info),
        ebml(&[0x16, 0x54, 0xAE, 0x6B], &ebml(&[0xAE], &entry)),
        ebml(&[0x1F, 0x43, 0xB6, 0x75], &[0u8; 64]),
        ebml(&[0x12, 0x54, 0xC3, 0x67], &tags),
    ]
    .concat();
    out.extend(ebml(&[0x18, 0x53, 0x80, 0x67], &segment));
    out
}

fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16().chain(std::iter::once(0)).flat_map(|u| u.to_le_bytes()).collect()
}

fn wma() -> Vec<u8> {
    let (title, author) = (utf16z("Song"), utf16z("Band"));
    let mut content = Vec::new();
    for len in [title.len(), author.len(), 0, 0, 0] {
        content.extend((len as u16).to_le_bytes());
    }
    content.extend(&title);
    content.extend(&author);
    let mut object = media_guid(0x33).to_vec();
    object.extend(((24 + content.len()) as u64).to_le_bytes());
    object.extend(content);
    let mut out = media_guid(0x30).to_vec();
    out.extend(((30 + object.len()) as u64).to_le_bytes());
    out.extend(1u32.to_le_bytes());
    out.extend([1, 2]);
    out.extend(object);
    out
}

fn media_guid(first: u8) -> [u8; 16] {
    [first, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C]
}

fn dicom() -> Vec<u8> {
    let mut out = vec![0u8; 128];
    out.extend(b"DICM");
    let element = |group: u16, element: u16, vr: &[u8; 2], value: &[u8]| {
        let mut e = group.to_le_bytes().to_vec();
        e.extend(element.to_le_bytes());
        e.extend(vr);
        e.extend((value.len() as u16).to_le_bytes());
        e.extend(value);
        e
    };
    out.extend(element(0x0008, 0x0060, b"CS", b"MR"));
    out.extend(element(0x0008, 0x1030, b"LO", b"BRAIN MRI "));
    out.extend(element(0x0010, 0x0010, b"PN", b"DOE^JOHN"));
    out.extend(element(0x0028, 0x0010, b"US", &256u16.to_le_bytes()));
    out.extend(element(0x0028, 0x0011, b"US", &256u16.to_le_bytes()));
    out
}

fn psd() -> Vec<u8> {
    let mut out = b"8BPS".to_vec();
    out.extend(1u16.to_be_bytes());
    out.extend([0u8; 6]);
    out.extend(3u16.to_be_bytes());
    out.extend(100u32.to_be_bytes());
    out.extend(200u32.to_be_bytes());
    out.extend(8u16.to_be_bytes());
    out.extend(3u16.to_be_bytes());
    out.extend(0u32.to_be_bytes()); // color mode data
    let writer: Vec<u8> = "Adobe Photoshop".encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
    let mut version = 1u32.to_be_bytes().to_vec();
    version.push(1);
    version.extend(15u32.to_be_bytes());
    version.extend(&writer);
    version.extend(0u32.to_be_bytes());
    version.extend(1u32.to_be_bytes());
    let resources = [irb(0x0404, &iim(&[(5, "Poster")])), irb(0x0421, &version)].concat();
    out.extend((resources.len() as u32).to_be_bytes());
    out.extend(resources);
    out.extend(10u32.to_be_bytes()); // layer and mask section
    out.extend(6u32.to_be_bytes()); // layer info
    out.extend((-3i16).to_be_bytes());
    out.extend([0u8; 8]);
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn jpeg_exif_gps_iptc_xmp() {
    let out = run(jpeg(), "IMG_0042.jpg", "image/jpeg");
    let m = &out.meta;
    assert_eq!(get(m, "camera.make"), "Canon");
    assert_eq!(get(m, "camera.model"), "EOS R5");
    assert!(get(m, "taken_at").as_str().unwrap().starts_with("2024-06-01"), "{}", get(m, "taken_at"));
    assert!((get(m, "gps.lat").as_f64().unwrap() - 64.141_666_7).abs() < 1e-5);
    assert!((get(m, "gps.lon").as_f64().unwrap() + 21.94).abs() < 1e-5);
    assert_eq!(get(m, "width"), 640);
    assert_eq!(get(m, "height"), 480);
    assert_eq!(get(m, "title"), "Lagoon", "IPTC is read before XMP");
    assert_eq!(get(m, "location.city"), "Höfn");
    assert_eq!(get(m, "author"), "Ada");
    let words = keywords(m);
    for w in ["iceland", "glacier", "ice", "travel"] {
        assert!(words.contains(&w.to_string()), "{:?}", words);
    }
    assert_eq!(get(m, "exif.Make"), "Canon");
    assert_eq!(get(m, "xmp.dc:title"), "Glacier Lagoon");
    assert_eq!(get(m, "xmp.xmp:Rating"), "4");
    assert_eq!(get(m, "iptc.Keywords"), &json!(["iceland", "glacier"]));
    for e in ["exif", "iptc", "image", "xmp"] {
        assert!(out.extractors.contains(&e.to_string()), "{:?}", out.extractors);
    }
    assert!(out.errors.is_empty(), "{:?}", out.errors);
}

#[test]
fn wav_with_bext_ixml_and_riff_info() {
    let out = run(wav(), "take3.wav", "audio/x-wav");
    let m = &out.meta;
    assert_eq!(get(m, "duration_ms"), 1000);
    assert_eq!(get(m, "sample_rate"), 44100);
    assert_eq!(get(m, "channels"), 2);
    assert_eq!(get(m, "bit_depth"), 16);
    assert_eq!(get(m, "title"), "Take Five");
    assert_eq!(get(m, "artist"), "Quartet");
    assert_eq!(get(m, "description"), "Scene 12 take 3");
    assert_eq!(get(m, "recorded_at"), "2024-05-02T14:03:00");
    assert_eq!(get(m, "bext.originator"), "ZOOM F8n");
    assert_eq!(get(m, "bext.time_reference"), 48000);
    assert!(get(m, "bext.coding_history").as_str().unwrap().starts_with("A=PCM"));
    assert_eq!(get(m, "ixml.project"), "Night Shoot");
    assert_eq!(get(m, "ixml.tracks"), &json!(["Boom", "Lav"]));
    assert!(get(m, "riff_info").is_object());
}

#[test]
fn flac_vorbis_comments() {
    let out = run(flac(), "03 Blue in Green.flac", "audio/flac");
    let m = &out.meta;
    assert_eq!(get(m, "title"), "Blue in Green");
    assert_eq!(get(m, "artist"), "Miles Davis");
    assert_eq!(get(m, "album"), "Kind of Blue");
    assert_eq!(get(m, "genre"), "Jazz");
    assert_eq!(get(m, "year"), "1959");
    assert_eq!(get(m, "track"), "3");
    assert_eq!(get(m, "duration_ms"), 10_000);
    assert_eq!(get(m, "vorbis.TITLE"), "Blue in Green");
}

#[test]
fn png_text_and_xmp() {
    let out = run(png(), "sunset.png", "image/png");
    let m = &out.meta;
    assert_eq!(get(m, "title"), "Sunset");
    assert_eq!(get(m, "author"), "Grace");
    assert_eq!(get(m, "width"), 32);
    assert_eq!(get(m, "height"), 16);
    assert_eq!(get(m, "xmp.dc:title"), "Glacier Lagoon");
}

#[test]
fn word_document() {
    let bytes = zip_of(&[
        ("[Content_Types].xml", "<Types/>"),
        (
            "docProps/core.xml",
            r#"<cp:coreProperties xmlns:cp="c" xmlns:dc="d"><dc:title>Quarterly Report</dc:title><dc:creator>Ada Lovelace</dc:creator><cp:keywords>finance, q3</cp:keywords></cp:coreProperties>"#,
        ),
        ("docProps/app.xml", "<Properties><Pages>4</Pages><Words>1200</Words><Application>Microsoft Office Word</Application></Properties>"),
        (
            "word/document.xml",
            r#"<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>Revenue grew</w:t></w:r><w:r><w:t xml:space="preserve"> nine percent</w:t></w:r></w:p><w:p><w:r><w:t>Second paragraph</w:t></w:r></w:p></w:body></w:document>"#,
        ),
    ]);
    let out = run(bytes, "report.docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
    let m = &out.meta;
    assert_eq!(get(m, "title"), "Quarterly Report");
    assert_eq!(get(m, "author"), "Ada Lovelace");
    assert_eq!(keywords(m), vec!["finance", "q3"]);
    assert_eq!(get(m, "pages"), 4);
    assert_eq!(get(m, "words"), 1200);
    assert_eq!(get(m, "creator_tool"), "Microsoft Office Word");
    assert!(get(m, "text").as_str().unwrap().contains("Revenue grew nine percent\nSecond paragraph"));
}

#[test]
fn opendocument_and_epub() {
    let odt = zip_of(&[
        (
            "meta.xml",
            r#"<office:document-meta xmlns:office="o" xmlns:meta="m" xmlns:dc="d"><office:meta><dc:title>Minutes</dc:title><meta:initial-creator>Grace Hopper</meta:initial-creator><meta:keyword>meeting</meta:keyword><meta:document-statistic meta:page-count="2" meta:word-count="350"/></office:meta></office:document-meta>"#,
        ),
        ("content.xml", r#"<office:document-content xmlns:office="o" xmlns:text="t"><office:body><office:text><text:p>Agenda item one</text:p></office:text></office:body></office:document-content>"#),
    ]);
    let out = run(odt, "minutes.odt", "application/vnd.oasis.opendocument.text");
    assert_eq!(get(&out.meta, "title"), "Minutes");
    assert_eq!(get(&out.meta, "author"), "Grace Hopper");
    assert_eq!(get(&out.meta, "pages"), 2);
    assert_eq!(get(&out.meta, "words"), 350);
    assert!(get(&out.meta, "text").as_str().unwrap().contains("Agenda item one"));

    let epub = zip_of(&[
        ("mimetype", "application/epub+zip"),
        ("META-INF/container.xml", r#"<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#),
        (
            "OEBPS/content.opf",
            r#"<package><metadata xmlns:dc="d"><dc:title>Dune</dc:title><dc:creator>Frank Herbert</dc:creator><dc:language>en</dc:language><dc:subject>Science fiction</dc:subject></metadata></package>"#,
        ),
    ]);
    let out = run(epub, "dune.epub", "application/epub+zip");
    assert_eq!(get(&out.meta, "title"), "Dune");
    assert_eq!(get(&out.meta, "author"), "Frank Herbert");
    assert_eq!(get(&out.meta, "language"), "en");
    assert!(keywords(&out.meta).contains(&"Science fiction".to_string()));
}

#[test]
fn pdf_info_dictionary() {
    use lopdf::{dictionary, Object};
    let mut doc = lopdf::Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let page_id = doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()] });
    doc.objects.insert(pages_id, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1 }));
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    let info = doc.add_object(dictionary! {
        "Title" => Object::string_literal("Annual Report"),
        "Author" => Object::string_literal("Ada"),
        "Keywords" => Object::string_literal("finance; audit"),
        "Creator" => Object::string_literal("Adobe Illustrator 28.0"),
        "CreationDate" => Object::string_literal("D:20240315093000Z"),
    });
    doc.trailer.set("Root", catalog);
    doc.trailer.set("Info", info);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let out = run(bytes, "report.pdf", "application/pdf");
    let m = &out.meta;
    assert_eq!(get(m, "title"), "Annual Report");
    assert_eq!(get(m, "author"), "Ada");
    assert_eq!(get(m, "pages"), 1);
    assert_eq!(keywords(m), vec!["finance", "audit"]);
    assert_eq!(get(m, "pdf.CreationDate"), "2024-03-15T09:30:00Z");
    assert_eq!(get(m, "pdf.illustrator"), true);
}

#[test]
fn png_color_profile() {
    use std::io::Write;
    let profile = super::icc::tests::profile("Display P3");
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(&profile).unwrap();
    let data = [b"Display P3\0\0".to_vec(), z.finish().unwrap()].concat();
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend((data.len() as u32).to_be_bytes());
    png.extend(b"iCCP");
    png.extend(&data);
    png.extend([0u8; 4]);
    png.extend([0, 0, 0, 0]);
    png.extend(b"IEND");
    png.extend([0u8; 4]);
    let out = run(png, "wide.png", "image/png");
    assert_eq!(get(&out.meta, "color_profile"), "Display P3");
    assert_eq!(get(&out.meta, "icc.device_class"), "display");
}

#[test]
fn gopro_telemetry() {
    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        [((body.len() + 8) as u32).to_be_bytes().to_vec(), kind.to_vec(), body.to_vec()].concat()
    }
    fn full(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        mp4_box(kind, &[vec![0u8; 4], body.to_vec()].concat())
    }
    let samples = [super::gpmf::tests::sample(47.6062, -122.3321, "240315093000.000"), super::gpmf::tests::sample(47.6072, -122.3321, "240315093001.000")];
    let ftyp = mp4_box(b"ftyp", b"mp41\0\0\0\0mp41");
    let mdat_at = ftyp.len() as u32 + 8;
    let mdat = mp4_box(b"mdat", &samples.concat());
    let sizes: Vec<u8> = [0u32, 2, samples[0].len() as u32, samples[1].len() as u32].iter().flat_map(|v| v.to_be_bytes()).collect();
    let stbl = mp4_box(
        b"stbl",
        &[
            full(b"stsd", &[1u32.to_be_bytes().to_vec(), mp4_box(b"gpmd", &[0u8; 8])].concat()),
            full(b"stsz", &sizes),
            full(b"stsc", &[1u32, 1, 2, 1].iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<u8>>()),
            full(b"stco", &[1u32, mdat_at].iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<u8>>()),
        ]
        .concat(),
    );
    let hdlr = full(b"hdlr", &[0u32.to_be_bytes().to_vec(), b"meta".to_vec(), vec![0u8; 12], b"GoPro MET\0".to_vec()].concat());
    let moov = mp4_box(b"moov", &mp4_box(b"trak", &mp4_box(b"mdia", &[hdlr, mp4_box(b"minf", &stbl)].concat())));
    let out = run([ftyp, mdat, moov].concat(), "GX010001.MP4", "video/mp4");
    let m = &out.meta;
    assert_eq!(get(m, "gpmf.device"), "HERO9 Black");
    assert_eq!(get(m, "camera.model"), "HERO9 Black");
    assert_eq!(get(m, "gpmf.gps.points"), 2);
    assert_eq!(get(m, "gpmf.gps.start"), "2024-03-15T09:30:00.000Z");
    assert_eq!(get(m, "gps.lat"), 47.6062);
    let distance = get(m, "gpmf.gps.distance_m").as_f64().unwrap();
    assert!((distance - 111.2).abs() < 1.0, "{}", distance);
    assert_eq!(get(m, "gpmf.gps.track").as_array().map(Vec::len), Some(2));
}

#[test]
fn pdf_body_text() {
    use lopdf::{content::{Content, Operation}, dictionary, Object, Stream};
    let mut doc = lopdf::Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
    let resources_id = doc.add_object(dictionary! { "Font" => dictionary! { "F1" => font_id } });
    let mut kids = Vec::new();
    for line in ["Quarterly revenue grew", "beekeeping cooperative"] {
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![72.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal(line)]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages_id, "Contents" => content_id, "Resources" => resources_id, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()] });
        kids.push(page_id.into());
    }
    doc.objects.insert(pages_id, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => 2 }));
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let out = run(bytes, "report.pdf", "application/pdf");
    let text = get(&out.meta, "text").as_str().unwrap_or_default().to_string();
    assert!(text.contains("Quarterly revenue grew"), "{}", text);
    assert!(text.contains("beekeeping cooperative"), "both pages: {}", text);
    assert_eq!(get(&out.meta, "pdf.text_pages"), 2);
}

#[test]
fn matroska_info_tracks_and_tags() {
    let out = run(webm(), "holiday.webm", "video/webm");
    let m = &out.meta;
    assert_eq!(get(m, "duration_ms"), 12_345);
    assert_eq!(get(m, "width"), 1920);
    assert_eq!(get(m, "height"), 1080);
    assert_eq!(get(m, "codec"), "V_VP9");
    assert_eq!(get(m, "title"), "Holiday Cut");
    assert_eq!(get(m, "artist"), "Jo");
    assert_eq!(get(m, "matroska.title"), "Holiday");
    assert_eq!(get(m, "matroska.tags.ARTIST"), "Jo");
    assert_eq!(get(m, "creator_tool"), "HexDB Test");
}

#[test]
fn asf_mpeg7_svg_tar_dicom_psd_markdown() {
    let out = run(wma(), "song.wma", "audio/x-ms-wma");
    assert_eq!(get(&out.meta, "title"), "Song");
    assert_eq!(get(&out.meta, "artist"), "Band");

    let mpeg7 = r#"<?xml version="1.0"?><Mpeg7 xmlns="urn:mpeg:mpeg7:schema:2001"><Description><CreationInformation><Creation><Title>Evening News</Title><Abstract><FreeTextAnnotation>Top stories</FreeTextAnnotation></Abstract><Creator><Agent><Name><GivenName>Jane</GivenName><FamilyName>Doe</FamilyName></Name></Agent></Creator></Creation></CreationInformation><KeywordAnnotation><Keyword>news</Keyword><Keyword>weather</Keyword></KeywordAnnotation></Description></Mpeg7>"#;
    let out = run(mpeg7.as_bytes().to_vec(), "news.xml", "application/xml");
    assert_eq!(get(&out.meta, "title"), "Evening News");
    assert_eq!(get(&out.meta, "author"), "Jane Doe");
    assert_eq!(get(&out.meta, "description"), "Top stories");
    assert_eq!(keywords(&out.meta), vec!["news", "weather"]);

    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="200px" height="100"><title>Logo</title><desc>Company mark</desc><text x="1" y="2">HexDB</text></svg>"#;
    let out = run(svg.as_bytes().to_vec(), "logo.svg", "image/svg+xml");
    assert_eq!(get(&out.meta, "title"), "Logo");
    assert_eq!(get(&out.meta, "description"), "Company mark");
    assert_eq!(get(&out.meta, "width"), 200);
    assert!(get(&out.meta, "text").as_str().unwrap().contains("HexDB"));

    let mut tar = tar::Builder::new(Vec::new());
    for (path, data) in [("docs/readme.txt", &b"hello"[..]), ("docs/notes.md", b"# notes")] {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, path, data).unwrap();
    }
    let out = run(tar.into_inner().unwrap(), "bundle.tar", "application/x-tar");
    assert_eq!(get(&out.meta, "archive.entries"), 2);
    assert_eq!(get(&out.meta, "archive.names"), &json!(["docs/readme.txt", "docs/notes.md"]));

    let out = run(dicom(), "scan.dcm", "application/dicom");
    assert_eq!(get(&out.meta, "dicom.Modality"), "MR");
    assert_eq!(get(&out.meta, "title"), "BRAIN MRI");
    assert_eq!(get(&out.meta, "width"), 256);
    assert!(!serde_json::to_string(&out.meta).unwrap().contains("DOE"), "patient names are never indexed");

    let out = run(psd(), "poster.psd", "image/vnd.adobe.photoshop");
    assert_eq!(get(&out.meta, "width"), 200);
    assert_eq!(get(&out.meta, "height"), 100);
    assert_eq!(get(&out.meta, "color_mode"), "RGB");
    assert_eq!(get(&out.meta, "layers"), 3);
    assert_eq!(get(&out.meta, "creator_tool"), "Adobe Photoshop");
    assert_eq!(get(&out.meta, "title"), "Poster");

    let out = run(b"# Release notes\n\nHexDB stores files.\n".to_vec(), "notes.md", "text/markdown");
    assert_eq!(get(&out.meta, "title"), "Release notes");
    assert_eq!(get(&out.meta, "lines"), 3);
    assert!(get(&out.meta, "text").as_str().unwrap().contains("HexDB stores files"));
}

#[test]
fn c2pa_manifests_are_noticed() {
    let mut payload = b"JP\0\0\0\0\0\0jumbjumdc2pa\0claim_generator".to_vec();
    payload.push(0x60 | 11);
    payload.extend(b"TestCam/1.0");
    let mut bytes = vec![0xFF, 0xD8];
    bytes.extend(segment(0xEB, &payload));
    bytes.extend(sof(10, 10));
    bytes.extend([0xFF, 0xD9]);
    let out = run(bytes, "signed.jpg", "image/jpeg");
    assert_eq!(get(&out.meta, "c2pa.present"), true);
    assert_eq!(get(&out.meta, "c2pa.claim_generator"), "TestCam/1.0");
}

#[test]
fn hostile_and_broken_files_never_panic() {
    let mut x = 99u64;
    let noise: Vec<u8> = (0..200_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    for (name, kind) in [("x.jpg", "image/jpeg"), ("x.wav", "audio/x-wav"), ("x.pdf", "application/pdf"), ("x.docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"), ("x.mkv", "video/x-matroska"), ("x.zip", "application/zip")] {
        let _ = run(noise.clone(), name, kind);
    }
    // Truncated versions of real fixtures.
    for (bytes, name, kind) in [(jpeg(), "t.jpg", "image/jpeg"), (wav(), "t.wav", "audio/x-wav"), (webm(), "t.webm", "video/webm"), (psd(), "t.psd", "image/vnd.adobe.photoshop"), (png(), "t.png", "image/png"), (dicom(), "t.dcm", "application/dicom")] {
        for cut in [10, 100, bytes.len() / 2, bytes.len() - 1] {
            let _ = run(bytes[..cut.min(bytes.len())].to_vec(), name, kind);
        }
    }
    // Magic bytes that lie about the format.
    let _ = run([b"8BPS".to_vec(), vec![0xFF; 100]].concat(), "lie.psd", "image/vnd.adobe.photoshop");
    let _ = run([b"%PDF-1.7\n".to_vec(), noise[..1000].to_vec()].concat(), "lie.pdf", "application/pdf");
    let _ = run([vec![0x1A, 0x45, 0xDF, 0xA3, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]].concat(), "lie.mkv", "video/x-matroska");
}

#[test]
fn limits_are_kept() {
    // A budget too small for the file: extraction stops with errors, not a hang or a panic.
    let limits = Limits { max_read_bytes: 300_000, timeout: std::time::Duration::from_secs(5), max_text_bytes: 100 };
    let mut source = Source::new(Box::new(wav()), &limits);
    let out = extract(&mut source, "big.wav", "audio/x-wav", &limits);
    assert!(!out.errors.is_empty() || out.meta.len() < 20, "{:?}", out);

    // Long values are cut, text is capped.
    let long = "x".repeat(10_000);
    let xmp = format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="r"><rdf:Description xmlns:dc="d"><dc:description>{}</dc:description></rdf:Description></rdf:RDF></x:xmpmeta>"#,
        long
    );
    let out = extract(&mut Source::new(Box::new(xmp.clone().into_bytes()), &limits), "a.txt", "text/plain", &limits);
    assert_eq!(get(&out.meta, "description").as_str().unwrap().len(), 4096);
    assert!(get(&out.meta, "text").as_str().unwrap().len() <= 100);
}

fn syncsafe(n: u32) -> [u8; 4] {
    [((n >> 21) & 0x7F) as u8, ((n >> 14) & 0x7F) as u8, ((n >> 7) & 0x7F) as u8, (n & 0x7F) as u8]
}

/// ID3v2.4 tag followed by MPEG-1 Layer III frames (128 kbps, 44.1 kHz).
fn mp3() -> Vec<u8> {
    let mut frames = Vec::new();
    for (id, text) in [(b"TIT2", "Giant Steps"), (b"TPE1", "John Coltrane"), (b"TALB", "Giant Steps"), (b"TCON", "Jazz"), (b"TRCK", "1")] {
        let mut body = vec![3u8]; // UTF-8
        body.extend(text.as_bytes());
        frames.extend(id);
        frames.extend(syncsafe(body.len() as u32));
        frames.extend([0, 0]);
        frames.extend(body);
    }
    let mut out = b"ID3\x04\x00\x00".to_vec();
    out.extend(syncsafe(frames.len() as u32));
    out.extend(frames);
    for _ in 0..40 {
        let mut frame = vec![0xFF, 0xFB, 0x90, 0x64];
        frame.resize(417, 0);
        out.extend(frame);
    }
    out
}

#[test]
fn mp3_id3v2() {
    let out = run(mp3(), "01 Giant Steps.mp3", "audio/mpeg");
    let m = &out.meta;
    assert_eq!(get(m, "title"), "Giant Steps");
    assert_eq!(get(m, "artist"), "John Coltrane");
    assert_eq!(get(m, "album"), "Giant Steps");
    assert_eq!(get(m, "genre"), "Jazz");
    assert_eq!(get(m, "id3v2.TPE1"), "John Coltrane");
    assert_eq!(get(m, "sample_rate"), 44100);
    assert!(get(m, "duration_ms").as_u64().unwrap() > 900, "{}", get(m, "duration_ms"));
}
