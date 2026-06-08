//! Regression test for the Google Sheets gateway panic.
//!
//! The bug: the gsheet backend used a `reqwest::blocking` HTTP client, which
//! spins up its own internal tokio runtime. The gateway runs the (blocking)
//! store on `tokio::task::spawn_blocking`, so the reqwest runtime was dropped
//! *inside* tokio's async context, panicking the worker with:
//!
//!   "Cannot drop a runtime in a context where blocking is not allowed."
//!
//! That panic dropped the connection (client saw an empty reply), even though
//! the process limped on. This test drives the real router over `tower`'s
//! `oneshot` and asserts that a failing gsheet load returns a *normal* HTTP
//! error status (not a dropped connection / panic) and that the worker is still
//! healthy afterwards.
//!
//! Hermetic: the public URL points at `127.0.0.1:9` (the discard port), so the
//! connection is refused immediately — no real network or DNS to Google.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use inv_server::{api_router, AppState};

/// A gsheet `public_url` load against a refused connection must come back as a
/// normal error status (4xx/5xx), NOT crash the worker. Before the fix this
/// panicked inside `spawn_blocking` ("Cannot drop a runtime ...") and the
/// connection was dropped; after the fix (ureq) it is a clean Backend -> 500.
#[tokio::test]
async fn gsheet_public_url_failure_does_not_panic_worker() {
    let app = api_router(AppState::new());

    // Port 9 == discard; a connection here is refused, hermetically (no DNS/
    // network to google). The URL is the standard share/edit shape.
    let desc = json!({
        "kind": "gsheet",
        "mode": {
            "mode": "public_url",
            "url": "http://127.0.0.1:9/spreadsheets/d/FAKE/edit"
        }
    });

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/inventory")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&desc).unwrap()))
                .unwrap(),
        )
        .await
        .expect("router produced a response (no dropped connection / panic)");

    // A normal error status — the request failed gracefully, the worker did not
    // die. (Before the fix this path panicked and never produced a response.)
    assert!(
        resp.status().is_client_error() || resp.status().is_server_error(),
        "expected a 4xx/5xx error status, got {}",
        resp.status()
    );

    // Crucially: the worker survived. A follow-up health check must still be 200,
    // proving no panic took down the tokio worker.
    let health = app
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("health response after gsheet failure");
    assert_eq!(
        health.status(),
        StatusCode::OK,
        "worker must still be healthy after a gsheet failure"
    );
}
