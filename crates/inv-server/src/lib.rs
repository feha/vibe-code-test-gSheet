//! `inv-server`: a **zero-knowledge** encrypted-blob store.
//!
//! The server never decrypts and never parses a workspace blob as
//! [`inv_model::Workspace`] — it only stores and returns opaque bytes keyed by id.
//! Optimistic concurrency (an `if-match` / `x-version` u64 protocol) keeps
//! concurrent editors from clobbering each other.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use redb::{Database, ReadableTable, TableDefinition};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

/// Table of workspace blobs. Value layout: 8-byte little-endian version, then the
/// opaque (encrypted) blob bytes.
const WS: TableDefinition<&str, &[u8]> = TableDefinition::new("workspaces");

/// Table of photo blobs. Key: `"{wid}/{pid}"`. Value: opaque bytes.
const PH: TableDefinition<&str, &[u8]> = TableDefinition::new("photos");

/// Open (creating if needed) the redb database at `path`.
pub fn open_db(path: impl AsRef<Path>) -> anyhow::Result<Arc<Database>> {
    let db = Database::create(path)?;
    Ok(Arc::new(db))
}

/// Build the API-only router backed by `db`.
pub fn build_router(db: Arc<Database>) -> Router {
    build_router_with_static(db, None)
}

/// Build the router backed by `db`, optionally serving `static_dir` as a SPA
/// fallback for any non-API path (unmatched routes resolve to `index.html`).
pub fn build_router_with_static(db: Arc<Database>, static_dir: Option<PathBuf>) -> Router {
    let mut router = Router::new()
        .route("/api/health", get(health))
        .route("/api/w/:wid", get(get_workspace).put(put_workspace))
        .route(
            "/api/w/:wid/photo/:pid",
            get(get_photo).put(put_photo).delete(delete_photo),
        );

    if let Some(dir) = static_dir {
        // Serve files from `dir`, falling back to its `index.html` for SPA routes.
        let index = dir.join("index.html");
        // `.fallback` (not `.not_found_service`) serves index.html with a 200 for
        // any unmatched path, which is what an SPA's client-side router needs.
        let serve = ServeDir::new(dir).fallback(ServeFile::new(index));
        router = router.fallback_service(serve);
    }

    router
        // Permissive CORS so a separately-served dev frontend can call the API.
        .layer(CorsLayer::permissive())
        .with_state(db)
}

/// Run the HTTP server on `addr`. When `static_dir` is `Some`, its contents are
/// served as a SPA fallback for the frontend.
pub async fn run(
    addr: SocketAddr,
    db: Arc<Database>,
    static_dir: Option<PathBuf>,
) -> anyhow::Result<()> {
    let router = build_router_with_static(db, static_dir);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

/// Parse the version (first 8 bytes, little-endian) out of a stored value.
fn split_value(stored: &[u8]) -> (u64, &[u8]) {
    let mut v = [0u8; 8];
    v.copy_from_slice(&stored[..8]);
    (u64::from_le_bytes(v), &stored[8..])
}

/// Encode `version ++ blob` for storage.
fn encode_value(version: u64, blob: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + blob.len());
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(blob);
    out
}

fn server_error<E: std::fmt::Display>(e: E) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

async fn get_workspace(
    State(db): State<Arc<Database>>,
    AxumPath(wid): AxumPath<String>,
) -> Response {
    let read = match db.begin_read() {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    let table = match read.open_table(WS) {
        Ok(t) => t,
        // A brand-new database has no table yet -> nothing stored.
        Err(redb::TableError::TableDoesNotExist(_)) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return server_error(e),
    };
    match table.get(wid.as_str()) {
        Ok(Some(val)) => {
            let (version, blob) = split_value(val.value());
            let mut headers = HeaderMap::new();
            headers.insert("x-version", version.into());
            (headers, blob.to_vec()).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => server_error(e),
    }
}

async fn put_workspace(
    State(db): State<Arc<Database>>,
    AxumPath(wid): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // `if-match` header parsed as u64; absent or unparseable -> 0.
    let if_match: u64 = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let write = match db.begin_write() {
        Ok(w) => w,
        Err(e) => return server_error(e),
    };

    let new_version;
    {
        let mut table = match write.open_table(WS) {
            Ok(t) => t,
            Err(e) => return server_error(e),
        };

        let cur = match table.get(wid.as_str()) {
            Ok(Some(val)) => split_value(val.value()).0,
            Ok(None) => 0,
            Err(e) => return server_error(e),
        };

        if if_match != cur {
            // Stale write: reject with the current version so the client can refetch.
            let mut h = HeaderMap::new();
            h.insert("x-version", cur.into());
            return (StatusCode::CONFLICT, h).into_response();
        }

        new_version = cur + 1;
        let value = encode_value(new_version, &body);
        // `insert` returns a guard over the previous value that borrows `table`;
        // discard it (`.map(|_| ())`) so `table` can be dropped at end of scope.
        if let Err(e) = table.insert(wid.as_str(), value.as_slice()).map(|_| ()) {
            return server_error(e);
        }
    }
    if let Err(e) = write.commit() {
        return server_error(e);
    }

    let mut headers = HeaderMap::new();
    headers.insert("x-version", new_version.into());
    (StatusCode::OK, headers).into_response()
}

async fn get_photo(
    State(db): State<Arc<Database>>,
    AxumPath((wid, pid)): AxumPath<(String, String)>,
) -> Response {
    let key = format!("{wid}/{pid}");
    let read = match db.begin_read() {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    let table = match read.open_table(PH) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return server_error(e),
    };
    match table.get(key.as_str()) {
        Ok(Some(val)) => val.value().to_vec().into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => server_error(e),
    }
}

async fn put_photo(
    State(db): State<Arc<Database>>,
    AxumPath((wid, pid)): AxumPath<(String, String)>,
    body: Bytes,
) -> Response {
    let key = format!("{wid}/{pid}");
    let write = match db.begin_write() {
        Ok(w) => w,
        Err(e) => return server_error(e),
    };
    {
        let mut table = match write.open_table(PH) {
            Ok(t) => t,
            Err(e) => return server_error(e),
        };
        if let Err(e) = table.insert(key.as_str(), body.as_ref()).map(|_| ()) {
            return server_error(e);
        }
    }
    if let Err(e) = write.commit() {
        return server_error(e);
    }
    StatusCode::OK.into_response()
}

async fn delete_photo(
    State(db): State<Arc<Database>>,
    AxumPath((wid, pid)): AxumPath<(String, String)>,
) -> Response {
    let key = format!("{wid}/{pid}");
    let write = match db.begin_write() {
        Ok(w) => w,
        Err(e) => return server_error(e),
    };
    {
        let mut table = match write.open_table(PH) {
            Ok(t) => t,
            Err(e) => return server_error(e),
        };
        if let Err(e) = table.remove(key.as_str()).map(|_| ()) {
            return server_error(e);
        }
    }
    if let Err(e) = write.commit() {
        return server_error(e);
    }
    StatusCode::NO_CONTENT.into_response()
}
