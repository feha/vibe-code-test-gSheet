//! [`GSheetStore`]: a [`Store`](crate::Store) backed by a Google Sheet.
//!
//! ## Design: a JSON document in two cells, with optimistic concurrency
//!
//! Google Sheets has no transactions and no compare-and-swap, so we model the
//! whole [`Inventory`] as a single JSON string parked in a fixed cell, paired
//! with a monotonically increasing version number in the cell below it:
//!
//! ```text
//! Data!A1  = the Inventory serialized as a JSON string
//! Data!A2  = the version number (an integer, as text)
//! ```
//!
//! * [`load`](GSheetStore::load) does a `spreadsheets.values.get` on `Data!A1:A2`,
//!   parses `A1` as an [`Inventory`] (or [`Inventory::new`] when empty).
//! * [`transact_dyn`](GSheetStore::transact_dyn) implements **optimistic
//!   concurrency** with a bounded read-verify-write retry loop:
//!     1. read `(json, version)` from `A1:A2`;
//!     2. deserialize, run the caller's closure on the `&mut Inventory`;
//!     3. **re-read** `A2`; if it still equals the version we read, write back
//!        `A1 = new json`, `A2 = version + 1`; otherwise a concurrent transactor
//!        committed, so reload and retry;
//!     4. exhausting the retry budget yields [`StoreError::Conflict`].
//!
//! ### Residual race window (documented, honest)
//!
//! Because Sheets offers no atomic CAS, the verify (re-read `A2`) and the write
//! are two separate HTTP calls. Two transactors that both observe the same
//! version and both pass the verify check can still race between verify and
//! write, the last writer winning and losing the other's update. This loop is
//! therefore *best-effort* conflict avoidance, not a true serializing lock — it
//! shrinks, but does not eliminate, the lost-update window. (The realistic fix
//! would be a backend with real CAS; Sheets is not one.)
//!
//! ## Photos
//!
//! A Sheets cell caps at roughly 50k characters — far too small for image bytes —
//! so photo operations are explicitly unsupported and return
//! [`StoreError::Backend`] rather than silently corrupting data.
//!
//! ## Testability
//!
//! The HTTP concern is isolated behind the [`Transport`] trait: request URL/body
//! building and response parsing are pure functions ([`values_get_url`],
//! [`values_update_url`], [`auth_header`], [`parse_values_get`],
//! [`update_body`]), and the optimistic retry loop is driven through `Transport`
//! so it can be exercised with an in-memory fake. The live path uses
//! [`ReqwestTransport`] (reqwest blocking); it is **unverified in CI** because no
//! Google credentials are available — it is gated behind the
//! `GSHEET_TEST_SPREADSHEET_ID` / `GSHEET_TEST_TOKEN` env vars.

use inv_model::Inventory;
use serde_json::Value;

use crate::{Store, StoreError};

/// Base URL of the Google Sheets API v4.
const SHEETS_API_BASE: &str = "https://sheets.googleapis.com/v4/spreadsheets";

/// The fixed range holding the inventory JSON (`A1`) and version (`A2`).
const DATA_RANGE: &str = "Data!A1:A2";

/// Maximum number of read-verify-write attempts before giving up with
/// [`StoreError::Conflict`].
const MAX_ATTEMPTS: usize = 16;

/// Message returned by photo operations, which this backend cannot support.
const PHOTOS_UNSUPPORTED: &str = "photos are not supported on the Google Sheet backend";

// ---------------------------------------------------------------------------
// Pure helpers (no network): URL + header + body building, response parsing.
// These are the unit-tested seam.
// ---------------------------------------------------------------------------

/// Percent-encode a value-range path segment for a Sheets API URL.
///
/// Sheet ranges contain `!` and `:` which must be escaped in a URL path
/// segment. We intentionally encode the small, known set of characters that
/// appear in our fixed ranges rather than pulling in a urlencoding crate.
fn encode_range(range: &str) -> String {
    let mut out = String::with_capacity(range.len() + 4);
    for b in range.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => {
                out.push('%');
                out.push_str(&format!("{other:02X}"));
            }
        }
    }
    out
}

/// Build the `spreadsheets.values.get` URL for `range` in `spreadsheet_id`.
fn values_get_url(spreadsheet_id: &str, range: &str) -> String {
    format!(
        "{SHEETS_API_BASE}/{}/values/{}",
        encode_range(spreadsheet_id),
        encode_range(range)
    )
}

/// Build the `spreadsheets.values.update` URL for `range` in `spreadsheet_id`.
///
/// `valueInputOption=RAW` stores the JSON string verbatim (no formula/number
/// coercion by Sheets).
fn values_update_url(spreadsheet_id: &str, range: &str) -> String {
    format!(
        "{SHEETS_API_BASE}/{}/values/{}?valueInputOption=RAW",
        encode_range(spreadsheet_id),
        encode_range(range)
    )
}

/// Build the `Authorization` header value for the OAuth bearer `token`.
fn auth_header(token: &str) -> String {
    format!("Bearer {token}")
}

/// Build the JSON request body for a `values.update` writing `json` to `A1` and
/// `version` to `A2` of the given `range`.
fn update_body(range: &str, json: &str, version: u64) -> Value {
    serde_json::json!({
        "range": range,
        "majorDimension": "ROWS",
        "values": [[json], [version.to_string()]],
    })
}

/// Parse a `spreadsheets.values.get` response body into `(inventory_json, version)`.
///
/// The Sheets API returns `{"range":..,"values":[["<json>"],["<version>"]]}`.
/// Missing rows (a never-written sheet) are treated as empty json / version 0.
/// Returns the raw inventory JSON string (empty string when the cell is absent)
/// and the parsed version.
fn parse_values_get(body: &Value) -> Result<(String, u64), StoreError> {
    let rows = match body.get("values") {
        Some(Value::Array(rows)) => rows.as_slice(),
        // No `values` key at all => range is entirely empty.
        None => &[],
        Some(other) => {
            return Err(StoreError::Backend(format!(
                "gsheet: unexpected `values` shape: {other}"
            )));
        }
    };

    let cell = |row_idx: usize| -> String {
        rows.get(row_idx)
            .and_then(|r| r.as_array())
            .and_then(|cells| cells.first())
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string()
    };

    let json = cell(0);
    let version_str = cell(1);
    let version = if version_str.trim().is_empty() {
        0
    } else {
        version_str.trim().parse::<u64>().map_err(|e| {
            StoreError::Backend(format!("gsheet: bad version cell {version_str:?}: {e}"))
        })?
    };

    Ok((json, version))
}

/// Deserialize the inventory-JSON cell into an [`Inventory`], treating an empty
/// cell as a brand-new inventory.
fn inventory_from_cell(json: &str) -> Result<Inventory, StoreError> {
    if json.trim().is_empty() {
        return Ok(Inventory::new());
    }
    Inventory::from_json_bytes(json.as_bytes())
        .map_err(|e| StoreError::Backend(format!("gsheet: parse inventory cell: {e}")))
}

/// Serialize an [`Inventory`] to the compact JSON string stored in a cell.
fn inventory_to_cell(inv: &Inventory) -> Result<String, StoreError> {
    serde_json::to_string(inv)
        .map_err(|e| StoreError::Backend(format!("gsheet: serialize inventory: {e}")))
}

// ---------------------------------------------------------------------------
// Transport seam: the only thing that touches the network.
// ---------------------------------------------------------------------------

/// The minimal HTTP surface the store needs. Implemented for real by
/// [`ReqwestTransport`]; implemented by a fake in tests so the optimistic retry
/// loop runs without a network.
trait Transport: Send + Sync {
    /// `GET url` with the given bearer `token`, returning the parsed JSON body.
    fn get_json(&self, url: &str, token: &str) -> Result<Value, StoreError>;

    /// `PUT url` with the given bearer `token` and JSON `body`. The response body
    /// is ignored on success.
    fn put_json(&self, url: &str, token: &str, body: &Value) -> Result<(), StoreError>;
}

/// The live [`Transport`] over `reqwest::blocking`.
struct ReqwestTransport {
    client: reqwest::blocking::Client,
}

impl ReqwestTransport {
    fn new() -> Result<Self, StoreError> {
        let client = reqwest::blocking::Client::builder()
            .build()
            .map_err(|e| StoreError::Backend(format!("gsheet: build http client: {e}")))?;
        Ok(ReqwestTransport { client })
    }
}

impl Transport for ReqwestTransport {
    fn get_json(&self, url: &str, token: &str) -> Result<Value, StoreError> {
        let resp = self
            .client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, auth_header(token))
            .send()
            .map_err(|e| StoreError::Backend(format!("gsheet: GET {url}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            return Err(StoreError::Backend(format!(
                "gsheet: GET {url} -> {status}: {text}"
            )));
        }
        resp.json::<Value>()
            .map_err(|e| StoreError::Backend(format!("gsheet: decode GET {url}: {e}")))
    }

    fn put_json(&self, url: &str, token: &str, body: &Value) -> Result<(), StoreError> {
        let resp = self
            .client
            .put(url)
            .header(reqwest::header::AUTHORIZATION, auth_header(token))
            .json(body)
            .send()
            .map_err(|e| StoreError::Backend(format!("gsheet: PUT {url}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            return Err(StoreError::Backend(format!(
                "gsheet: PUT {url} -> {status}: {text}"
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The store.
// ---------------------------------------------------------------------------

/// A [`Store`](crate::Store) backed by a Google Sheet.
///
/// Holds the spreadsheet id, an OAuth access token, and a [`Transport`] (so the
/// whole struct is `Send + Sync`: it carries a connection-ish handle, not a bare
/// non-`Sync` client). See the module docs for the data layout and concurrency
/// model.
pub struct GSheetStore {
    /// The Google spreadsheet id (the long token in the sheet URL).
    spreadsheet_id: String,
    /// An OAuth 2.0 access token authorizing Sheets API calls.
    token: String,
    /// The HTTP transport (real reqwest in production; injectable for tests).
    transport: Box<dyn Transport>,
}

impl GSheetStore {
    /// Open a store over the Google Sheet identified by `spreadsheet_id`,
    /// authorized by the OAuth `token`. Builds the live reqwest transport.
    pub fn open(spreadsheet_id: &str, token: &str) -> Result<Self, StoreError> {
        Ok(GSheetStore {
            spreadsheet_id: spreadsheet_id.to_string(),
            token: token.to_string(),
            transport: Box::new(ReqwestTransport::new()?),
        })
    }

    /// Construct over an arbitrary [`Transport`] (test seam).
    #[cfg(test)]
    fn with_transport(spreadsheet_id: &str, token: &str, transport: Box<dyn Transport>) -> Self {
        GSheetStore {
            spreadsheet_id: spreadsheet_id.to_string(),
            token: token.to_string(),
            transport,
        }
    }

    /// Read the `(inventory, version)` currently in the sheet.
    fn read_state(&self) -> Result<(Inventory, u64), StoreError> {
        let url = values_get_url(&self.spreadsheet_id, DATA_RANGE);
        let body = self.transport.get_json(&url, &self.token)?;
        let (json, version) = parse_values_get(&body)?;
        let inv = inventory_from_cell(&json)?;
        Ok((inv, version))
    }

    /// Read just the version cell (`A2`) for the optimistic verify step.
    fn read_version(&self) -> Result<u64, StoreError> {
        let url = values_get_url(&self.spreadsheet_id, DATA_RANGE);
        let body = self.transport.get_json(&url, &self.token)?;
        let (_json, version) = parse_values_get(&body)?;
        Ok(version)
    }

    /// Write `inv` at version `new_version` back to `A1:A2`.
    fn write_state(&self, inv: &Inventory, new_version: u64) -> Result<(), StoreError> {
        let json = inventory_to_cell(inv)?;
        let url = values_update_url(&self.spreadsheet_id, DATA_RANGE);
        let body = update_body(DATA_RANGE, &json, new_version);
        self.transport.put_json(&url, &self.token, &body)
    }
}

impl Store for GSheetStore {
    fn load(&self) -> Result<Inventory, StoreError> {
        let (inv, _version) = self.read_state()?;
        Ok(inv)
    }

    fn transact_dyn(
        &self,
        f: &mut dyn FnMut(&mut Inventory) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        for _ in 0..MAX_ATTEMPTS {
            // 1. Read current state + the version we are racing against.
            let (mut inv, version) = self.read_state()?;

            // 2. Run the caller's mutation.
            f(&mut inv)?;

            // 3. Optimistic verify: re-read the version; only commit if it has
            //    not moved since step 1. If it moved, a concurrent transactor
            //    won — reload and retry.
            let observed = self.read_version()?;
            if observed != version {
                continue;
            }

            // 4. Commit at version + 1.
            self.write_state(&inv, version + 1)?;
            return Ok(());
        }
        // Exhausted the retry budget under sustained contention.
        Err(StoreError::Conflict)
    }

    fn get_photo(&self, _key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        Err(StoreError::Backend(PHOTOS_UNSUPPORTED.to_string()))
    }

    fn put_photo(&self, _key: &str, _bytes: &[u8]) -> Result<(), StoreError> {
        Err(StoreError::Backend(PHOTOS_UNSUPPORTED.to_string()))
    }

    fn delete_photo(&self, _key: &str) -> Result<(), StoreError> {
        Err(StoreError::Backend(PHOTOS_UNSUPPORTED.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StoreExt;
    use inv_core::InventoryExt;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    // --- pure helper tests -------------------------------------------------

    #[test]
    fn url_and_auth_construction() {
        // values.get URL escapes `!` and `:` in the range.
        let get = values_get_url("sheet-id", "Data!A1:A2");
        assert_eq!(
            get,
            "https://sheets.googleapis.com/v4/spreadsheets/sheet-id/values/Data%21A1%3AA2"
        );

        // values.update URL carries the RAW input option.
        let put = values_update_url("sheet-id", "Data!A1:A2");
        assert_eq!(
            put,
            "https://sheets.googleapis.com/v4/spreadsheets/sheet-id/values/Data%21A1%3AA2?valueInputOption=RAW"
        );

        // Auth header is a bearer token.
        assert_eq!(auth_header("tok123"), "Bearer tok123");
    }

    #[test]
    fn update_body_shape() {
        let body = update_body("Data!A1:A2", "{\"x\":1}", 7);
        assert_eq!(body["range"], "Data!A1:A2");
        assert_eq!(body["majorDimension"], "ROWS");
        // A1 holds the json string; A2 holds the version as text.
        assert_eq!(body["values"][0][0], "{\"x\":1}");
        assert_eq!(body["values"][1][0], "7");
    }

    #[test]
    fn inventory_cell_json_roundtrip() {
        let mut inv = Inventory::new();
        inv.add_instance("Item", "thing", BTreeMap::new(), None, 42)
            .unwrap();

        let cell = inventory_to_cell(&inv).unwrap();
        let back = inventory_from_cell(&cell).unwrap();
        assert_eq!(inv, back);
    }

    #[test]
    fn empty_cell_is_fresh_inventory() {
        let inv = inventory_from_cell("").unwrap();
        assert_eq!(inv, Inventory::new());
        let inv2 = inventory_from_cell("   ").unwrap();
        assert_eq!(inv2, Inventory::new());
    }

    #[test]
    fn parse_values_get_full_rows() {
        let body = serde_json::json!({
            "range": "Data!A1:A2",
            "majorDimension": "ROWS",
            "values": [["{\"hello\":1}"], ["5"]],
        });
        let (json, version) = parse_values_get(&body).unwrap();
        assert_eq!(json, "{\"hello\":1}");
        assert_eq!(version, 5);
    }

    #[test]
    fn parse_values_get_missing_rows_defaults() {
        // Never-written sheet: no `values` key at all.
        let (json, version) = parse_values_get(&serde_json::json!({"range": "Data!A1:A2"})).unwrap();
        assert_eq!(json, "");
        assert_eq!(version, 0);

        // Only the json row present, version cell absent.
        let body = serde_json::json!({ "values": [["{}"]] });
        let (json, version) = parse_values_get(&body).unwrap();
        assert_eq!(json, "{}");
        assert_eq!(version, 0);
    }

    #[test]
    fn parse_values_get_bad_version_errs() {
        let body = serde_json::json!({ "values": [["{}"], ["not-a-number"]] });
        match parse_values_get(&body) {
            Err(StoreError::Backend(m)) => assert!(m.contains("bad version")),
            other => panic!("expected Backend(bad version), got {other:?}"),
        }
    }

    // --- fake transport for driving the optimistic loop -------------------

    /// A scripted fake transport. It serves a sequence of GET responses (one per
    /// call) and records PUTs. The version cell is what drives the optimistic
    /// decision, so the script lets a test simulate "version changed underneath
    /// us between read and verify".
    struct FakeTransport {
        /// Queue of JSON bodies to return from successive `get_json` calls.
        get_responses: Mutex<Vec<Value>>,
        /// Recorded `(url, body)` of each `put_json` call.
        puts: Mutex<Vec<(String, Value)>>,
        /// Recorded bearer tokens seen, to assert auth wiring.
        seen_tokens: Mutex<Vec<String>>,
    }

    impl FakeTransport {
        fn new(get_responses: Vec<Value>) -> Self {
            FakeTransport {
                get_responses: Mutex::new(get_responses),
                puts: Mutex::new(Vec::new()),
                seen_tokens: Mutex::new(Vec::new()),
            }
        }
    }

    // Implement `Transport` for `Arc<FakeTransport>` so a test can keep one Arc
    // to inspect recorded puts/tokens while handing a clone to the store. This
    // avoids any unsafe aliasing.
    impl Transport for Arc<FakeTransport> {
        fn get_json(&self, url: &str, token: &str) -> Result<Value, StoreError> {
            (**self).get_json(url, token)
        }
        fn put_json(&self, url: &str, token: &str, body: &Value) -> Result<(), StoreError> {
            (**self).put_json(url, token, body)
        }
    }

    impl Transport for FakeTransport {
        fn get_json(&self, _url: &str, token: &str) -> Result<Value, StoreError> {
            self.seen_tokens.lock().unwrap().push(token.to_string());
            let mut q = self.get_responses.lock().unwrap();
            if q.is_empty() {
                return Err(StoreError::Backend("fake: GET queue exhausted".into()));
            }
            Ok(q.remove(0))
        }

        fn put_json(&self, url: &str, token: &str, body: &Value) -> Result<(), StoreError> {
            self.seen_tokens.lock().unwrap().push(token.to_string());
            self.puts
                .lock()
                .unwrap()
                .push((url.to_string(), body.clone()));
            Ok(())
        }
    }

    /// Build a values.get body for a given inventory json + version.
    fn get_body(json: &str, version: u64) -> Value {
        serde_json::json!({
            "range": DATA_RANGE,
            "values": [[json], [version.to_string()]],
        })
    }

    #[test]
    fn transact_commits_when_version_unchanged() {
        // read_state -> version 3; verify read -> version 3 (unchanged) -> write.
        let fake = Arc::new(FakeTransport::new(vec![get_body("", 3), get_body("", 3)]));
        let store = GSheetStore::with_transport("sid", "tok", Box::new(fake.clone()));

        let id = store
            .transact(&mut |inv| {
                inv.add_instance("Item", "thing", BTreeMap::new(), None, 1)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            })
            .expect("transact should commit");
        assert_eq!(id, 1);

        let puts = fake.puts.lock().unwrap();
        assert_eq!(puts.len(), 1, "exactly one commit");
        let (url, body) = &puts[0];
        assert_eq!(
            url,
            "https://sheets.googleapis.com/v4/spreadsheets/sid/values/Data%21A1%3AA2?valueInputOption=RAW"
        );
        // Committed at version 3 + 1 = 4.
        assert_eq!(body["values"][1][0], "4");
        // The committed json must contain the new instance.
        let committed = body["values"][0][0].as_str().unwrap();
        let inv = inventory_from_cell(committed).unwrap();
        assert_eq!(inv.get(1).unwrap().name, "thing");

        // Auth header wiring: every call used the bearer token.
        let tokens = fake.seen_tokens.lock().unwrap();
        assert!(tokens.iter().all(|t| t == "tok"));
    }

    #[test]
    fn transact_retries_then_commits_when_version_moves() {
        // Attempt 1: read v3, verify sees v4 (someone committed) -> reload/retry.
        // Attempt 2: read v4, verify sees v4 (stable) -> commit at v5.
        let fake = Arc::new(FakeTransport::new(vec![
            get_body("", 3), // attempt 1 read_state
            get_body("", 4), // attempt 1 verify -> changed -> retry
            get_body("", 4), // attempt 2 read_state
            get_body("", 4), // attempt 2 verify -> stable
        ]));
        let store = GSheetStore::with_transport("sid", "tok", Box::new(fake.clone()));

        store
            .transact(&mut |inv| {
                inv.add_instance("Item", "thing", BTreeMap::new(), None, 1)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            })
            .expect("transact should eventually commit");

        let puts = fake.puts.lock().unwrap();
        assert_eq!(puts.len(), 1, "only the successful attempt writes");
        // Committed at v4 + 1 = 5.
        assert_eq!(puts[0].1["values"][1][0], "5");
    }

    #[test]
    fn transact_exhausts_retries_into_conflict() {
        // Every verify sees a version different from the one just read, so no
        // attempt ever commits; after MAX_ATTEMPTS the store gives up.
        // Each attempt consumes 2 GETs (read_state + verify). Make the verify
        // always disagree by alternating versions.
        let mut responses = Vec::new();
        for i in 0..MAX_ATTEMPTS {
            let read_v = (i as u64) * 2 + 1; // read_state version
            let verify_v = read_v + 1; // verify sees a different version
            responses.push(get_body("", read_v));
            responses.push(get_body("", verify_v));
        }
        let fake = Arc::new(FakeTransport::new(responses));
        let store = GSheetStore::with_transport("sid", "tok", Box::new(fake.clone()));

        let res = store.transact(&mut |inv| {
            inv.add_instance("Item", "thing", BTreeMap::new(), None, 1)
                .map_err(|e| StoreError::Backend(e.to_string()))
        });
        assert_eq!(res, Err(StoreError::Conflict));

        assert!(
            fake.puts.lock().unwrap().is_empty(),
            "no commit should ever land"
        );
    }

    #[test]
    fn load_parses_inventory() {
        let mut inv = Inventory::new();
        inv.add_instance("Item", "thing", BTreeMap::new(), None, 1)
            .unwrap();
        let json = inventory_to_cell(&inv).unwrap();
        let fake = FakeTransport::new(vec![get_body(&json, 9)]);
        let store = GSheetStore::with_transport("sid", "tok", Box::new(fake));

        let loaded = store.load().expect("load");
        assert_eq!(loaded, inv);
    }

    #[test]
    fn load_empty_sheet_is_fresh() {
        let fake = FakeTransport::new(vec![serde_json::json!({"range": DATA_RANGE})]);
        let store = GSheetStore::with_transport("sid", "tok", Box::new(fake));
        assert_eq!(store.load().unwrap(), Inventory::new());
    }

    #[test]
    fn photos_are_unsupported() {
        let fake = FakeTransport::new(vec![]);
        let store = GSheetStore::with_transport("sid", "tok", Box::new(fake));
        for r in [
            store.get_photo("k").err(),
            store.put_photo("k", b"x").err(),
            store.delete_photo("k").err(),
        ] {
            match r {
                Some(StoreError::Backend(m)) => assert!(m.contains("not supported")),
                other => panic!("expected Backend(not supported), got {other:?}"),
            }
        }
    }

    #[test]
    fn store_is_send_sync() {
        fn require_send_sync<T: Send + Sync>() {}
        require_send_sync::<GSheetStore>();
    }

    // --- LIVE test, gated behind real Google credentials -------------------

    /// End-to-end against a real Google Sheet. Skipped unless both
    /// `GSHEET_TEST_SPREADSHEET_ID` and `GSHEET_TEST_TOKEN` are set; also marked
    /// `#[ignore]` so it never runs in a normal `cargo test`. UNVERIFIED in this
    /// environment (no credentials available).
    #[test]
    #[ignore = "requires real Google Sheets credentials (GSHEET_TEST_SPREADSHEET_ID + GSHEET_TEST_TOKEN)"]
    fn live_roundtrip() {
        let (Ok(sid), Ok(token)) = (
            std::env::var("GSHEET_TEST_SPREADSHEET_ID"),
            std::env::var("GSHEET_TEST_TOKEN"),
        ) else {
            eprintln!("skipping live_roundtrip: credentials not set");
            return;
        };

        let store = GSheetStore::open(&sid, &token).expect("open live store");
        let id = store
            .transact(&mut |inv| {
                inv.add_instance("Item", "live-thing", BTreeMap::new(), None, 1)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            })
            .expect("live transact");
        let loaded = store.load().expect("live load");
        assert_eq!(loaded.get(id).unwrap().name, "live-thing");
    }
}
