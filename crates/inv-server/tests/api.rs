//! Integration tests for the zero-knowledge blob store.
//!
//! These drive the real `axum::Router` via `tower::ServiceExt::oneshot` (no real
//! network), collecting bodies with `http_body_util::BodyExt` and backing the db
//! with a `tempfile::tempdir`.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use tower::ServiceExt; // for `oneshot`

/// Build a fresh router backed by a temp redb file. Returns the router and the
/// tempdir guard (which must be kept alive for the duration of the test).
fn fresh() -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = inv_server::open_db(dir.path().join("test.redb")).expect("open_db");
    let router = inv_server::build_router(db);
    (router, dir)
}

async fn body_bytes(resp: axum::http::Response<Body>) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes()
        .to_vec()
}

fn version_header(resp: &axum::http::Response<Body>) -> Option<u64> {
    resp.headers()
        .get("x-version")
        .map(|v| v.to_str().unwrap().parse::<u64>().unwrap())
}

fn put_ws(wid: &str, if_match: u64, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(format!("/api/w/{wid}"))
        .header("if-match", if_match.to_string())
        .body(body.into())
        .unwrap()
}

fn get_ws(wid: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/api/w/{wid}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn health_returns_ok() {
    let (router, _dir) = fresh();
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"ok");
}

#[tokio::test]
async fn put_get_and_version_increment() {
    let (router, _dir) = fresh();
    let wid = "11111111-1111-1111-1111-111111111111";
    let blob = b"first-encrypted-blob".to_vec();

    // PUT new workspace with if-match 0 -> 200, x-version 1.
    let resp = router
        .clone()
        .oneshot(put_ws(wid, 0, blob.clone()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(version_header(&resp), Some(1));

    // GET -> identical bytes + x-version 1.
    let resp = router.clone().oneshot(get_ws(wid)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(version_header(&resp), Some(1));
    assert_eq!(body_bytes(resp).await, blob);

    // PUT with if-match 1 -> 200, x-version 2.
    let blob2 = b"second".to_vec();
    let resp = router
        .clone()
        .oneshot(put_ws(wid, 1, blob2.clone()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(version_header(&resp), Some(2));

    let resp = router.oneshot(get_ws(wid)).await.unwrap();
    assert_eq!(version_header(&resp), Some(2));
    assert_eq!(body_bytes(resp).await, blob2);
}

#[tokio::test]
async fn get_missing_workspace_is_404() {
    let (router, _dir) = fresh();
    let resp = router
        .oneshot(get_ws("deadbeef-0000-0000-0000-000000000000"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stale_concurrency_conflicts_and_leaves_blob_unchanged() {
    let (router, _dir) = fresh();
    let wid = "22222222-2222-2222-2222-222222222222";
    let good = b"the-good-blob".to_vec();

    // Establish version 1.
    let resp = router
        .clone()
        .oneshot(put_ws(wid, 0, good.clone()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(version_header(&resp), Some(1));

    // Stale PUT with if-match 0 (current is 1) -> 409 + x-version 1.
    let resp = router
        .clone()
        .oneshot(put_ws(wid, 0, b"clobber".to_vec()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(version_header(&resp), Some(1));

    // The stored blob and version are unchanged.
    let resp = router.oneshot(get_ws(wid)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(version_header(&resp), Some(1));
    assert_eq!(body_bytes(resp).await, good);
}

#[tokio::test]
async fn store_is_opaque_to_non_utf8_bytes() {
    let (router, _dir) = fresh();
    let wid = "33333333-3333-3333-3333-333333333333";

    // Arbitrary non-UTF8 bytes including invalid sequences, NULs, and high bytes.
    let blob: Vec<u8> = vec![
        0x00, 0xFF, 0xFE, 0x80, 0x81, 0xC0, 0xC1, 0xED, 0xA0, 0x80, 0xF5, 0x90, 0x00, 0x7F, 0xAB,
    ];
    assert!(std::str::from_utf8(&blob).is_err(), "test data must be non-UTF8");

    let resp = router
        .clone()
        .oneshot(put_ws(wid, 0, blob.clone()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = router.oneshot(get_ws(wid)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, blob, "store must be byte-identical");
}

fn photo_uri(wid: &str, pid: &str) -> String {
    format!("/api/w/{wid}/photo/{pid}")
}

#[tokio::test]
async fn photo_lifecycle_put_get_delete() {
    let (router, _dir) = fresh();
    let wid = "44444444-4444-4444-4444-444444444444";
    let pid = "55555555-5555-5555-5555-555555555555";
    let img: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47, 0x00, 0xFF, 0x10, 0x20]; // PNG-ish bytes

    // PUT photo -> 200.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(photo_uri(wid, pid))
                .body(Body::from(img.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // GET photo -> same bytes.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(photo_uri(wid, pid))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, img);

    // DELETE photo -> 204.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(photo_uri(wid, pid))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // GET after delete -> 404.
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(photo_uri(wid, pid))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn static_dir_fallback_serves_spa_and_api_still_works() {
    let dir = tempfile::tempdir().expect("tempdir");
    let static_dir = dir.path().join("dist");
    std::fs::create_dir_all(&static_dir).unwrap();
    std::fs::write(static_dir.join("index.html"), b"<!doctype html><title>spa</title>").unwrap();

    let db = inv_server::open_db(dir.path().join("test.redb")).expect("open_db");
    let router = inv_server::build_router_with_static(db, Some(static_dir));

    // Unknown non-API path falls back to the SPA index.html.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/some/spa/route")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_bytes(resp).await;
    assert!(
        body.windows(3).any(|w| w == b"spa"),
        "fallback should serve index.html"
    );

    // The API still works alongside the static fallback.
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, b"ok");
}

#[tokio::test]
async fn cors_allows_cross_origin() {
    let (router, _dir) = fresh();
    // A simple GET from a foreign origin should carry permissive CORS headers so a
    // separately-served dev frontend can read the response.
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/health")
                .header("origin", "http://localhost:1234")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let allow_origin = resp
        .headers()
        .get("access-control-allow-origin")
        .expect("CORS allow-origin header present");
    assert_eq!(allow_origin, "*");
}
