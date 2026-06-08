//! [`PostgresStore`]: a [`Store`](crate::Store) backed by a PostgreSQL database.
//!
//! This is a **stub**: it compiles so the [`open`](crate::open) factory can route
//! to it, but every method currently returns
//! [`StoreError::Backend`]`("postgres adapter not yet implemented")`. A later
//! agent fills in the real implementation; the crate's dependencies (`postgres`,
//! `r2d2`, `r2d2_postgres`) are already declared so only this file needs to change.

use inv_model::Inventory;

use crate::{Store, StoreError};

/// Message returned by every not-yet-implemented method on this stub.
const NYI: &str = "postgres adapter not yet implemented";

/// A [`Store`](crate::Store) backed by PostgreSQL.
///
/// Currently a stub (see module docs). Holds the connection URL so a later
/// implementation can build a pool from it without changing the factory.
pub struct PostgresStore {
    /// The PostgreSQL connection URL (e.g. `postgres://user:pass@host/db`).
    #[allow(dead_code)]
    url: String,
}

impl PostgresStore {
    /// Open a store over the database at `url`.
    ///
    /// The stub never connects, so this cannot fail yet; the signature returns a
    /// `Result` so the real implementation can surface connection errors without
    /// touching the [`open`](crate::open) factory.
    pub fn open(url: &str) -> Result<Self, StoreError> {
        Ok(PostgresStore {
            url: url.to_string(),
        })
    }
}

impl Store for PostgresStore {
    fn load(&self) -> Result<Inventory, StoreError> {
        Err(StoreError::Backend(NYI.to_string()))
    }

    fn transact_dyn(
        &self,
        _f: &mut dyn FnMut(&mut Inventory) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        Err(StoreError::Backend(NYI.to_string()))
    }

    fn get_photo(&self, _key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        Err(StoreError::Backend(NYI.to_string()))
    }

    fn put_photo(&self, _key: &str, _bytes: &[u8]) -> Result<(), StoreError> {
        Err(StoreError::Backend(NYI.to_string()))
    }

    fn delete_photo(&self, _key: &str) -> Result<(), StoreError> {
        Err(StoreError::Backend(NYI.to_string()))
    }
}
