//! [`GSheetStore`]: a [`Store`](crate::Store) backed by a Google Sheet.
//!
//! This is a **stub**: it compiles so the [`open`](crate::open) factory can route
//! to it, but every method currently returns
//! [`StoreError::Backend`]`("gsheet adapter not yet implemented")`. A later agent
//! fills in the real implementation; the crate's `reqwest` dependency is already
//! declared so only this file needs to change.

use inv_model::Inventory;

use crate::{Store, StoreError};

/// Message returned by every not-yet-implemented method on this stub.
const NYI: &str = "gsheet adapter not yet implemented";

/// A [`Store`](crate::Store) backed by a Google Sheet.
///
/// Currently a stub (see module docs). Holds the spreadsheet id and an OAuth
/// access token so a later implementation can talk to the Sheets API without
/// changing the factory.
pub struct GSheetStore {
    /// The Google spreadsheet id (the long token in the sheet URL).
    #[allow(dead_code)]
    spreadsheet_id: String,
    /// An OAuth 2.0 access token authorizing Sheets API calls.
    #[allow(dead_code)]
    token: String,
}

impl GSheetStore {
    /// Open a store over the Google Sheet identified by `spreadsheet_id`,
    /// authorized by the OAuth `token`.
    ///
    /// The stub never connects, so this cannot fail yet; the signature returns a
    /// `Result` so the real implementation can surface auth/network errors
    /// without touching the [`open`](crate::open) factory.
    pub fn open(spreadsheet_id: &str, token: &str) -> Result<Self, StoreError> {
        Ok(GSheetStore {
            spreadsheet_id: spreadsheet_id.to_string(),
            token: token.to_string(),
        })
    }
}

impl Store for GSheetStore {
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
