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

/// A serializable description of *which* backing store to open and how to reach
/// it. The bring-your-own-database UI sends one of these with every request; the
/// gateway uses [`open`] to obtain (and cache) a live [`Store`].
///
/// Serde tag is `"kind"` with `snake_case` variants, so the wire forms are:
/// `{"kind":"file","path":"..."}`,
/// `{"kind":"postgres","url":"..."}`,
/// `{"kind":"gsheet","spreadsheet_id":"...","token":"..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoreDescriptor {
    /// A [`FileStore`] over the JSON document at `path`.
    File { path: String },
    /// A [`PostgresStore`] reachable at the connection `url`.
    Postgres { url: String },
    /// A [`GSheetStore`] over the Google Sheet `spreadsheet_id`, authorized by
    /// the OAuth access `token`.
    ///
    /// `rename_all = "snake_case"` would map this variant to `g_sheet`; we pin it
    /// to `gsheet` to match the agreed wire contract.
    #[serde(rename = "gsheet")]
    GSheet {
        spreadsheet_id: String,
        token: String,
    },
}

/// Open the [`Store`] described by `desc`.
///
/// `File` yields a working [`FileStore`]. `Postgres` and `GSheet` yield their
/// (currently stubbed) adapters, which route correctly but return
/// [`StoreError::Backend`] from their methods until implemented.
pub fn open(desc: &StoreDescriptor) -> Result<Box<dyn Store>, StoreError> {
    match desc {
        StoreDescriptor::File { path } => Ok(Box::new(FileStore::new(path))),
        StoreDescriptor::Postgres { url } => Ok(Box::new(PostgresStore::open(url)?)),
        StoreDescriptor::GSheet {
            spreadsheet_id,
            token,
        } => Ok(Box::new(GSheetStore::open(spreadsheet_id, token)?)),
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

        let gs: StoreDescriptor = serde_json::from_str(
            r#"{"kind":"gsheet","spreadsheet_id":"abc","token":"tok"}"#,
        )
        .unwrap();
        assert_eq!(
            gs,
            StoreDescriptor::GSheet {
                spreadsheet_id: "abc".to_string(),
                token: "tok".to_string()
            }
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
    fn open_gsheet_routes_to_stub() {
        let store = open(&StoreDescriptor::GSheet {
            spreadsheet_id: "sid".to_string(),
            token: "tok".to_string(),
        })
        .expect("factory routes even though adapter is a stub");
        match store.load() {
            Err(StoreError::Backend(m)) => assert!(m.contains("gsheet")),
            other => panic!("expected Backend(gsheet...), got {other:?}"),
        }
    }
}
