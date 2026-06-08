//! HTTP client for the gateway API, plus serde mirrors of the gateway wire types.
//!
//! The backend crates (`inv-server`, `inv-store`) are frozen and must not be
//! modified, so we define matching serde types here. The shapes mirror the
//! gateway's `Op` (serde tag `"op"`, snake_case) and `StoreDescriptor` (serde tag
//! `"kind"`, snake_case with `gsheet` pinned). `inv_model::{Inventory,FieldValue}`
//! are reused directly since they are the shared contract.

use std::collections::BTreeMap;

use gloo_net::http::Request;
use inv_model::{FieldValue, Inventory};
use serde::{Deserialize, Serialize};

/// Mirror of `inv_store::GSheetMode`.
///
/// serde tag `"mode"`, snake_case variants, with `OAuth` pinned to `"oauth"`
/// (snake_case would otherwise yield `o_auth`). No secrets travel on the wire:
/// the browser only names a public URL or a spreadsheet id plus an access mode;
/// any credential is resolved server-side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum GSheetMode {
    /// Read a published / link-shared sheet (e.g. a CSV-export URL). No credential.
    PublicUrl { url: String },
    /// Read/write a private sheet authorized by a server-side OAuth credential;
    /// only the `spreadsheet_id` is supplied here.
    #[serde(rename = "oauth")]
    OAuth { spreadsheet_id: String },
    /// Read/write using the app's own server-side identity (a service account).
    /// `spreadsheet_id = None` asks the backend to create a fresh spreadsheet.
    AppHosted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spreadsheet_id: Option<String>,
    },
}

/// Mirror of `inv_store::StoreDescriptor`.
///
/// serde tag `"kind"`, snake_case variants, with `GSheet` pinned to `"gsheet"`
/// (snake_case would otherwise yield `g_sheet`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoreDescriptor {
    File {
        path: String,
    },
    Postgres {
        url: String,
    },
    #[serde(rename = "gsheet")]
    GSheet {
        mode: GSheetMode,
    },
}

impl StoreDescriptor {
    /// A short human label for the header / toasts.
    pub fn label(&self) -> String {
        match self {
            StoreDescriptor::File { path } => format!("File: {path}"),
            StoreDescriptor::Postgres { url } => format!("Postgres: {url}"),
            StoreDescriptor::GSheet { mode } => match mode {
                GSheetMode::PublicUrl { url } => format!("Google Sheet (public): {url}"),
                GSheetMode::OAuth { spreadsheet_id } => {
                    format!("Google Sheet (OAuth): {spreadsheet_id}")
                }
                GSheetMode::AppHosted {
                    spreadsheet_id: Some(id),
                } => format!("Google Sheet (app): {id}"),
                GSheetMode::AppHosted {
                    spreadsheet_id: None,
                } => "Google Sheet (app: new)".to_string(),
            },
        }
    }
}

/// Mirror of the gateway's edit patch wire shape (`Op::EditInstance.patch`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PatchWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub set_fields: BTreeMap<String, FieldValue>,
    #[serde(default)]
    pub remove_fields: Vec<String>,
}

/// Mirror of the gateway's remove mode (`"cascade"` | `"reparent"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoveModeWire {
    Cascade,
    Reparent,
}

/// Mirror of `inv_server::Op`. serde tag `"op"`, snake_case variants.
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
}

/// Body of `POST /api/op`.
#[derive(Debug, Clone, Serialize)]
struct OpRequest<'a> {
    store: &'a StoreDescriptor,
    op: &'a Op,
}

/// Body of `POST /api/photo/get`.
#[derive(Debug, Clone, Serialize)]
struct PhotoGetRequest<'a> {
    store: &'a StoreDescriptor,
    key: &'a str,
}

/// An API error: a human-readable message suitable for a toast.
#[derive(Debug, Clone)]
pub struct ApiError(pub String);

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<gloo_net::Error> for ApiError {
    fn from(e: gloo_net::Error) -> Self {
        ApiError(e.to_string())
    }
}

/// `POST /api/inventory` -> `store.load()` as `Inventory`.
pub async fn load_inventory(desc: &StoreDescriptor) -> Result<Inventory, ApiError> {
    let resp = Request::post("/api/inventory")
        .json(desc)
        .map_err(ApiError::from)?
        .send()
        .await
        .map_err(ApiError::from)?;
    json_or_err(resp).await
}

/// `POST /api/op` -> FULL updated `Inventory` (the committed state).
pub async fn apply_op(desc: &StoreDescriptor, op: &Op) -> Result<Inventory, ApiError> {
    let body = OpRequest { store: desc, op };
    let resp = Request::post("/api/op")
        .json(&body)
        .map_err(ApiError::from)?
        .send()
        .await
        .map_err(ApiError::from)?;
    json_or_err(resp).await
}

/// `POST /api/photo/put?store=<json>&key=<key>&mime=<mime>` with raw body bytes.
pub async fn put_photo(
    desc: &StoreDescriptor,
    key: &str,
    bytes: Vec<u8>,
    mime: &str,
) -> Result<(), ApiError> {
    let store_json = serde_json::to_string(desc)
        .map_err(|e| ApiError(format!("serialize descriptor: {e}")))?;
    let store_q = encode_uri_component(&store_json);
    let key_q = encode_uri_component(key);
    let mime_q = encode_uri_component(mime);
    let url = format!("/api/photo/put?store={store_q}&key={key_q}&mime={mime_q}");
    let resp = Request::post(&url)
        .header("Content-Type", mime)
        .body(bytes)
        .map_err(ApiError::from)?
        .send()
        .await
        .map_err(ApiError::from)?;
    if resp.ok() {
        Ok(())
    } else {
        Err(ApiError(error_text(resp).await))
    }
}

/// `POST /api/photo/get` -> raw photo bytes (or `None` on 404).
pub async fn get_photo(desc: &StoreDescriptor, key: &str) -> Result<Option<Vec<u8>>, ApiError> {
    let body = PhotoGetRequest { store: desc, key };
    let resp = Request::post("/api/photo/get")
        .json(&body)
        .map_err(ApiError::from)?
        .send()
        .await
        .map_err(ApiError::from)?;
    if resp.status() == 404 {
        return Ok(None);
    }
    if !resp.ok() {
        return Err(ApiError(error_text(resp).await));
    }
    let bytes = resp.binary().await.map_err(ApiError::from)?;
    Ok(Some(bytes))
}

/// Decode a JSON response body into `T`, or surface the error body as a message.
async fn json_or_err<T: for<'de> Deserialize<'de>>(
    resp: gloo_net::http::Response,
) -> Result<T, ApiError> {
    if resp.ok() {
        resp.json::<T>().await.map_err(ApiError::from)
    } else {
        Err(ApiError(error_text(resp).await))
    }
}

/// Best-effort extraction of an error message from a non-OK response.
async fn error_text(resp: gloo_net::http::Response) -> String {
    let status = resp.status();
    match resp.text().await {
        Ok(t) if !t.is_empty() => t,
        _ => format!("request failed (HTTP {status})"),
    }
}

/// `encodeURIComponent` via the JS global, for building query strings safely.
fn encode_uri_component(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}
