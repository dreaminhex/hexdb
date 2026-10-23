//! Updates: the version report, and rolling updates refused where there is no
//! hosting service to run them.

use anyhow::Result;
use hexdb_tests::TestServer;
use reqwest::Method;
use serde_json::json;

#[test]
fn updates_report_the_version_and_refuse_a_rollout_without_hosting() -> Result<()> {
    let server = TestServer::start()?;
    let updates = server.request(Method::GET, "/updates", None, &[])?;
    assert_eq!(updates.status, 200, "{}", updates.body);
    assert_eq!(updates.body["running"], env!("CARGO_PKG_VERSION"));
    assert_eq!(updates.body["hosted"], false);
    assert!(updates.body["offer"].is_null());

    // Each hex in the lattice status reports its version.
    let status = server.request(Method::GET, "/status", None, &[])?.body;
    assert_eq!(status["network"]["lattice"]["hexes"][0]["version"], env!("CARGO_PKG_VERSION"), "{}", status);
    assert_eq!(status["updates"]["hosted"], false);

    let refused = server.request(Method::POST, "/updates/rollout", None, &[])?;
    assert_eq!(refused.status, 409, "{}", refused.body);
    assert!(refused.body.to_string().contains("isn't hosted"), "{}", refused.body);

    // Administrators only.
    let reader = server.user_with_roles("rita", json!([{ "name": "reader", "tessellations": ["*"] }]))?;
    assert_eq!(server.request_as(Some(&reader), Method::GET, "/updates", None, &[])?.status, 403);
    assert_eq!(server.request_as(Some(&reader), Method::POST, "/updates/rollout", None, &[])?.status, 403);
    Ok(())
}
