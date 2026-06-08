//! `inv-store`: persistence for [`inv_model::Inventory`].
//!
//! The [`Store`] trait abstracts a backing store behind a single race-free
//! read-modify-write primitive, [`Store::transact`]. A transaction loads the
//! current inventory, hands a `&mut Inventory` to the caller's closure, and
//! atomically commits the result. Concurrent transactors on the same backing
//! store never lose updates: on a detected conflict the transaction reloads the
//! latest state and re-runs the closure (bounded retries).
//!
//! [`FileStore`] is the option-1 backend: a single pretty-JSON document plus a
//! sidecar directory for photo bytes. Race-freedom comes from an OS exclusive
//! advisory file lock held for the whole read-modify-write, and durability from
//! a write-to-temp + atomic-rename commit.

use std::fmt;

use inv_model::Inventory;
use serde::{Deserialize, Serialize};

mod file_store;
mod gsheet;
mod postgres;

pub use file_store::FileStore;
pub use gsheet::GSheetStore;
pub use postgres::PostgresStore;

/// Errors produced by a [`Store`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// A concurrent modification could not be reconciled within the retry budget.
    Conflict,
    /// The requested resource does not exist.
    NotFound,
    /// An I/O failure (file system, etc.). Carries a human-readable message.
    Io(String),
    /// A backend-specific failure (parse error, remote backend, etc.).
    Backend(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Conflict => write!(f, "store conflict: concurrent modification"),
            StoreError::NotFound => write!(f, "not found"),
            StoreError::Io(m) => write!(f, "io error: {m}"),
            StoreError::Backend(m) => write!(f, "backend error: {m}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// A pluggable, race-free persistence backend for an [`Inventory`].
///
/// `Store` is `Send + Sync` so a `Box<dyn Store>` can be shared across threads
/// and driven from async code via `tokio::task::spawn_blocking` (the gateway
/// runs every store op on a blocking pool).
pub trait Store: Send + Sync {
    /// Load the current inventory. A missing/empty backing store yields
    /// [`Inventory::new`].
    fn load(&self) -> Result<Inventory, StoreError>;

    /// Object-safe core of [`Store::transact`].
    ///
    /// Loads the current inventory, applies `f` to a `&mut Inventory`, and
    /// atomically commits the mutated inventory. If a concurrent transactor
    /// committed in the meantime (conflict), the inventory is reloaded and `f`
    /// is re-run, up to a bounded number of attempts. There are NO lost updates
    /// across concurrent transactors on the same backing store.
    ///
    /// Implementors implement *this* (non-generic) method; callers normally use
    /// the ergonomic generic [`Store::transact`] wrapper, which returns a value
    /// out of the closure. This split keeps [`Store`] dyn-compatible so
    /// `Box<dyn Store>` works (a generic method cannot appear in a vtable).
    fn transact_dyn(
        &self,
        f: &mut dyn FnMut(&mut Inventory) -> Result<(), StoreError>,
    ) -> Result<(), StoreError>;

    /// Fetch the bytes for a photo `key`, or `None` if absent.
    fn get_photo(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError>;

    /// Store (create or overwrite) the bytes for a photo `key`.
    fn put_photo(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError>;

    /// Delete the bytes for a photo `key`. Deleting a missing key is a no-op.
    fn delete_photo(&self, key: &str) -> Result<(), StoreError>;
}

/// Ergonomic, generic transaction helper layered over the dyn-compatible
/// [`Store`] trait.
///
/// [`Store`] itself only exposes the non-generic [`Store::transact_dyn`] so it
/// stays object-safe (`Box<dyn Store>` must work). This extension trait adds the
/// familiar `transact<T>` that returns a value out of the closure. The blanket
/// impl covers every `Store` — including `?Sized` ones like `dyn Store`, so
/// `Box<dyn Store>` and `&dyn Store` get it for free.
pub trait StoreExt: Store {
    /// Run a read-modify-write transaction, returning a value out of the closure.
    ///
    /// Same semantics as [`Store::transact_dyn`]; the closure's `Ok(T)` is
    /// surfaced to the caller after a successful commit.
    fn transact<T>(
        &self,
        f: &mut dyn FnMut(&mut Inventory) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut out: Option<T> = None;
        self.transact_dyn(&mut |inv| {
            // Reset on every (re)run so a retried attempt never keeps a stale
            // value produced by a previous, discarded attempt.
            out = None;
            out = Some(f(inv)?);
            Ok(())
        })?;
        Ok(out.expect("transact_dyn committed without producing a value"))
    }
}

impl<S: Store + ?Sized> StoreExt for S {}

/// How the Google Sheets backend authenticates and which spreadsheet it targets.
///
/// Crucially, **no secrets travel on the wire**. The browser only ever names a
/// public URL or a spreadsheet id and an *access mode*; any credential
/// (OAuth client/token, service-account key) is read server-side from the
/// environment by [`open`]. This keeps tokens out of the client and out of the
/// gateway's request bodies / store-cache keys.
///
/// Serde tag is `"mode"` with `snake_case` variants, so the wire forms are:
/// `{"mode":"public_url","url":"https://docs.google.com/.../export?format=csv"}`,
/// `{"mode":"oauth","spreadsheet_id":"abc"}`,
/// `{"mode":"app_hosted","spreadsheet_id":"abc"}` (or `spreadsheet_id` omitted /
/// `null` to mean "create a fresh spreadsheet for me").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum GSheetMode {
    /// A read path over a *published / link-shared* sheet reachable without any
    /// credential (e.g. a CSV-export URL). No server env required.
    PublicUrl { url: String },
    /// Read/write a private sheet authorized by **server-side** OAuth credentials
    /// (the browser never sends a token). The credential is loaded from the
    /// environment by [`open`]; only the `spreadsheet_id` is supplied here.
    ///
    /// `rename_all = "snake_case"` would map this to `o_auth`; we pin it to
    /// `oauth` to match the agreed wire contract.
    #[serde(rename = "oauth")]
    OAuth { spreadsheet_id: String },
    /// Read/write using the app's own **server-side** identity (a Google service
    /// account). `spreadsheet_id = None` asks the backend to create a brand-new
    /// spreadsheet owned by the app and use that.
    AppHosted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spreadsheet_id: Option<String>,
    },
}

/// A serializable description of *which* backing store to open and how to reach
/// it. The bring-your-own-database UI sends one of these with every request; the
/// gateway uses [`open`] to obtain (and cache) a live [`Store`].
///
/// Serde tag is `"kind"` with `snake_case` variants, so the wire forms are:
/// `{"kind":"file","path":"..."}`,
/// `{"kind":"postgres","url":"..."}`,
/// `{"kind":"gsheet","mode":{"mode":"public_url","url":"..."}}` (and the other
/// [`GSheetMode`] shapes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoreDescriptor {
    /// A [`FileStore`] over the JSON document at `path`.
    File { path: String },
    /// A [`PostgresStore`] reachable at the connection `url`.
    Postgres { url: String },
    /// A [`GSheetStore`] over a Google Sheet, reached via one of three access
    /// [`modes`](GSheetMode). Secrets are never carried here — they are resolved
    /// server-side from the environment by [`open`].
    ///
    /// `rename_all = "snake_case"` would map this variant to `g_sheet`; we pin it
    /// to `gsheet` to match the agreed wire contract.
    #[serde(rename = "gsheet")]
    GSheet { mode: GSheetMode },
}

/// Open the [`Store`] described by `desc`.
///
/// `File` yields a working [`FileStore`]. `Postgres` and `GSheet` yield their
/// (currently stubbed) adapters, which route correctly but return
/// [`StoreError::Backend`] from their methods until implemented.
///
/// For [`GSheetMode::OAuth`] / [`GSheetMode::AppHosted`] the required credential
/// is read from the **server environment** here (never from the request):
///
/// * `INV_GSHEET_OAUTH_CLIENT` — OAuth client config (path to JSON, or inline JSON)
/// * `INV_GSHEET_TOKEN` — a pre-obtained OAuth access/refresh token
/// * `INV_GSHEET_SERVICE_ACCOUNT` — service-account key (path to JSON, or inline JSON)
///
/// If a mode needs a credential that is not configured, the constructor returns
/// [`StoreError::Backend`] with an actionable message naming the missing env var.
/// [`GSheetMode::PublicUrl`] needs no credential.
pub fn open(desc: &StoreDescriptor) -> Result<Box<dyn Store>, StoreError> {
    match desc {
        StoreDescriptor::File { path } => Ok(Box::new(FileStore::new(path))),
        StoreDescriptor::Postgres { url } => Ok(Box::new(PostgresStore::open(url)?)),
        StoreDescriptor::GSheet { mode } => {
            let store = match mode {
                GSheetMode::PublicUrl { url } => GSheetStore::public_url(url)?,
                GSheetMode::OAuth { spreadsheet_id } => GSheetStore::oauth(spreadsheet_id)?,
                GSheetMode::AppHosted { spreadsheet_id } => {
                    GSheetStore::app_hosted(spreadsheet_id.clone())?
                }
            };
            Ok(Box::new(store))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inv_core::InventoryExt;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    /// `Store` must be object-safe AND `Send + Sync` so `Box<dyn Store>` can be
    /// shared across threads / used from `spawn_blocking`.
    fn _assert_box_dyn_store_send_sync() {
        fn require_send_sync<T: Send + Sync>() {}
        require_send_sync::<Box<dyn Store>>();
    }

    #[test]
    fn open_file_yields_a_working_store() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("inv.json");
        let desc = StoreDescriptor::File {
            path: path.to_string_lossy().into_owned(),
        };

        let store = open(&desc).expect("open(File) should succeed");

        // A fresh store loads an empty inventory.
        let inv = store.load().expect("load");
        assert!(inv.instances.is_empty());
        assert_eq!(inv.next_id, 1);

        // A transaction round-trips through the real FileStore.
        let id = store
            .transact(&mut |inv| {
                inv.add_instance("Item", "thing", BTreeMap::new(), None, 1)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            })
            .expect("transact");
        assert_eq!(id, 1);

        let inv = store.load().expect("reload");
        assert_eq!(inv.get(1).unwrap().name, "thing");

        // Photo bytes round-trip too.
        store.put_photo("1-0", b"hello").expect("put_photo");
        assert_eq!(store.get_photo("1-0").unwrap().as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn descriptor_serde_wire_shapes() {
        let file: StoreDescriptor =
            serde_json::from_str(r#"{"kind":"file","path":"/tmp/x.json"}"#).unwrap();
        assert_eq!(
            file,
            StoreDescriptor::File {
                path: "/tmp/x.json".to_string()
            }
        );

        let pg: StoreDescriptor =
            serde_json::from_str(r#"{"kind":"postgres","url":"postgres://h/db"}"#).unwrap();
        assert_eq!(
            pg,
            StoreDescriptor::Postgres {
                url: "postgres://h/db".to_string()
            }
        );

        // Public-URL mode: no secret on the wire, just a URL.
        let gs_public: StoreDescriptor = serde_json::from_str(
            r#"{"kind":"gsheet","mode":{"mode":"public_url","url":"https://x/csv"}}"#,
        )
        .unwrap();
        assert_eq!(
            gs_public,
            StoreDescriptor::GSheet {
                mode: GSheetMode::PublicUrl {
                    url: "https://x/csv".to_string()
                }
            }
        );

        // OAuth mode: spreadsheet id only; the token lives server-side.
        let gs_oauth: StoreDescriptor = serde_json::from_str(
            r#"{"kind":"gsheet","mode":{"mode":"oauth","spreadsheet_id":"abc"}}"#,
        )
        .unwrap();
        assert_eq!(
            gs_oauth,
            StoreDescriptor::GSheet {
                mode: GSheetMode::OAuth {
                    spreadsheet_id: "abc".to_string()
                }
            }
        );

        // App-hosted with an explicit id.
        let gs_app: StoreDescriptor = serde_json::from_str(
            r#"{"kind":"gsheet","mode":{"mode":"app_hosted","spreadsheet_id":"abc"}}"#,
        )
        .unwrap();
        assert_eq!(
            gs_app,
            StoreDescriptor::GSheet {
                mode: GSheetMode::AppHosted {
                    spreadsheet_id: Some("abc".to_string())
                }
            }
        );

        // App-hosted with no id == "create a new sheet for me". The id field may
        // be omitted entirely, and round-trips back to the omitted form.
        let gs_app_new: StoreDescriptor =
            serde_json::from_str(r#"{"kind":"gsheet","mode":{"mode":"app_hosted"}}"#).unwrap();
        assert_eq!(
            gs_app_new,
            StoreDescriptor::GSheet {
                mode: GSheetMode::AppHosted {
                    spreadsheet_id: None
                }
            }
        );
        assert_eq!(
            serde_json::to_value(&gs_app_new).unwrap(),
            serde_json::json!({"kind":"gsheet","mode":{"mode":"app_hosted"}})
        );
    }

    #[test]
    fn open_postgres_routes_to_stub() {
        let store = open(&StoreDescriptor::Postgres {
            url: "postgres://localhost/db".to_string(),
        })
        .expect("factory routes even though adapter is a stub");
        match store.load() {
            Err(StoreError::Backend(m)) => assert!(m.contains("postgres")),
            other => panic!("expected Backend(postgres...), got {other:?}"),
        }
    }

    #[test]
    fn open_gsheet_public_url_routes_to_stub() {
        // PublicUrl needs no credential, so open() always succeeds and routes to
        // the (stubbed) adapter, which reports a gsheet backend error on use.
        let store = open(&StoreDescriptor::GSheet {
            mode: GSheetMode::PublicUrl {
                url: "https://example/csv".to_string(),
            },
        })
        .expect("factory routes even though adapter is a stub");
        match store.load() {
            Err(StoreError::Backend(m)) => assert!(m.contains("gsheet")),
            other => panic!("expected Backend(gsheet...), got {other:?}"),
        }
    }

    #[test]
    fn open_gsheet_oauth_without_credential_is_actionable_error() {
        // With no server-side OAuth credential configured, opening an OAuth-mode
        // sheet fails fast with a clear, actionable message naming the env var.
        // (Guard against a credential leaking in from the ambient environment.)
        if std::env::var_os("INV_GSHEET_TOKEN").is_some()
            || std::env::var_os("INV_GSHEET_OAUTH_CLIENT").is_some()
        {
            return;
        }
        let res = open(&StoreDescriptor::GSheet {
            mode: GSheetMode::OAuth {
                spreadsheet_id: "sid".to_string(),
            },
        });
        match res {
            Err(StoreError::Backend(m)) => {
                assert!(m.contains("INV_GSHEET"), "message names the env var: {m}");
            }
            Err(other) => panic!("expected Backend(... INV_GSHEET ...), got {other:?}"),
            Ok(_) => panic!("expected an error when no OAuth credential is configured"),
        }
    }

    #[test]
    fn open_gsheet_app_hosted_without_credential_is_actionable_error() {
        if std::env::var_os("INV_GSHEET_SERVICE_ACCOUNT").is_some() {
            return;
        }
        let res = open(&StoreDescriptor::GSheet {
            mode: GSheetMode::AppHosted {
                spreadsheet_id: None,
            },
        });
        match res {
            Err(StoreError::Backend(m)) => {
                assert!(m.contains("INV_GSHEET"), "message names the env var: {m}");
            }
            Err(other) => panic!("expected Backend(... INV_GSHEET ...), got {other:?}"),
            Ok(_) => panic!("expected an error when no service account is configured"),
        }
    }
}
