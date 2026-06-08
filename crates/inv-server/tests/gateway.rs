//! Gateway integration tests: drive the axum router with `tower`'s `oneshot`
//! against a real [`FileStore`](inv_store::FileStore) backed by a tempfile.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use inv_server::{api_router, AppState};

/// Build a `{"kind":"file","path":...}` descriptor over a fresh tempfile dir.
/// Returns (descriptor JSON value, the kept TempDir).
fn file_descriptor() -> (Value, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inv.json");
    let desc = json!({ "kind": "file", "path": path.to_string_lossy() });
    (desc, dir)
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = body_bytes(resp).await;
    serde_json::from_slice(&bytes).unwrap()
}

fn post_json(uri: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn health_returns_ok() {
    let app = api_router(AppState::new());
    let resp = app
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
async fn inventory_of_fresh_file_store_is_empty() {
    let app = api_router(AppState::new());
    let (desc, _dir) = file_descriptor();

    let resp = app
        .oneshot(post_json("/api/inventory", &desc))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let inv = body_json(resp).await;
    assert_eq!(inv["next_id"], 1);
    assert!(inv["instances"].as_object().unwrap().is_empty());
    assert!(inv["classes"].as_object().unwrap().is_empty());
}

#[tokio::test]
async fn add_instance_op_returns_inventory_with_id_1() {
    let app = api_router(AppState::new());
    let (desc, _dir) = file_descriptor();

    let req = json!({
        "store": desc,
        "op": {
            "op": "add_instance",
            "class": "Box",
            "name": "first",
            "fields": {},
            "parent": null,
            "tags": ["red"]
        }
    });

    let resp = app.oneshot(post_json("/api/op", &req)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let inv = body_json(resp).await;
    // The instance got store-native id 1, keyed by "1" in the JSON map.
    let inst = &inv["instances"]["1"];
    assert_eq!(inst["id"], 1);
    assert_eq!(inst["class"], "Box");
    assert_eq!(inst["name"], "first");
    assert_eq!(inst["tags"], json!(["red"]));
    assert_eq!(inv["next_id"], 2);
    // The class was auto-created.
    assert_eq!(inv["classes"]["Box"]["name"], "Box");
}

#[tokio::test]
async fn move_instance_forming_cycle_is_409() {
    let app = api_router(AppState::new());
    let (desc, _dir) = file_descriptor();

    // a (id 1) <- b (id 2)
    let add_a = json!({
        "store": desc,
        "op": {"op":"add_instance","class":"C","name":"a","fields":{},"parent":null,"tags":[]}
    });
    let resp = app
        .clone()
        .oneshot(post_json("/api/op", &add_a))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let add_b = json!({
        "store": desc,
        "op": {"op":"add_instance","class":"C","name":"b","fields":{},"parent":1,"tags":[]}
    });
    let resp = app
        .clone()
        .oneshot(post_json("/api/op", &add_b))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Move a (1) under b (2): b is a descendant of a -> cycle.
    let mv = json!({
        "store": desc,
        "op": {"op":"move_instance","id":1,"new_parent":2}
    });
    let resp = app.oneshot(post_json("/api/op", &mv)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn photo_put_then_get_roundtrip() {
    let app = api_router(AppState::new());
    let (desc, _dir) = file_descriptor();
    let store_str = serde_json::to_string(&desc).unwrap();

    // PUT raw bytes.
    let put_uri = format!(
        "/api/photo/put?store={}&key={}&mime={}",
        urlencoding(&store_str),
        urlencoding("1-0"),
        urlencoding("image/png")
    );
    let put = Request::builder()
        .method("POST")
        .uri(&put_uri)
        .header("content-type", "image/png")
        .body(Body::from(vec![1u8, 2, 3, 4, 5]))
        .unwrap();
    let resp = app.clone().oneshot(put).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // GET them back.
    let get_body = json!({ "store": desc, "key": "1-0" });
    let resp = app
        .clone()
        .oneshot(post_json("/api/photo/get", &get_body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_bytes(resp).await, vec![1u8, 2, 3, 4, 5]);

    // Missing key -> 404.
    let miss = json!({ "store": desc, "key": "does-not-exist" });
    let resp = app
        .oneshot(post_json("/api/photo/get", &miss))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn op_on_missing_instance_is_404() {
    let app = api_router(AppState::new());
    let (desc, _dir) = file_descriptor();

    let edit = json!({
        "store": desc,
        "op": {"op":"edit_instance","id":999,"patch":{"name":"x"}}
    });
    let resp = app.oneshot(post_json("/api/op", &edit)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Minimal percent-encoder for query values used in tests (encodes the chars we
/// care about: `/`, `:`, space, `{`, `}`, `"`, `,`).
fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
