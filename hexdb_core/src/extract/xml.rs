// XML-based metadata: XMP packets (in almost any format), MPEG-7
// descriptions, iXML (field recorders), SVG, and a small DOM the other
// extractors use for office and e-book formats.

use super::{decode_text, Ctx};
use quick_xml::events::Event;
use serde_json::{json, Map, Value};

/// Elements parsed at most per document (entity bombs and huge files stop here).
const MAX_NODES: usize = 50_000;

/// A parsed XML element.
#[derive(Debug, Default, Clone)]
pub(crate) struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
    pub text: String,
}

impl Node {
    pub fn local(&self) -> &str {
        local(&self.name)
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name || local(k) == name).map(|(_, v)| v.as_str())
    }

    /// Every descendant (and self) with this local name.
    pub fn find_all<'a>(&'a self, local_name: &str, out: &mut Vec<&'a Node>) {
        if self.local() == local_name {
            out.push(self);
        }
        for child in &self.children {
            child.find_all(local_name, out);
        }
    }

    pub fn find(&self, local_name: &str) -> Option<&Node> {
        if self.local() == local_name {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(local_name))
    }

    /// All text inside, in order, separated by `sep` between elements.
    pub fn all_text(&self, sep: &str, out: &mut String) {
        if !self.text.trim().is_empty() {
            if !out.is_empty() {
                out.push_str(sep);
            }
            out.push_str(self.text.trim());
        }
        for child in &self.children {
            child.all_text(sep, out);
        }
    }

    pub fn text_of(&self) -> String {
        let mut out = String::new();
        self.all_text(" ", &mut out);
        out
    }
}

pub(crate) fn local(name: &str) -> &str {
    name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name)
}

/// Parse XML into a tree; `None` if it isn't well formed enough to have a root.
pub(crate) fn parse(bytes: &[u8]) -> Option<Node> {
    let text = decode_text(bytes);
    let mut reader = quick_xml::Reader::from_str(&text);
    reader.config_mut().trim_text(false);
    let mut stack: Vec<Node> = vec![Node { name: "#document".into(), ..Default::default() }];
    let mut count = 0usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                count += 1;
                if count > MAX_NODES {
                    break;
                }
                stack.push(element(&e));
            }
            Ok(Event::Empty(e)) => {
                count += 1;
                if count > MAX_NODES {
                    break;
                }
                let node = element(&e);
                stack.last_mut()?.children.push(node);
            }
            Ok(Event::End(_)) => {
                if stack.len() > 1 {
                    let done = stack.pop()?;
                    stack.last_mut()?.children.push(done);
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(top) = stack.last_mut() {
                    let piece = t.unescape().map(|c| c.into_owned()).unwrap_or_else(|_| String::from_utf8_lossy(&t).into_owned());
                    if top.text.len() < 1 << 20 {
                        top.text.push_str(&piece);
                    }
                }
            }
            Ok(Event::CData(t)) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    // Close anything left open (truncated input).
    while stack.len() > 1 {
        let done = stack.pop()?;
        stack.last_mut()?.children.push(done);
    }
    let document = stack.pop()?;
    document.children.into_iter().next()
}

fn element(e: &quick_xml::events::BytesStart) -> Node {
    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let attrs = e
        .attributes()
        .flatten()
        .map(|a| {
            let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
            let value = a.unescape_value().map(|v| v.into_owned()).unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
            (key, value)
        })
        .collect();
    Node { name, attrs, ..Default::default() }
}

/// Find `<open ...> ... </close>` in bytes; returns the slice including both.
pub(crate) fn find_block<'a>(bytes: &'a [u8], open: &[u8], close: &[u8]) -> Option<&'a [u8]> {
    let start = memfind(bytes, open)?;
    let end = memfind(&bytes[start..], close)? + start + close.len();
    Some(&bytes[start..end])
}

pub(crate) fn memfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Where packets may be: the start, the end, and for files up to 16 MB, everywhere.
fn scan_windows(ctx: &mut Ctx) -> Vec<Vec<u8>> {
    let len = ctx.source.len();
    if len <= 16 * 1024 * 1024 && len > ctx.head.len() as u64 {
        if let Ok(all) = ctx.source.all(16 * 1024 * 1024) {
            return vec![all];
        }
    }
    let mut windows = vec![ctx.head.clone()];
    if len > ctx.head.len() as u64 {
        if let Ok(tail) = ctx.source.tail(2 * 1024 * 1024) {
            windows.push(tail);
        }
    }
    windows
}

// ---------------------------------------------------------------------------
// XMP
// ---------------------------------------------------------------------------

/// The value of an XMP property element.
fn xmp_value(node: &Node) -> Value {
    for child in &node.children {
        match child.name.as_str() {
            "rdf:Seq" | "rdf:Bag" => {
                return Value::Array(child.children.iter().filter(|li| li.local() == "li").map(xmp_item).collect());
            }
            "rdf:Alt" => {
                let items: Vec<&Node> = child.children.iter().filter(|li| li.local() == "li").collect();
                let chosen = items.iter().find(|li| li.attr("xml:lang") == Some("x-default")).or(items.first());
                return chosen.map(|li| xmp_item(li)).unwrap_or(Value::Null);
            }
            "rdf:Description" => return Value::Object(xmp_props(child)),
            _ => {}
        }
    }
    if node.attr("rdf:parseType") == Some("Resource") || (!node.children.is_empty() && node.text.trim().is_empty()) {
        return Value::Object(xmp_props(node));
    }
    if let Some(resource) = node.attr("rdf:resource") {
        return json!(resource);
    }
    let props = xmp_attrs(node);
    if !props.is_empty() && node.text.trim().is_empty() {
        return Value::Object(props);
    }
    json!(node.text.trim())
}

fn xmp_item(li: &Node) -> Value {
    if li.children.is_empty() && xmp_attrs(li).is_empty() {
        return json!(li.text.trim());
    }
    xmp_value(li)
}

fn xmp_attrs(node: &Node) -> Map<String, Value> {
    node.attrs
        .iter()
        .filter(|(k, _)| !k.starts_with("xmlns") && !k.starts_with("rdf:") && !k.starts_with("xml:") && k.contains(':'))
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect()
}

fn xmp_props(desc: &Node) -> Map<String, Value> {
    let mut map = xmp_attrs(desc);
    for child in &desc.children {
        if child.name.starts_with("rdf:") {
            continue;
        }
        map.insert(child.name.clone(), xmp_value(child));
    }
    map
}

/// Every property of every `rdf:Description` in a packet.
pub(crate) fn xmp_from_dom(root: &Node) -> Map<String, Value> {
    let mut descriptions = Vec::new();
    root.find_all("Description", &mut descriptions);
    let mut map = Map::new();
    for desc in descriptions.into_iter().filter(|d| d.name == "rdf:Description") {
        for (k, v) in xmp_props(desc) {
            map.entry(k).or_insert(v);
        }
    }
    map
}

pub(crate) fn xmp_packet(bytes: &[u8]) -> Option<Map<String, Value>> {
    let block = find_block(bytes, b"<x:xmpmeta", b"</x:xmpmeta>")
        .or_else(|| find_block(bytes, b"<x:xapmeta", b"</x:xapmeta>"))
        .or_else(|| find_block(bytes, b"<rdf:RDF", b"</rdf:RDF>"))?;
    let root = parse(block)?;
    let map = xmp_from_dom(&root);
    (!map.is_empty()).then_some(map)
}

fn first_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => items.iter().find_map(first_text),
        Value::Object(map) => map.values().find_map(first_text),
        _ => None,
    }
}

fn all_texts(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => s.split([',', ';']).map(|w| w.trim().to_string()).filter(|w| !w.is_empty()).collect(),
        Value::Array(items) => items.iter().flat_map(all_texts).collect(),
        _ => Vec::new(),
    }
}

/// XMP GPS coordinates: "64,8.5N" or "64,8,30N" (degrees, minutes[, seconds]) or decimal.
pub(crate) fn xmp_coordinate(text: &str) -> Option<f64> {
    let text = text.trim();
    let (body, sign) = match text.chars().last()? {
        'N' | 'E' => (&text[..text.len() - 1], 1.0),
        'S' | 'W' => (&text[..text.len() - 1], -1.0),
        _ => (text, 1.0),
    };
    let parts: Vec<f64> = body.split(',').map(|p| p.trim().parse::<f64>()).collect::<Result<_, _>>().ok()?;
    let value = match parts.as_slice() {
        [d] => *d,
        [d, m] => d + m / 60.0,
        [d, m, s] => d + m / 60.0 + s / 3600.0,
        _ => return None,
    };
    Some(value * sign)
}

pub(crate) fn xmp(ctx: &mut Ctx) -> anyhow::Result<bool> {
    let mut found = None;
    for window in scan_windows(ctx) {
        if let Some(map) = xmp_packet(&window) {
            found = Some(map);
            break;
        }
    }
    let Some(map) = found else { return Ok(false) };
    let get = |k: &str| map.get(k);
    if let Some(v) = get("dc:title").and_then(first_text) {
        ctx.norm("title", json!(v));
    }
    if let Some(v) = get("dc:description").and_then(first_text) {
        ctx.norm("description", json!(v));
    }
    if let Some(v) = get("dc:creator") {
        ctx.norm("author", json!(all_texts(v).join(", ")));
    }
    if let Some(v) = get("dc:rights").and_then(first_text) {
        ctx.norm("copyright", json!(v));
    }
    for key in ["dc:subject", "pdf:Keywords", "lr:hierarchicalSubject"] {
        if let Some(v) = get(key) {
            let words = all_texts(v);
            ctx.keywords(words);
        }
    }
    for (key, norm) in [("xmp:CreatorTool", "creator_tool"), ("xmp:Rating", "rating"), ("xmpDM:artist", "artist"), ("xmpDM:album", "album"), ("xmpDM:genre", "genre")] {
        if let Some(v) = get(key).and_then(first_text) {
            ctx.norm(norm, json!(v));
        }
    }
    for key in ["exif:DateTimeOriginal", "photoshop:DateCreated", "xmp:CreateDate"] {
        if let Some(v) = get(key).and_then(first_text) {
            ctx.norm("taken_at", json!(v));
            break;
        }
    }
    for (key, field) in [("tiff:Make", "make"), ("tiff:Model", "model"), ("aux:Lens", "lens"), ("exifEX:LensModel", "lens")] {
        if let Some(v) = get(key).and_then(first_text) {
            ctx.norm_in("camera", field, json!(v));
        }
    }
    for (key, field) in [("photoshop:City", "city"), ("photoshop:State", "state"), ("photoshop:Country", "country"), ("Iptc4xmpCore:Location", "sublocation")] {
        if let Some(v) = get(key).and_then(first_text) {
            ctx.norm_in("location", field, json!(v));
        }
    }
    if let (Some(lat), Some(lon)) = (
        get("exif:GPSLatitude").and_then(first_text).and_then(|t| xmp_coordinate(&t)),
        get("exif:GPSLongitude").and_then(first_text).and_then(|t| xmp_coordinate(&t)),
    ) {
        ctx.norm("gps", json!({ "lat": lat, "lon": lon }));
    }
    ctx.raw("xmp", map);
    Ok(true)
}

// ---------------------------------------------------------------------------
// MPEG-7
// ---------------------------------------------------------------------------

pub(crate) fn mpeg7(ctx: &mut Ctx) -> anyhow::Result<bool> {
    let mut root = None;
    for window in scan_windows(ctx) {
        if let Some(block) = find_block(&window, b"<Mpeg7", b"</Mpeg7>").or_else(|| find_block(&window, b"<mpeg7:Mpeg7", b"</mpeg7:Mpeg7>")) {
            root = parse(block);
            if root.is_some() {
                break;
            }
        }
    }
    let Some(root) = root else { return Ok(false) };
    let texts = |name: &str| -> Vec<String> {
        let mut nodes = Vec::new();
        root.find_all(name, &mut nodes);
        let mut out: Vec<String> = nodes.iter().map(|n| n.text_of()).filter(|t| !t.is_empty()).collect();
        out.dedup();
        out
    };
    let titles = texts("Title");
    let keywords = texts("Keyword");
    let annotations = texts("FreeTextAnnotation");
    let mut people = Vec::new();
    let mut names = Vec::new();
    root.find_all("Name", &mut names);
    for name in names {
        let given = name.find("GivenName").map(|n| n.text_of()).unwrap_or_default();
        let family = name.find("FamilyName").map(|n| n.text_of()).unwrap_or_default();
        let full = format!("{} {}", given, family).trim().to_string();
        if !full.is_empty() && !people.contains(&full) {
            people.push(full);
        }
    }
    let genres = texts("Genre");
    if titles.is_empty() && keywords.is_empty() && annotations.is_empty() && people.is_empty() {
        return Ok(false);
    }
    ctx.norm("title", titles.first().map(|t| json!(t)).unwrap_or(Value::Null));
    ctx.norm("description", annotations.first().map(|t| json!(t)).unwrap_or(Value::Null));
    if !people.is_empty() {
        ctx.norm("author", json!(people.join(", ")));
    }
    ctx.keywords(keywords.clone());
    let mut raw = Map::new();
    raw.insert("titles".into(), json!(titles));
    raw.insert("keywords".into(), json!(keywords));
    raw.insert("annotations".into(), json!(annotations));
    raw.insert("people".into(), json!(people));
    raw.insert("genres".into(), json!(genres));
    raw.retain(|_, v| v.as_array().is_some_and(|a| !a.is_empty()));
    ctx.raw("mpeg7", raw);
    Ok(true)
}

// ---------------------------------------------------------------------------
// iXML (in WAV files from field recorders)
// ---------------------------------------------------------------------------

/// The useful fields of an iXML chunk.
pub(crate) fn ixml(bytes: &[u8]) -> Map<String, Value> {
    let mut map = Map::new();
    let Some(root) = parse(bytes) else { return map };
    for child in &root.children {
        if child.children.is_empty() {
            let text = child.text.trim();
            if !text.is_empty() {
                map.insert(child.name.to_ascii_lowercase(), json!(text));
            }
        }
    }
    for section in ["SPEED", "BEXT", "SYNC_POINT_LIST", "LOCATION", "USER"] {
        if let Some(node) = root.children.iter().find(|c| c.name == section) {
            let fields: Map<String, Value> = node
                .children
                .iter()
                .filter(|c| c.children.is_empty() && !c.text.trim().is_empty())
                .map(|c| (c.name.to_ascii_lowercase(), json!(c.text.trim())))
                .collect();
            if !fields.is_empty() {
                map.insert(section.to_ascii_lowercase(), Value::Object(fields));
            }
        }
    }
    let mut tracks = Vec::new();
    root.find_all("TRACK", &mut tracks);
    let names: Vec<String> = tracks.iter().filter_map(|t| t.find("NAME").map(|n| n.text_of())).filter(|n| !n.is_empty()).collect();
    if !names.is_empty() {
        map.insert("tracks".into(), json!(names));
    }
    map
}

// ---------------------------------------------------------------------------
// SVG
// ---------------------------------------------------------------------------

pub(crate) fn svg(ctx: &mut Ctx) -> anyhow::Result<bool> {
    if ctx.content_type != "image/svg+xml" && ctx.ext() != "svg" {
        return Ok(false);
    }
    let bytes = ctx.source.all(8 * 1024 * 1024)?;
    let Some(root) = parse(&bytes) else { return Ok(false) };
    if root.local() != "svg" {
        return Ok(false);
    }
    let mut raw = Map::new();
    for attr in ["width", "height", "viewBox", "version"] {
        if let Some(v) = root.attr(attr) {
            raw.insert(attr.to_string(), json!(v));
        }
    }
    let direct = |name: &str| root.children.iter().find(|c| c.local() == name).map(|n| n.text_of()).filter(|t| !t.is_empty());
    if let Some(title) = direct("title") {
        raw.insert("title".into(), json!(title));
        ctx.norm("title", json!(title));
    }
    if let Some(desc) = direct("desc") {
        raw.insert("desc".into(), json!(desc));
        ctx.norm("description", json!(desc));
    }
    let number = |v: Option<&str>| v.and_then(|s| s.trim_end_matches("px").trim().parse::<f64>().ok());
    if let (Some(w), Some(h)) = (number(root.attr("width")), number(root.attr("height"))) {
        ctx.norm("width", json!(w.round() as u64));
        ctx.norm("height", json!(h.round() as u64));
    }
    let mut texts = Vec::new();
    root.find_all("text", &mut texts);
    let words: Vec<String> = texts.iter().map(|t| t.text_of()).filter(|t| !t.is_empty()).collect();
    ctx.add_text(&words.join("\n"));
    // RDF metadata in SVG often has no x:xmpmeta wrapper.
    if let Some(metadata) = root.find("RDF") {
        let map = xmp_from_dom(metadata);
        if !map.is_empty() {
            ctx.raw("xmp", map);
        }
    }
    ctx.raw("svg", raw);
    Ok(true)
}
