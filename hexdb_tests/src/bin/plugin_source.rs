//! A source plugin for tests: writes one document to the tessellation named
//! by `SOURCE_TESSELLATION` through the API with the key HexDB gave it, tries
//! one it isn't allowed to write, reports both statuses on stdout (which HexDB
//! logs), then waits.

fn main() {
    let api = std::env::var("HEXDB_API").expect("HEXDB_API");
    let key = std::env::var("HEXDB_API_KEY").expect("HEXDB_API_KEY");
    let tessellation = std::env::var("SOURCE_TESSELLATION").unwrap_or_else(|_| "ingested".into());
    let client = reqwest::blocking::Client::new();
    let post = |tess: &str| {
        client
            .post(format!("{}/{}", api, tess))
            .bearer_auth(&key)
            .json(&serde_json::json!({ "from": "source plugin" }))
            .send()
            .map(|r| r.status().as_u16())
            .unwrap_or(0)
    };
    println!("source pid {}", std::process::id());
    println!("source wrote {} -> {}", tessellation, post(&tessellation));
    println!("source wrote forbidden -> {}", post("forbidden"));
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
