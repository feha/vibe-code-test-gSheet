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

mod file_store;

pub use file_store::FileStore;

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
pub trait Store {
    /// Load the current inventory. A missing/empty backing store yields
    /// [`Inventory::new`].
    fn load(&self) -> Result<Inventory, StoreError>;

    /// Run a read-modify-write transaction.
    ///
    /// Loads the current inventory, applies `f` to a `&mut Inventory`, and
    /// atomically commits the mutated inventory. If a concurrent transactor
    /// committed in the meantime (conflict), the inventory is reloaded and `f`
    /// is re-run, up to a bounded number of attempts. There are NO lost updates
    /// across concurrent transactors on the same backing store.
    fn transact<T>(
        &self,
        f: &mut dyn FnMut(&mut Inventory) -> Result<T, StoreError>,
    ) -> Result<T, StoreError>;

    /// Fetch the bytes for a photo `key`, or `None` if absent.
    fn get_photo(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError>;

    /// Store (create or overwrite) the bytes for a photo `key`.
    fn put_photo(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError>;

    /// Delete the bytes for a photo `key`. Deleting a missing key is a no-op.
    fn delete_photo(&self, key: &str) -> Result<(), StoreError>;
}
