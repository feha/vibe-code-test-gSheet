//! Property tests for the zero-knowledge blob store's invariants.
//!
//! `proptest` is not a declared dependency of this crate (the workspace lockfile
//! is frozen), so these property tests use a small, deterministic xorshift PRNG to
//! generate many varied inputs and assert each invariant holds across all of them.
//! All code under test is the real router + redb store (no mocks).

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Deterministic xorshift64* PRNG so failures are reproducible from the seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Avoid the zero state, which xorshift cannot escape.
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % (n as u64)) as usize
    }
    /// A random byte vector of length 0..=max_len, full 0..=255 byte range
    /// (deliberately including non-UTF8 sequences and embedded NULs).
    fn bytes(&mut self, max_len: usize) -> Vec<u8> {
        let len = self.below(max_len + 1);
        (0..len).map(|_| (self.next_u64() & 0xFF) as u8).collect()
    }
    /// A random UUID-shaped key string.
    fn uuid(&mut self) -> String {
        let a = self.next_u64();
        let b = self.next_u64();
        format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            a as u32,
            (a >> 32) as u16,
            (a >> 48) as u16,
            b as u16,
            b >> 16 & 0xFFFF_FFFF_FFFF
        )
    }
}

fn fresh() -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = inv_server::open_db(dir.path().join("p.redb")).expect("open_db");
    (inv_server::build_router(db), dir)
}

async fn body_bytes(resp: axum::http::Response<Body>) -> Vec<u8> {
    resp.into_body().collect().await.unwrap().to_bytes().to_vec()
}

fn version_header(resp: &axum::http::Response<Body>) -> Option<u64> {
    resp.headers()
        .get("x-version")
        .map(|v| v.to_str().unwrap().parse::<u64>().unwrap())
}

fn put_ws(wid: &str, if_match: u64, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(format!("/api/w/{wid}"))
        .header("if-match", if_match.to_string())
        .body(Body::from(body))
        .unwrap()
}

fn get_ws(wid: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(format!("/api/w/{wid}"))
        .body(Body::empty())
        .unwrap()
}

fn photo_req(method: &str, wid: &str, pid: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(format!("/api/w/{wid}/photo/{pid}"))
        .body(Body::from(body))
        .unwrap()
}

/// INVARIANT: arbitrary bytes stored under correct optimistic-concurrency rules
/// round-trip byte-identically, and the version increments by exactly 1 each
/// successful PUT and is reflected by GET. (Opacity + version monotonicity.)
#[tokio::test]
async fn prop_roundtrip_and_version_monotonicity() {
    let (router, _dir) = fresh();
    let mut rng = Rng::new(0xDEADBEEF);

    // Several independent workspaces, each updated several times.
    for _ in 0..40 {
        let wid = rng.uuid();
        let mut version = 0u64;
        let updates = 1 + rng.below(6);
        for _ in 0..updates {
            let blob = rng.bytes(512);
            let resp = router
                .clone()
                .oneshot(put_ws(&wid, version, blob.clone()))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "correct if-match must succeed");
            version += 1;
            assert_eq!(
                version_header(&resp),
                Some(version),
                "version must increment by exactly 1"
            );

            // GET reflects current version and byte-identical content.
            let resp = router.clone().oneshot(get_ws(&wid)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(version_header(&resp), Some(version));
            assert_eq!(body_bytes(resp).await, blob, "store must be byte-identical");
        }
    }
}

/// INVARIANT: a PUT whose `if-match` differs from the current version is rejected
/// with 409 + current version, and leaves the stored blob and version unchanged.
#[tokio::test]
async fn prop_optimistic_concurrency_rejects_stale_writes() {
    let (router, _dir) = fresh();
    let mut rng = Rng::new(0x12345678);

    for _ in 0..60 {
        let wid = rng.uuid();
        // Establish a known good state at version 1.
        let good = rng.bytes(256);
        let resp = router
            .clone()
            .oneshot(put_ws(&wid, 0, good.clone()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(version_header(&resp), Some(1));

        // A stale if-match (anything != 1) must be rejected.
        let mut stale = rng.next_u64();
        if stale == 1 {
            stale = 2; // ensure it is genuinely stale
        }
        let resp = router
            .clone()
            .oneshot(put_ws(&wid, stale, rng.bytes(256)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT, "stale write must 409");
        assert_eq!(
            version_header(&resp),
            Some(1),
            "409 must report current version"
        );

        // State is unchanged.
        let resp = router.clone().oneshot(get_ws(&wid)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(version_header(&resp), Some(1));
        assert_eq!(body_bytes(resp).await, good, "stale write must not mutate");
    }
}

/// INVARIANT: photos round-trip byte-identically and are isolated by their
/// `(wid, pid)` key — storing one never affects another, and DELETE removes only
/// the targeted photo.
#[tokio::test]
async fn prop_photo_roundtrip_and_key_isolation() {
    let (router, _dir) = fresh();
    let mut rng = Rng::new(0xABCDEF01);

    for _ in 0..30 {
        // Two distinct keys.
        let wid_a = rng.uuid();
        let pid_a = rng.uuid();
        let wid_b = rng.uuid();
        let pid_b = rng.uuid();
        let img_a = rng.bytes(400);
        let img_b = rng.bytes(400);

        for (m, w, p, img) in [
            ("PUT", &wid_a, &pid_a, &img_a),
            ("PUT", &wid_b, &pid_b, &img_b),
        ] {
            let resp = router
                .clone()
                .oneshot(photo_req(m, w, p, img.clone()))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        // Each key returns its own bytes (isolation + opacity).
        let resp = router
            .clone()
            .oneshot(photo_req("GET", &wid_a, &pid_a, vec![]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, img_a);

        let resp = router
            .clone()
            .oneshot(photo_req("GET", &wid_b, &pid_b, vec![]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, img_b);

        // Deleting A leaves B intact and makes A a 404.
        let resp = router
            .clone()
            .oneshot(photo_req("DELETE", &wid_a, &pid_a, vec![]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = router
            .clone()
            .oneshot(photo_req("GET", &wid_a, &pid_a, vec![]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = router
            .clone()
            .oneshot(photo_req("GET", &wid_b, &pid_b, vec![]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, img_b, "delete must not affect other keys");
    }
}

/// INVARIANT (zero-knowledge): the server never needs to interpret a blob; even a
/// blob that is *valid* `inv_model::Workspace` JSON is stored and returned as
/// opaque bytes, byte-for-byte, exactly like arbitrary binary. We additionally
/// store deliberately malformed/garbage bytes to prove no parsing occurs.
#[tokio::test]
async fn prop_blobs_are_never_parsed_as_workspace() {
    let (router, _dir) = fresh();
    let mut rng = Rng::new(0x0F0F0F0F);

    for _ in 0..40 {
        let wid = rng.uuid();
        // Half the time use random binary; half the time use bytes that look like
        // (or partially like) workspace JSON — both must be treated identically.
        let blob: Vec<u8> = if rng.below(2) == 0 {
            rng.bytes(600)
        } else {
            let id = rng.uuid();
            let mut s = format!(
                "{{\"id\":\"{id}\",\"classes\":{{}},\"instances\":{{}}}}"
            )
            .into_bytes();
            // Corrupt it sometimes so it is NOT valid JSON; the server must not care.
            if rng.below(2) == 0 {
                s.truncate(s.len().saturating_sub(1 + rng.below(s.len().max(1))));
                s.extend_from_slice(&[0x00, 0xFF, 0xFE]);
            }
            s
        };

        let resp = router
            .clone()
            .oneshot(put_ws(&wid, 0, blob.clone()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = router.clone().oneshot(get_ws(&wid)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_bytes(resp).await,
            blob,
            "blob must be returned opaque and byte-identical regardless of content"
        );
    }
}
