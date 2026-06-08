//! `inv-server`: the storage **gateway** for the bring-your-own-database
//! inventory app.
//!
//! This is a *local broker* between the (WASM) UI and whatever store the user
//! chose. The UI never speaks a backend protocol directly; instead every request
//! carries a [`StoreDescriptor`] naming the backing store, and the gateway:
//!
//! 1. opens (and caches, keyed by the serialized descriptor) the [`Store`], so
//!    connections are reused across requests;
//! 2. runs the blocking store op on a blocking thread via
//!    [`tokio::task::spawn_blocking`] (the [`Store`] API is synchronous);
//! 3. returns the full updated [`Inventory`] as JSON (for mutations) or the
//!    requested bytes (for photos).
//!
//! There are NO accounts and NO UUIDs. Object identity is the store-native `i64`.
//! `now` is injected as unix-millis from [`SystemTime`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use inv_core::{CoreError, InstancePatch, InventoryExt, RemoveMode};
use inv_model::{FieldValue, Inventory};
use inv_store::{open, Store, StoreDescriptor, StoreError, StoreExt};

use std::collections::BTreeMap;

/// Shared gateway state: a cache of opened stores keyed by the serialized
/// [`StoreDescriptor`], so repeated requests reuse the same handle/connection.
#[derive(Clone, Default)]
pub struct AppState {
    stores: Arc<Mutex<HashMap<String, Arc<dyn Store>>>>,
}

impl AppState {
    /// Create an empty gateway state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached store for `desc`, opening (and caching) it if needed.
    fn store_for(&self, desc: &StoreDescriptor) -> Result<Arc<dyn Store>, StoreError> {
        // Cache key is the canonical serialization of the descriptor.
        let key = serde_json::to_string(desc)
            .map_err(|e| StoreError::Backend(format!("serialize descriptor: {e}")))?;
        {
            let map = self.stores.lock().expect("store cache poisoned");
            if let Some(s) = map.get(&key) {
                return Ok(s.clone());
            }
        }
        // Open outside the lock is unnecessary here (open is cheap), but we keep
        // the critical section short and tolerate a benign double-open race by
        // letting the first inserted handle win.
        let store: Arc<dyn Store> = Arc::from(open(desc)?);
        let mut map = self.stores.lock().expect("store cache poisoned");
        let entry = map.entry(key).or_insert(store);
        Ok(entry.clone())
    }
}

/// Current wall-clock time in unix milliseconds.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Wire mirror of [`inv_core::InstancePatch`] (the core type is not `Serde`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PatchWire {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub set_fields: BTreeMap<String, FieldValue>,
    #[serde(default)]
    pub remove_fields: Vec<String>,
}

impl From<PatchWire> for InstancePatch {
    fn from(p: PatchWire) -> Self {
        InstancePatch {
            name: p.name,
            set_fields: p.set_fields,
            remove_fields: p.remove_fields,
        }
    }
}

/// Wire mirror of [`inv_core::RemoveMode`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoveModeWire {
    Cascade,
    Reparent,
}

impl From<RemoveModeWire> for RemoveMode {
    fn from(m: RemoveModeWire) -> Self {
        match m {
            RemoveModeWire::Cascade => RemoveMode::Cascade,
            RemoveModeWire::Reparent => RemoveMode::Reparent,
        }
    }
}

/// A mutation to apply to an inventory inside a single transaction.
///
/// Serde-tagged on `"op"` so the wire form is e.g.
/// `{"op":"add_instance","class":"Box","name":"a","fields":{},"parent":null,"tags":[]}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    AddInstance {
        class: String,
        name: String,
        #[serde(default)]
        fields: BTreeMap<String, FieldValue>,
        #[serde(default)]
        parent: Option<i64>,
        #[serde(default)]
        tags: Vec<String>,
    },
    EditInstance {
        id: i64,
        patch: PatchWire,
    },
    MoveInstance {
        id: i64,
        #[serde(default)]
        new_parent: Option<i64>,
    },
    RemoveInstance {
        id: i64,
        mode: RemoveModeWire,
    },
    DuplicateInstance {
        id: i64,
        deep: bool,
    },
    AddTag {
        id: i64,
        tag: String,
    },
    RemoveTag {
        id: i64,
        tag: String,
    },
    AddRelationship {
        id: i64,
        kind: String,
        target: i64,
    },
    RemoveRelationship {
        id: i64,
        kind: String,
        target: i64,
    },
    ChangeClass {
        id: i64,
        new_class: String,
    },
    DeleteClass {
        name: String,
    },
}

impl Op {
    /// Apply this op to `inv` using the injected `now`. Errors are [`CoreError`]s,
    /// which the caller maps to HTTP statuses.
    fn apply(self, inv: &mut Inventory, now: i64) -> Result<(), CoreError> {
        match self {
            Op::AddInstance {
                class,
                name,
                fields,
                parent,
                tags,
            } => {
                let id = inv.add_instance(&class, &name, fields, parent, now)?;
                for tag in tags {
                    inv.add_tag(id, &tag, now)?;
                }
                Ok(())
            }
            Op::EditInstance { id, patch } => inv.edit_instance(id, patch.into(), now),
            Op::MoveInstance { id, new_parent } => inv.move_instance(id, new_parent, now),
            Op::RemoveInstance { id, mode } => inv.remove_instance(id, mode.into()).map(|_| ()),
            Op::DuplicateInstance { id, deep } => {
                inv.duplicate_instance(id, deep, now).map(|_| ())
            }
            Op::AddTag { id, tag } => inv.add_tag(id, &tag, now),
            Op::RemoveTag { id, tag } => inv.remove_tag(id, &tag, now),
            Op::AddRelationship { id, kind, target } => {
                inv.add_relationship(id, &kind, target, now)
            }
            Op::RemoveRelationship { id, kind, target } => {
                inv.remove_relationship(id, &kind, target, now)
            }
            Op::ChangeClass { id, new_class } => inv.change_class(id, &new_class, now),
            Op::DeleteClass { name } => inv.delete_class(&name),
        }
    }
}

/// Body of `POST /api/op`: `{ "store": <StoreDescriptor>, "op": <Op> }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpRequest {
    pub store: StoreDescriptor,
    pub op: Op,
}

/// Body of `POST /api/photo/get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhotoGetRequest {
    pub store: StoreDescriptor,
    pub key: String,
}

/// Query params for `POST /api/photo/put`.
#[derive(Debug, Clone, Deserialize)]
pub struct PhotoPutQuery {
    /// JSON-serialized [`StoreDescriptor`] (so the body can be raw photo bytes).
    pub store: String,
    pub key: String,
    #[serde(default)]
    pub mime: Option<String>,
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Map a [`CoreError`] to an HTTP status + message.
fn core_status(e: &CoreError) -> StatusCode {
    match e {
        CoreError::WouldCycle | CoreError::InvalidParent(_) | CoreError::ClassInUse => {
            StatusCode::CONFLICT
        }
        CoreError::NotFound(_) => StatusCode::NOT_FOUND,
    }
}

/// Map a [`StoreError`] to an HTTP status. `NotFound` -> 404, everything else
/// is a 500 (the gateway could not talk to / commit to the store).
fn store_status(e: &StoreError) -> StatusCode {
    match e {
        StoreError::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// An error surfaced from a transaction body: either a domain [`CoreError`] or a
/// store-layer [`StoreError`]. We thread it through [`StoreError`] (the closure's
/// error type) by stashing domain errors and recovering them after the transact.
enum TxnError {
    Core(CoreError),
    Store(StoreError),
}

impl TxnError {
    fn into_response(self) -> Response {
        match self {
            TxnError::Core(e) => (core_status(&e), e.to_string()).into_response(),
            TxnError::Store(e) => (store_status(&e), e.to_string()).into_response(),
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health() -> &'static str {
    "ok"
}

/// `POST /api/inventory` — load and return the full inventory for a store.
async fn get_inventory(
    State(state): State<AppState>,
    Json(desc): Json<StoreDescriptor>,
) -> Response {
    let store = match state.store_for(&desc) {
        Ok(s) => s,
        Err(e) => return (store_status(&e), e.to_string()).into_response(),
    };
    let loaded = tokio::task::spawn_blocking(move || store.load()).await;
    match loaded {
        Ok(Ok(inv)) => Json(inv).into_response(),
        Ok(Err(e)) => (store_status(&e), e.to_string()).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task join error: {e}"),
        )
            .into_response(),
    }
}

/// `POST /api/op` — apply one op in a transaction, return the full inventory.
async fn apply_op(State(state): State<AppState>, Json(req): Json<OpRequest>) -> Response {
    let store = match state.store_for(&req.store) {
        Ok(s) => s,
        Err(e) => return (store_status(&e), e.to_string()).into_response(),
    };
    let op = req.op;
    let now = now_millis();

    let result = tokio::task::spawn_blocking(move || {
        // Stash a domain error here so we can return the precise CoreError after
        // the transaction (the closure must yield a StoreError).
        let mut core_err: Option<CoreError> = None;
        let txn = store.transact(&mut |inv| {
            core_err = None;
            if let Err(e) = op.clone().apply(inv, now) {
                core_err = Some(e);
                // Abort the commit; the inventory mutation (if any) is discarded.
                return Err(StoreError::Backend("domain error".to_string()));
            }
            Ok(inv.clone())
        });
        match txn {
            Ok(inv) => Ok(inv),
            Err(store_e) => {
                if let Some(ce) = core_err {
                    Err(TxnError::Core(ce))
                } else {
                    Err(TxnError::Store(store_e))
                }
            }
        }
    })
    .await;

    match result {
        Ok(Ok(inv)) => Json(inv).into_response(),
        Ok(Err(txn_err)) => txn_err.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task join error: {e}"),
        )
            .into_response(),
    }
}

/// `POST /api/photo/put` — store raw photo bytes under a key.
async fn put_photo(
    State(state): State<AppState>,
    Query(q): Query<PhotoPutQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let desc: StoreDescriptor = match serde_json::from_str(&q.store) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("bad store descriptor: {e}"),
            )
                .into_response()
        }
    };
    // mime is accepted from query or the Content-Type header; it is not persisted
    // by the Store (photo bytes are opaque) but is part of the contract so the
    // caller can pass it through. We read it to honor the contract.
    let _mime = q.mime.or_else(|| {
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    });

    let store = match state.store_for(&desc) {
        Ok(s) => s,
        Err(e) => return (store_status(&e), e.to_string()).into_response(),
    };
    let key = q.key;
    let bytes = body.to_vec();
    let res = tokio::task::spawn_blocking(move || store.put_photo(&key, &bytes)).await;
    match res {
        Ok(Ok(())) => StatusCode::OK.into_response(),
        Ok(Err(e)) => (store_status(&e), e.to_string()).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task join error: {e}"),
        )
            .into_response(),
    }
}

/// `POST /api/photo/get` — fetch raw photo bytes, or 404.
async fn get_photo(
    State(state): State<AppState>,
    Json(req): Json<PhotoGetRequest>,
) -> Response {
    let store = match state.store_for(&req.store) {
        Ok(s) => s,
        Err(e) => return (store_status(&e), e.to_string()).into_response(),
    };
    let key = req.key;
    let res = tokio::task::spawn_blocking(move || store.get_photo(&key)).await;
    match res {
        Ok(Ok(Some(bytes))) => (StatusCode::OK, bytes).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "photo not found").into_response(),
        Ok(Err(e)) => (store_status(&e), e.to_string()).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("task join error: {e}"),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the API-only router (no static file serving). Useful for tests.
pub fn api_router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/inventory", post(get_inventory))
        .route("/api/op", post(apply_op))
        .route("/api/photo/put", post(put_photo))
        .route("/api/photo/get", post(get_photo))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Build the full app router: the API plus, if `static_dir` is `Some` and exists,
/// a static SPA served from it with `index.html` as the fallback.
pub fn app(state: AppState, static_dir: Option<&str>) -> Router {
    let mut router = api_router(state);
    if let Some(dir) = static_dir {
        let path = std::path::Path::new(dir);
        if path.is_dir() {
            let index = path.join("index.html");
            let serve = ServeDir::new(path).not_found_service(ServeFile::new(index));
            router = router.fallback_service(serve);
        }
    }
    router
}
