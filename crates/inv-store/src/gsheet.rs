//! [`GSheetStore`]: a [`Store`](crate::Store) backed by a Google Sheet using a
//! **native, human-readable** tab/row/column layout (not an opaque JSON blob).
//!
//! ## Native layout
//!
//! The whole [`Inventory`] is spread across multiple tabs so the sheet is
//! directly inspectable and editable by a human in the Google Sheets UI:
//!
//! * **One tab per class**, tab name == class name. The header row is
//!   `[id, name, parent, tags, <one column per class field, in field order>,
//!   created_at, updated_at]`. Each instance is one row; typed field values live
//!   in their own columns.
//! * **`_classes`** — columns `[name, created_at]`.
//! * **`_class_fields`** — columns `[class, field, type, required, ord]`, so field
//!   defs round-trip even for classes that have no instances yet.
//! * **`_relationships`** — columns `[instance_id, kind, target]`.
//! * **`_photos`** — columns `[key, instance_id, mime, name]`. (A cell cannot hold
//!   image bytes, so [`get_photo`](GSheetStore::get_photo) /
//!   [`put_photo`](GSheetStore::put_photo) return [`StoreError::Backend`]; only the
//!   photo *metadata* round-trips through this tab.)
//! * **`_meta`** — key/value rows for `version` and `next_id`.
//!
//! The mapping is a **lossless round-trip**: `grids_to_inventory ∘
//! inventory_to_grids == identity` over any [`Inventory`] (see the unit tests).
//!
//! ## Concurrency: optimistic, version-stamped, bounded retry
//!
//! Google Sheets has no transactions and no compare-and-swap, so
//! [`transact_dyn`](GSheetStore::transact_dyn) uses an optimistic loop keyed on the
//! `version` row in `_meta`:
//!
//! 1. read `_meta` (the `version`) and all tabs -> reconstruct an [`Inventory`];
//! 2. run the caller's closure on `&mut Inventory`;
//! 3. **re-read** `_meta`'s `version`; if it is unchanged, write back every tab
//!    plus `version + 1`; otherwise a concurrent transactor won — reload and retry;
//! 4. exhausting the retry budget yields [`StoreError::Conflict`].
//!
//! ### Residual non-atomic window (documented, honest)
//!
//! Because Sheets offers no atomic CAS, the verify (re-read `version`) and the
//! multi-tab write are separate HTTP calls, and even the multi-tab write itself is
//! not atomic across tabs. Two transactors that both observe the same version and
//! both pass the verify can still race between verify and write — last writer
//! wins, losing the other's update — and a crash mid-write can leave tabs
//! partially updated. This loop is therefore *best-effort* conflict avoidance, not
//! a true serializing lock. (The realistic fix would be a backend with real CAS;
//! Sheets is not one.) The File backend (OS lock) and Postgres backend
//! (`SELECT ... FOR UPDATE`) remain the strongly-serialized options.
//!
//! ## Access modes
//!
//! No secret ever travels on the wire (see [`crate::GSheetMode`]); credentials are
//! resolved **server-side** from the environment:
//!
//! * **`PublicUrl`** — READ-ONLY over a published / link-shared sheet, via the
//!   unauthenticated gviz CSV export endpoint. Needs no credential. Writes return
//!   [`StoreError::Backend`].
//! * **`OAuth`** — read/write via Sheets API v4 with an OAuth access token read
//!   from `INV_GSHEET_TOKEN` (or an OAuth client from `INV_GSHEET_OAUTH_CLIENT`).
//! * **`AppHosted`** — read/write with a service account from
//!   `INV_GSHEET_SERVICE_ACCOUNT`; a missing `spreadsheet_id` means "create a new
//!   spreadsheet for me".
//!
//! ## Testability
//!
//! The grid mapping, CSV parsing, URL/id parsing, and the optimistic-retry
//! decision are all pure / fake-transport-driven so they are unit-tested without a
//! network. The live network path uses [`ReqwestTransport`] and is **UNVERIFIED**
//! in this environment (no Google credentials); its end-to-end test is
//! `#[ignore]` and self-skips when creds are absent.

use std::collections::{BTreeMap, BTreeSet};

use inv_model::{
    Class, FieldDef, FieldType, FieldValue, Instance, Inventory, Photo, Relationship,
};
use serde_json::Value;

use crate::{Store, StoreError};

/// Base URL of the Google Sheets API v4.
const SHEETS_API_BASE: &str = "https://sheets.googleapis.com/v4/spreadsheets";

/// Maximum number of read-verify-write attempts before giving up with
/// [`StoreError::Conflict`].
const MAX_ATTEMPTS: usize = 16;

/// Message returned by photo byte operations, which this backend cannot support.
const PHOTOS_UNSUPPORTED: &str = "photos not supported on the Google Sheet backend";

/// On-wire schema version stamped into `_meta`.
const SCHEMA_VERSION: &str = "1";

// Reserved special tab names. Class tabs must never collide with these.
const TAB_CLASSES: &str = "_classes";
const TAB_CLASS_FIELDS: &str = "_class_fields";
const TAB_RELATIONSHIPS: &str = "_relationships";
const TAB_PHOTOS: &str = "_photos";
const TAB_META: &str = "_meta";

/// A single sheet/tab modeled as a grid of string cells (row-major). The first
/// row is the header.
type Grid = Vec<Vec<String>>;

// ===========================================================================
// Pure value <-> cell encoding
// ===========================================================================

/// The sentinel cell for an explicit [`FieldValue::Empty`] (present-but-unset).
///
/// The model distinguishes three states for a class field on an instance:
/// * the field is **absent** from the instance's map entirely;
/// * the field is present with a typed value;
/// * the field is present but **explicitly `Empty`** (present-but-unset).
///
/// A single cell must round-trip all three losslessly. We therefore map:
/// * absent field  -> a truly blank cell (the common, human-readable case);
/// * explicit `Empty` -> this sentinel;
/// * a typed value -> its textual rendering.
///
/// The sentinel is a glyph that never appears in real data, keeping blank cells
/// meaning "no value here" for a human reader.
const EMPTY_SENTINEL: &str = "\u{2205}"; // ∅

/// Encode an [`Option<FieldValue>`] (where `None` == field absent on the
/// instance) into a single cell.
///
/// Encoding rules (per value):
/// * `None`  (absent)         -> the empty string;
/// * `Empty` (present, unset) -> [`EMPTY_SENTINEL`];
/// * `Text`  -> the string verbatim;
/// * `Number` -> the f64 rendered with `{}` (round-trips exactly for finite f64);
/// * `Bool`   -> `"true"` / `"false"`;
/// * `Date`   -> the i64 unix-millis rendered as decimal.
fn encode_field_cell(v: Option<&FieldValue>) -> String {
    match v {
        None => String::new(),
        Some(FieldValue::Empty) => EMPTY_SENTINEL.to_string(),
        Some(FieldValue::Text(s)) => s.clone(),
        Some(FieldValue::Number(n)) => format!("{n}"),
        Some(FieldValue::Bool(b)) => b.to_string(),
        Some(FieldValue::Date(ms)) => ms.to_string(),
    }
}

/// Decode a single cell back into an [`Option<FieldValue>`] using the column's
/// declared [`FieldType`]. `None` means the field is absent on the instance (a
/// blank cell); [`EMPTY_SENTINEL`] decodes to `Some(Empty)`.
///
/// A `Text` column is total (any non-blank, non-sentinel string is valid). For
/// `Number`/`Bool`/`Date`, a non-blank cell that fails to parse is a hard error so
/// corruption surfaces rather than being silently coerced.
fn decode_field_cell(cell: &str, ty: FieldType) -> Result<Option<FieldValue>, StoreError> {
    if cell.is_empty() {
        return Ok(None);
    }
    if cell == EMPTY_SENTINEL {
        return Ok(Some(FieldValue::Empty));
    }
    let v = match ty {
        FieldType::Text => FieldValue::Text(cell.to_string()),
        FieldType::Number => cell
            .parse::<f64>()
            .map(FieldValue::Number)
            .map_err(|e| StoreError::Backend(format!("gsheet: bad number cell {cell:?}: {e}")))?,
        FieldType::Bool => match cell {
            "true" | "TRUE" | "True" => FieldValue::Bool(true),
            "false" | "FALSE" | "False" => FieldValue::Bool(false),
            other => {
                return Err(StoreError::Backend(format!(
                    "gsheet: bad bool cell {other:?}"
                )))
            }
        },
        FieldType::Date => cell
            .parse::<i64>()
            .map(FieldValue::Date)
            .map_err(|e| StoreError::Backend(format!("gsheet: bad date cell {cell:?}: {e}")))?,
    };
    Ok(Some(v))
}

/// Render a [`FieldType`] as the token stored in `_class_fields.type`.
fn field_type_token(ty: FieldType) -> &'static str {
    match ty {
        FieldType::Text => "text",
        FieldType::Number => "number",
        FieldType::Bool => "bool",
        FieldType::Date => "date",
    }
}

/// Parse a [`FieldType`] token from `_class_fields.type`.
fn parse_field_type(tok: &str) -> Result<FieldType, StoreError> {
    match tok {
        "text" => Ok(FieldType::Text),
        "number" => Ok(FieldType::Number),
        "bool" => Ok(FieldType::Bool),
        "date" => Ok(FieldType::Date),
        other => Err(StoreError::Backend(format!(
            "gsheet: unknown field type {other:?}"
        ))),
    }
}

/// Join an instance's tags into a single cell. Tags never contain commas in this
/// app's usage, but we still pick a separator that is round-trippable for the
/// (sorted, de-duplicated) `BTreeSet`.
fn encode_tags(tags: &BTreeSet<String>) -> String {
    tags.iter().cloned().collect::<Vec<_>>().join(",")
}

/// Split a tags cell back into a set, dropping empties (so `""` -> no tags).
fn decode_tags(cell: &str) -> BTreeSet<String> {
    cell.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Encode an `Option<i64>` parent (the empty cell == `None`).
fn encode_opt_id(id: Option<i64>) -> String {
    id.map(|n| n.to_string()).unwrap_or_default()
}

/// Decode an `Option<i64>` parent cell (`""` -> `None`).
fn decode_opt_id(cell: &str) -> Result<Option<i64>, StoreError> {
    let t = cell.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<i64>()
        .map(Some)
        .map_err(|e| StoreError::Backend(format!("gsheet: bad id cell {cell:?}: {e}")))
}

/// Parse a required i64 id cell.
fn parse_id(cell: &str, ctx: &str) -> Result<i64, StoreError> {
    cell.trim()
        .parse::<i64>()
        .map_err(|e| StoreError::Backend(format!("gsheet: bad {ctx} id {cell:?}: {e}")))
}

// ===========================================================================
// Inventory <-> grids (the native layout). Pure, the core unit-tested seam.
// ===========================================================================

/// The fixed (non-field) leading columns of a class tab.
const CLASS_LEADING: [&str; 4] = ["id", "name", "parent", "tags"];
/// The fixed trailing columns of a class tab.
const CLASS_TRAILING: [&str; 2] = ["created_at", "updated_at"];

/// Build the header row for a class tab: the leading columns, one column per
/// field (in declared order), then the trailing columns.
fn class_header(class: &Class) -> Vec<String> {
    let mut h: Vec<String> = CLASS_LEADING.iter().map(|s| s.to_string()).collect();
    h.extend(class.fields.iter().map(|f| f.name.clone()));
    h.extend(CLASS_TRAILING.iter().map(|s| s.to_string()));
    h
}

/// Serialize the whole [`Inventory`] into the native per-tab grid layout.
///
/// The returned map is `{tab_name -> Grid}` where each `Grid`'s first row is the
/// header. This is the inverse of [`grids_to_inventory`].
fn inventory_to_grids(inv: &Inventory) -> BTreeMap<String, Grid> {
    let mut grids: BTreeMap<String, Grid> = BTreeMap::new();

    // _classes
    let mut classes_grid: Grid = vec![vec!["name".into(), "created_at".into()]];
    for class in inv.classes.values() {
        classes_grid.push(vec![class.name.clone(), class.created_at.to_string()]);
    }
    grids.insert(TAB_CLASSES.to_string(), classes_grid);

    // _class_fields
    let mut cf_grid: Grid = vec![vec![
        "class".into(),
        "field".into(),
        "type".into(),
        "required".into(),
        "ord".into(),
    ]];
    for class in inv.classes.values() {
        for (ord, fd) in class.fields.iter().enumerate() {
            cf_grid.push(vec![
                class.name.clone(),
                fd.name.clone(),
                field_type_token(fd.field_type).to_string(),
                fd.required.to_string(),
                ord.to_string(),
            ]);
        }
    }
    grids.insert(TAB_CLASS_FIELDS.to_string(), cf_grid);

    // One tab per class. Always emit the header (so empty classes still carry
    // their columns), with a row per instance of that class.
    for class in inv.classes.values() {
        let mut grid: Grid = vec![class_header(class)];
        // Deterministic row order: by instance id (BTreeMap iteration is ordered).
        for inst in inv.instances.values().filter(|i| i.class == class.name) {
            let mut row: Vec<String> = Vec::with_capacity(class_header(class).len());
            row.push(inst.id.to_string());
            row.push(inst.name.clone());
            row.push(encode_opt_id(inst.parent));
            row.push(encode_tags(&inst.tags));
            for fd in &class.fields {
                row.push(encode_field_cell(inst.fields.get(&fd.name)));
            }
            row.push(inst.created_at.to_string());
            row.push(inst.updated_at.to_string());
            grid.push(row);
        }
        grids.insert(class.name.clone(), grid);
    }

    // _relationships
    let mut rel_grid: Grid = vec![vec![
        "instance_id".into(),
        "kind".into(),
        "target".into(),
    ]];
    for inst in inv.instances.values() {
        for r in &inst.relationships {
            rel_grid.push(vec![
                inst.id.to_string(),
                r.kind.clone(),
                r.target.to_string(),
            ]);
        }
    }
    grids.insert(TAB_RELATIONSHIPS.to_string(), rel_grid);

    // _photos
    let mut photo_grid: Grid = vec![vec![
        "key".into(),
        "instance_id".into(),
        "mime".into(),
        "name".into(),
    ]];
    for inst in inv.instances.values() {
        for p in &inst.photos {
            photo_grid.push(vec![
                p.key.clone(),
                inst.id.to_string(),
                p.mime.clone(),
                p.name.clone(),
            ]);
        }
    }
    grids.insert(TAB_PHOTOS.to_string(), photo_grid);

    // _meta
    let meta_grid: Grid = vec![
        vec!["key".into(), "value".into()],
        vec!["version".into(), SCHEMA_VERSION.to_string()],
        vec!["next_id".into(), inv.next_id.to_string()],
    ];
    grids.insert(TAB_META.to_string(), meta_grid);

    grids
}

/// Index a header row to `{column name -> column index}`.
fn header_index(grid: &Grid) -> BTreeMap<String, usize> {
    grid.first()
        .map(|h| {
            h.iter()
                .enumerate()
                .map(|(i, name)| (name.clone(), i))
                .collect()
        })
        .unwrap_or_default()
}

/// Fetch a cell by row + column name, returning `""` when the column or cell is
/// absent (Sheets omits trailing empty cells).
fn cell<'a>(row: &'a [String], idx: &BTreeMap<String, usize>, col: &str) -> &'a str {
    idx.get(col)
        .and_then(|&i| row.get(i))
        .map(String::as_str)
        .unwrap_or("")
}

/// Reconstruct an [`Inventory`] from the native per-tab grids. Inverse of
/// [`inventory_to_grids`].
///
/// The class field columns and `_class_fields` are the source of truth for each
/// class's schema; `_relationships`/`_photos` are folded back onto their owning
/// instances; `next_id` comes from `_meta`. Unknown / missing tabs default to
/// empty so a never-written sheet reconstructs [`Inventory::new`].
fn grids_to_inventory(grids: &BTreeMap<String, Grid>) -> Result<Inventory, StoreError> {
    let mut inv = Inventory::new();

    // --- _class_fields: rebuild each class's ordered field list. -------------
    // class -> Vec<(ord, FieldDef)>, sorted by ord afterward.
    let mut class_fields: BTreeMap<String, Vec<(usize, FieldDef)>> = BTreeMap::new();
    if let Some(grid) = grids.get(TAB_CLASS_FIELDS) {
        let idx = header_index(grid);
        for row in grid.iter().skip(1) {
            let class = cell(row, &idx, "class").to_string();
            if class.is_empty() {
                continue;
            }
            let field = cell(row, &idx, "field").to_string();
            let ty = parse_field_type(cell(row, &idx, "type"))?;
            let required = matches!(cell(row, &idx, "required"), "true" | "TRUE" | "True");
            let ord: usize = cell(row, &idx, "ord").parse().unwrap_or(usize::MAX);
            class_fields.entry(class).or_default().push((
                ord,
                FieldDef {
                    name: field,
                    field_type: ty,
                    required,
                },
            ));
        }
    }
    for defs in class_fields.values_mut() {
        defs.sort_by_key(|(ord, _)| *ord);
    }

    // --- _classes: create each class with its (ordered) fields. -------------
    if let Some(grid) = grids.get(TAB_CLASSES) {
        let idx = header_index(grid);
        for row in grid.iter().skip(1) {
            let name = cell(row, &idx, "name").to_string();
            if name.is_empty() {
                continue;
            }
            let created_at = parse_id(cell(row, &idx, "created_at"), "class created_at")?;
            let fields = class_fields
                .get(&name)
                .map(|v| v.iter().map(|(_, fd)| fd.clone()).collect())
                .unwrap_or_default();
            inv.classes.insert(
                name.clone(),
                Class {
                    name,
                    fields,
                    created_at,
                },
            );
        }
    }

    // --- per-class tabs: rebuild instances (minus relationships/photos). ----
    for (class_name, class) in &inv.classes.clone() {
        let Some(grid) = grids.get(class_name) else {
            continue;
        };
        let idx = header_index(grid);
        for row in grid.iter().skip(1) {
            // Skip wholly blank rows that Sheets sometimes leaves behind.
            if row.iter().all(|c| c.trim().is_empty()) {
                continue;
            }
            let id = parse_id(cell(row, &idx, "id"), "instance")?;
            let name = cell(row, &idx, "name").to_string();
            let parent = decode_opt_id(cell(row, &idx, "parent"))?;
            let tags = decode_tags(cell(row, &idx, "tags"));
            let created_at = parse_id(cell(row, &idx, "created_at"), "instance created_at")?;
            let updated_at = parse_id(cell(row, &idx, "updated_at"), "instance updated_at")?;

            let mut fields: BTreeMap<String, FieldValue> = BTreeMap::new();
            for fd in &class.fields {
                let raw = cell(row, &idx, &fd.name);
                // `None` => the field is absent on this instance; only insert
                // when the cell carried a value (or the explicit-Empty sentinel).
                if let Some(val) = decode_field_cell(raw, fd.field_type)? {
                    fields.insert(fd.name.clone(), val);
                }
            }

            inv.instances.insert(
                id,
                Instance {
                    id,
                    class: class_name.clone(),
                    name,
                    fields,
                    tags,
                    parent,
                    photos: Vec::new(),
                    relationships: Vec::new(),
                    created_at,
                    updated_at,
                },
            );
        }
    }

    // --- _relationships: fold edges onto their owning instances. -------------
    if let Some(grid) = grids.get(TAB_RELATIONSHIPS) {
        let idx = header_index(grid);
        for row in grid.iter().skip(1) {
            let raw = cell(row, &idx, "instance_id");
            if raw.trim().is_empty() {
                continue;
            }
            let owner = parse_id(raw, "relationship instance")?;
            let kind = cell(row, &idx, "kind").to_string();
            let target = parse_id(cell(row, &idx, "target"), "relationship target")?;
            if let Some(inst) = inv.instances.get_mut(&owner) {
                inst.relationships.push(Relationship { kind, target });
            }
        }
    }

    // --- _photos: fold metadata onto their owning instances. -----------------
    if let Some(grid) = grids.get(TAB_PHOTOS) {
        let idx = header_index(grid);
        for row in grid.iter().skip(1) {
            let raw = cell(row, &idx, "instance_id");
            if raw.trim().is_empty() {
                continue;
            }
            let owner = parse_id(raw, "photo instance")?;
            let key = cell(row, &idx, "key").to_string();
            let mime = cell(row, &idx, "mime").to_string();
            let name = cell(row, &idx, "name").to_string();
            if let Some(inst) = inv.instances.get_mut(&owner) {
                inst.photos.push(Photo { key, mime, name });
            }
        }
    }

    // --- _meta: next_id (version is read separately for the optimistic loop).
    inv.next_id = read_meta_value(grids, "next_id")?
        .map(|v| {
            v.trim()
                .parse::<i64>()
                .map_err(|e| StoreError::Backend(format!("gsheet: bad next_id {v:?}: {e}")))
        })
        .transpose()?
        .unwrap_or(1);

    Ok(inv)
}

/// Look up a value from the `_meta` key/value tab.
fn read_meta_value(grids: &BTreeMap<String, Grid>, key: &str) -> Result<Option<String>, StoreError> {
    let Some(grid) = grids.get(TAB_META) else {
        return Ok(None);
    };
    let idx = header_index(grid);
    for row in grid.iter().skip(1) {
        if cell(row, &idx, "key") == key {
            return Ok(Some(cell(row, &idx, "value").to_string()));
        }
    }
    Ok(None)
}

/// Read the optimistic `version` from a set of grids (default `0` when absent).
fn version_from_grids(grids: &BTreeMap<String, Grid>) -> u64 {
    read_meta_value(grids, "version")
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

// ===========================================================================
// CSV parsing (for the unauthenticated gviz/CSV export of public sheets).
// ===========================================================================

/// Parse a CSV document (as returned by the gviz `out:csv` endpoint) into a
/// [`Grid`]. Quoted fields, embedded commas/newlines, and `""` escapes are
/// handled. A trailing newline does not produce a spurious empty row.
fn parse_csv(text: &str) -> Grid {
    let mut rows: Grid = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    let mut any_field_on_row = false;

    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        field.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                }
                other => field.push(other),
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => {
                    row.push(std::mem::take(&mut field));
                    any_field_on_row = true;
                }
                '\r' => { /* swallow; handle CRLF via the following \n */ }
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                    any_field_on_row = false;
                }
                other => {
                    field.push(other);
                    any_field_on_row = true;
                }
            }
        }
    }
    // Flush a final unterminated row (no trailing newline).
    if any_field_on_row || !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

// ===========================================================================
// Spreadsheet id / URL parsing
// ===========================================================================

/// Extract the spreadsheet id from a Google Sheets URL.
///
/// Accepts the common shapes:
/// * `https://docs.google.com/spreadsheets/d/<ID>/edit#gid=0`
/// * `https://docs.google.com/spreadsheets/d/<ID>/export?format=csv`
/// * `https://docs.google.com/spreadsheets/d/e/<PUBLISHED_ID>/pub?output=csv`
///
/// Falls back to a `?id=<ID>` / `&id=<ID>` query parameter. Returns
/// [`StoreError::Backend`] when no id can be found.
fn parse_spreadsheet_id(url: &str) -> Result<String, StoreError> {
    // Path form: .../d/<id>/...  (the published form .../d/e/<id> is also valid;
    // we take whatever segment follows /d/, which for /d/e/<id> would be "e" —
    // so special-case the published shape first.)
    if let Some(after) = url.split("/d/e/").nth(1) {
        let id: String = after.chars().take_while(|&c| c != '/' && c != '?').collect();
        if !id.is_empty() {
            return Ok(id);
        }
    }
    if let Some(after) = url.split("/d/").nth(1) {
        let id: String = after.chars().take_while(|&c| c != '/' && c != '?').collect();
        if !id.is_empty() {
            return Ok(id);
        }
    }
    // Query form: ?id=<id> or &id=<id>
    if let Some(after) = url.split("id=").nth(1) {
        let id: String = after.chars().take_while(|&c| c != '&' && c != '#').collect();
        if !id.is_empty() {
            return Ok(id);
        }
    }
    Err(StoreError::Backend(format!(
        "gsheet: could not parse a spreadsheet id from URL {url:?}"
    )))
}

/// Build the unauthenticated gviz CSV-export URL for a single `tab` of
/// `spreadsheet_id`.
fn gviz_csv_url(spreadsheet_id: &str, tab: &str) -> String {
    format!(
        "{}/spreadsheets/d/{}/gviz/tq?tqx=out:csv&sheet={}",
        "https://docs.google.com",
        spreadsheet_id,
        urlencode(tab),
    )
}

/// Build the `spreadsheets.values.get` URL for a `range` (e.g. an A1 tab range).
fn values_get_url(spreadsheet_id: &str, range: &str) -> String {
    format!(
        "{SHEETS_API_BASE}/{}/values/{}",
        urlencode(spreadsheet_id),
        urlencode(range),
    )
}

/// Build the `spreadsheets.values:batchUpdate` URL.
fn values_batch_update_url(spreadsheet_id: &str) -> String {
    format!(
        "{SHEETS_API_BASE}/{}/values:batchUpdate",
        urlencode(spreadsheet_id),
    )
}

/// Build the `spreadsheets.get` URL (used to discover existing tab names).
fn spreadsheet_get_url(spreadsheet_id: &str) -> String {
    format!("{SHEETS_API_BASE}/{}", urlencode(spreadsheet_id))
}

/// Build the `spreadsheets.create` URL.
fn spreadsheet_create_url() -> String {
    SHEETS_API_BASE.to_string()
}

/// Build the `Authorization` header value for an OAuth bearer `token`.
fn auth_header(token: &str) -> String {
    format!("Bearer {token}")
}

/// Percent-encode a URL component, escaping everything outside the RFC 3986
/// unreserved set. Used for spreadsheet ids, A1 ranges (`!`, `:`) and tab names.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ===========================================================================
// Request/response bodies for the Sheets API v4.
// ===========================================================================

/// Build a single `values:batchUpdate` request body that overwrites a set of
/// `{tab -> Grid}` ranges with `RAW` input (no formula/number coercion).
///
/// Each tab is written to the open-ended range `'<tab>'!A1` (the API extends the
/// range to fit the provided values). Empty tabs that previously had data could
/// retain stale trailing rows; callers should clear-then-write when shrinking.
fn batch_update_body(grids: &BTreeMap<String, Grid>) -> Value {
    let data: Vec<Value> = grids
        .iter()
        .map(|(tab, grid)| {
            let values: Vec<Vec<Value>> = grid
                .iter()
                .map(|row| row.iter().map(|c| Value::String(c.clone())).collect())
                .collect();
            serde_json::json!({
                "range": format!("'{}'!A1", tab.replace('\'', "''")),
                "majorDimension": "ROWS",
                "values": values,
            })
        })
        .collect();
    serde_json::json!({
        "valueInputOption": "RAW",
        "data": data,
    })
}

/// Build a `spreadsheets.create` body requesting the full set of tabs (so the new
/// spreadsheet starts with our native structure).
fn create_spreadsheet_body(grids: &BTreeMap<String, Grid>) -> Value {
    let sheets: Vec<Value> = grids
        .keys()
        .map(|tab| serde_json::json!({ "properties": { "title": tab } }))
        .collect();
    serde_json::json!({
        "properties": { "title": "inv-store inventory" },
        "sheets": sheets,
    })
}

/// Parse a `spreadsheets.values.get` response into a [`Grid`] (its `values`).
fn parse_values_get(body: &Value) -> Grid {
    match body.get("values") {
        Some(Value::Array(rows)) => rows
            .iter()
            .map(|r| {
                r.as_array()
                    .map(|cells| {
                        cells
                            .iter()
                            .map(|c| match c {
                                Value::String(s) => s.clone(),
                                Value::Null => String::new(),
                                other => other.to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Parse the list of tab titles from a `spreadsheets.get` response.
fn parse_tab_titles(body: &Value) -> Vec<String> {
    body.get("sheets")
        .and_then(Value::as_array)
        .map(|sheets| {
            sheets
                .iter()
                .filter_map(|s| {
                    s.get("properties")
                        .and_then(|p| p.get("title"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the `spreadsheetId` from a `spreadsheets.create` response.
fn parse_created_id(body: &Value) -> Result<String, StoreError> {
    body.get("spreadsheetId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| StoreError::Backend("gsheet: create response had no spreadsheetId".into()))
}

// ===========================================================================
// Transport seam: the only thing that touches the network.
// ===========================================================================

/// The minimal HTTP surface the store needs. Implemented for real by
/// [`ReqwestTransport`]; implemented by a fake in tests so the grid mapping and
/// optimistic retry loop run without a network.
trait Transport: Send + Sync {
    /// `GET url`, returning the raw response text (used for the public CSV path).
    fn get_text(&self, url: &str) -> Result<String, StoreError>;

    /// `GET url` with an OAuth bearer `token`, returning the parsed JSON body.
    fn get_json(&self, url: &str, token: &str) -> Result<Value, StoreError>;

    /// `POST url` with an OAuth bearer `token` and JSON `body`, returning the
    /// parsed JSON response.
    fn post_json(&self, url: &str, token: &str, body: &Value) -> Result<Value, StoreError>;
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
    fn get_text(&self, url: &str) -> Result<String, StoreError> {
        let resp = self
            .client
            .get(url)
            .send()
            .map_err(|e| StoreError::Backend(format!("gsheet: GET {url}: {e}")))?;
        let status = resp.status();
        let text = resp
            .text()
            .map_err(|e| StoreError::Backend(format!("gsheet: read GET {url}: {e}")))?;
        if !status.is_success() {
            return Err(StoreError::Backend(format!(
                "gsheet: GET {url} -> {status}: {text}"
            )));
        }
        Ok(text)
    }

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

    fn post_json(&self, url: &str, token: &str, body: &Value) -> Result<Value, StoreError> {
        let resp = self
            .client
            .post(url)
            .header(reqwest::header::AUTHORIZATION, auth_header(token))
            .json(body)
            .send()
            .map_err(|e| StoreError::Backend(format!("gsheet: POST {url}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            return Err(StoreError::Backend(format!(
                "gsheet: POST {url} -> {status}: {text}"
            )));
        }
        resp.json::<Value>()
            .map_err(|e| StoreError::Backend(format!("gsheet: decode POST {url}: {e}")))
    }
}

// ===========================================================================
// Credentials and access modes.
// ===========================================================================

/// Which access mode a [`GSheetStore`] was constructed for, with its
/// server-resolved credential and target.
enum Access {
    /// Read-only over a published / link-shared sheet. No credential. The raw
    /// public URL is kept and the spreadsheet id is parsed lazily at read time,
    /// so constructing the store never fails (matching the factory contract that
    /// a public URL always `open`s; an unparseable URL surfaces as a
    /// [`StoreError::Backend`] from the first `load`).
    PublicUrl { url: String },
    /// Read/write a private sheet with a server-side OAuth access `token`.
    OAuth {
        spreadsheet_id: String,
        token: String,
    },
    /// Read/write with the app's service-account identity. `spreadsheet_id` is
    /// `None` until a fresh spreadsheet is created on first write.
    AppHosted {
        /// Interior-mutable so a created id can be remembered across calls.
        spreadsheet_id: std::sync::Mutex<Option<String>>,
        /// The service-account key JSON resolved from the environment. Carried for
        /// the (unverified) live OAuth-token-minting path; the token derivation
        /// itself is the live concern and is not exercised in unit tests.
        #[allow(dead_code)]
        service_account: String,
    },
}

/// A [`Store`](crate::Store) backed by a Google Sheet using the native layout.
pub struct GSheetStore {
    access: Access,
    transport: Box<dyn Transport>,
}

/// Read a credential from the environment, accepting either a path to a file
/// whose contents are the credential, or the inline credential value itself.
/// Returns `None` when the env var is unset/empty.
fn read_env_credential(var: &str) -> Option<String> {
    let raw = std::env::var(var).ok().filter(|v| !v.trim().is_empty())?;
    match std::fs::read_to_string(&raw) {
        Ok(contents) if !contents.trim().is_empty() => Some(contents),
        _ => Some(raw),
    }
}

impl GSheetStore {
    /// Open a READ-ONLY store over a published / link-shared sheet at `url`.
    ///
    /// No credential is required. The spreadsheet id is parsed from the URL and
    /// tabs are read via the unauthenticated gviz CSV export endpoint.
    pub fn public_url(url: &str) -> Result<Self, StoreError> {
        Ok(GSheetStore {
            access: Access::PublicUrl {
                url: url.to_string(),
            },
            transport: Box::new(ReqwestTransport::new()?),
        })
    }

    /// Open a read/write store over the private sheet `spreadsheet_id` using a
    /// **server-side** OAuth credential.
    ///
    /// Resolution order: `INV_GSHEET_TOKEN` (a ready access token), else
    /// `INV_GSHEET_OAUTH_CLIENT` (an OAuth client config to obtain one). If
    /// neither is configured, returns an actionable [`StoreError::Backend`] naming
    /// the env vars.
    pub fn oauth(spreadsheet_id: &str) -> Result<Self, StoreError> {
        let token = read_env_credential("INV_GSHEET_TOKEN")
            .or_else(|| read_env_credential("INV_GSHEET_OAUTH_CLIENT"))
            .ok_or_else(|| {
                StoreError::Backend(
                    "Google OAuth not configured: set INV_GSHEET_OAUTH_CLIENT \
                     (path to or inline OAuth client JSON) or INV_GSHEET_TOKEN \
                     (a server-side OAuth access token)"
                        .to_string(),
                )
            })?;
        Ok(GSheetStore {
            access: Access::OAuth {
                spreadsheet_id: spreadsheet_id.to_string(),
                token,
            },
            transport: Box::new(ReqwestTransport::new()?),
        })
    }

    /// Open a read/write store using the app's own **server-side** service-account
    /// identity. `spreadsheet_id = None` means "create a fresh spreadsheet for me"
    /// on first write.
    ///
    /// The service-account key is read from `INV_GSHEET_SERVICE_ACCOUNT` (a path
    /// to, or the inline JSON of, the key). If it is not configured, returns an
    /// actionable [`StoreError::Backend`].
    pub fn app_hosted(spreadsheet_id: Option<String>) -> Result<Self, StoreError> {
        let service_account = read_env_credential("INV_GSHEET_SERVICE_ACCOUNT").ok_or_else(|| {
            StoreError::Backend(
                "Google service account not configured: set INV_GSHEET_SERVICE_ACCOUNT \
                 (path to or inline service-account key JSON)"
                    .to_string(),
            )
        })?;
        Ok(GSheetStore {
            access: Access::AppHosted {
                spreadsheet_id: std::sync::Mutex::new(spreadsheet_id),
                service_account,
            },
            transport: Box::new(ReqwestTransport::new()?),
        })
    }

    /// Construct over an arbitrary [`Transport`] and [`Access`] (test seam).
    #[cfg(test)]
    fn with_transport(access: Access, transport: Box<dyn Transport>) -> Self {
        GSheetStore { access, transport }
    }

    /// The list of tab names we always read/write (the special tabs plus a tab per
    /// known class). For a never-written sheet only the specials exist; class tabs
    /// are discovered from `_classes`.
    fn read_all_grids(&self) -> Result<BTreeMap<String, Grid>, StoreError> {
        match &self.access {
            Access::PublicUrl { url } => {
                let id = parse_spreadsheet_id(url)?;
                self.read_grids_public(&id)
            }
            Access::OAuth {
                spreadsheet_id,
                token,
            } => self.read_grids_api(spreadsheet_id, token),
            Access::AppHosted { spreadsheet_id, .. } => {
                let id = spreadsheet_id.lock().unwrap().clone();
                match id {
                    Some(id) => {
                        let token = self.app_hosted_token()?;
                        self.read_grids_api(&id, &token)
                    }
                    // No spreadsheet yet: behaves like an empty inventory.
                    None => Ok(inventory_to_grids(&Inventory::new())),
                }
            }
        }
    }

    /// Read all native tabs from a public sheet via the gviz CSV endpoint.
    fn read_grids_public(&self, id: &str) -> Result<BTreeMap<String, Grid>, StoreError> {
        // Discover class tabs from `_classes` first, then read each tab's CSV.
        let mut grids: BTreeMap<String, Grid> = BTreeMap::new();
        for special in [
            TAB_CLASSES,
            TAB_CLASS_FIELDS,
            TAB_RELATIONSHIPS,
            TAB_PHOTOS,
            TAB_META,
        ] {
            let csv = self.transport.get_text(&gviz_csv_url(id, special))?;
            grids.insert(special.to_string(), parse_csv(&csv));
        }
        // Each class named in `_classes` gets its own tab read.
        let class_names: Vec<String> = grids
            .get(TAB_CLASSES)
            .map(|g| {
                let idx = header_index(g);
                g.iter()
                    .skip(1)
                    .map(|r| cell(r, &idx, "name").to_string())
                    .filter(|n| !n.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        for class in class_names {
            let csv = self.transport.get_text(&gviz_csv_url(id, &class))?;
            grids.insert(class, parse_csv(&csv));
        }
        Ok(grids)
    }

    /// Read all native tabs from a private sheet via the authenticated API.
    fn read_grids_api(
        &self,
        id: &str,
        token: &str,
    ) -> Result<BTreeMap<String, Grid>, StoreError> {
        // Discover existing tab titles so we only request ranges that exist
        // (requesting a missing range is a hard 400 from the API).
        let meta = self.transport.get_json(&spreadsheet_get_url(id), token)?;
        let titles = parse_tab_titles(&meta);
        if titles.is_empty() {
            // Treat a blank/never-structured spreadsheet as empty.
            return Ok(inventory_to_grids(&Inventory::new()));
        }
        let mut grids: BTreeMap<String, Grid> = BTreeMap::new();
        for title in titles {
            let url = values_get_url(id, &format!("'{}'!A1:ZZ", title.replace('\'', "''")));
            let body = self.transport.get_json(&url, token)?;
            grids.insert(title, parse_values_get(&body));
        }
        Ok(grids)
    }

    /// Resolve an OAuth access token for the app-hosted (service-account) path.
    ///
    /// Minting a token from a service-account key is the live concern (it requires
    /// a signed JWT exchange with Google). It is **UNVERIFIED** here; without that
    /// machinery we surface an actionable error so the failure is explicit rather
    /// than a confusing 401 at write time.
    fn app_hosted_token(&self) -> Result<String, StoreError> {
        // Allow an explicit token override (used by the live test harness / ops)
        // before falling back to the unimplemented JWT exchange.
        if let Some(tok) = read_env_credential("INV_GSHEET_TOKEN") {
            return Ok(tok);
        }
        Err(StoreError::Backend(
            "gsheet app-hosted token minting from a service-account key is not \
             available in this build; set INV_GSHEET_TOKEN to a server-side OAuth \
             access token for the service account"
                .to_string(),
        ))
    }

    /// Write all grids back to the sheet (read/write modes only), stamping the new
    /// `version`. Returns the spreadsheet id actually written (which may have been
    /// freshly created in app-hosted mode).
    fn write_all_grids(
        &self,
        grids: &BTreeMap<String, Grid>,
        new_version: u64,
    ) -> Result<(), StoreError> {
        let mut grids = grids.clone();
        // Stamp the version into _meta before writing.
        set_meta_version(&mut grids, new_version);

        match &self.access {
            Access::PublicUrl { .. } => Err(StoreError::Backend(
                "public-URL Google Sheets are read-only".to_string(),
            )),
            Access::OAuth {
                spreadsheet_id,
                token,
            } => {
                let url = values_batch_update_url(spreadsheet_id);
                let body = batch_update_body(&grids);
                self.transport.post_json(&url, token, &body).map(|_| ())
            }
            Access::AppHosted {
                spreadsheet_id, ..
            } => {
                let token = self.app_hosted_token()?;
                let existing = spreadsheet_id.lock().unwrap().clone();
                let id = match existing {
                    Some(id) => id,
                    None => {
                        // Create a fresh spreadsheet seeded with our tabs, then
                        // write the data into it.
                        let body = create_spreadsheet_body(&grids);
                        let resp =
                            self.transport
                                .post_json(&spreadsheet_create_url(), &token, &body)?;
                        let id = parse_created_id(&resp)?;
                        *spreadsheet_id.lock().unwrap() = Some(id.clone());
                        id
                    }
                };
                let url = values_batch_update_url(&id);
                let body = batch_update_body(&grids);
                self.transport.post_json(&url, &token, &body).map(|_| ())
            }
        }
    }

    /// Re-read just the optimistic `version` (verify step). Reads the whole sheet
    /// for simplicity, which is acceptable given the optimistic model.
    fn read_version(&self) -> Result<u64, StoreError> {
        let grids = self.read_all_grids()?;
        Ok(version_from_grids(&grids))
    }
}

/// Overwrite the `version` row of the `_meta` grid (inserting `_meta`/the row if
/// missing), so a freshly-built grid set always carries the committed version.
fn set_meta_version(grids: &mut BTreeMap<String, Grid>, version: u64) {
    let meta = grids.entry(TAB_META.to_string()).or_insert_with(|| {
        vec![vec!["key".into(), "value".into()]]
    });
    let idx = header_index(meta);
    let key_col = *idx.get("key").unwrap_or(&0);
    let val_col = *idx.get("value").unwrap_or(&1);
    for row in meta.iter_mut().skip(1) {
        if row.get(key_col).map(String::as_str) == Some("version") {
            while row.len() <= val_col {
                row.push(String::new());
            }
            row[val_col] = version.to_string();
            return;
        }
    }
    meta.push(vec!["version".into(), version.to_string()]);
}

impl Store for GSheetStore {
    fn load(&self) -> Result<Inventory, StoreError> {
        let grids = self.read_all_grids()?;
        grids_to_inventory(&grids)
    }

    fn transact_dyn(
        &self,
        f: &mut dyn FnMut(&mut Inventory) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        // Public sheets are read-only; fail fast with a clear message.
        if matches!(self.access, Access::PublicUrl { .. }) {
            return Err(StoreError::Backend(
                "public-URL Google Sheets are read-only".to_string(),
            ));
        }

        for _ in 0..MAX_ATTEMPTS {
            // 1. Read current grids + the version we are racing against.
            let grids = self.read_all_grids()?;
            let version = version_from_grids(&grids);
            let mut inv = grids_to_inventory(&grids)?;

            // 2. Run the caller's mutation.
            f(&mut inv)?;

            // 3. Optimistic verify: re-read the version; only commit if unchanged.
            let observed = self.read_version()?;
            if observed != version {
                continue;
            }

            // 4. Commit all tabs at version + 1.
            let new_grids = inventory_to_grids(&inv);
            self.write_all_grids(&new_grids, version + 1)?;
            return Ok(());
        }
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

    // -----------------------------------------------------------------------
    // Fixture: a rich inventory exercising every layout feature.
    // -----------------------------------------------------------------------

    fn rich_inventory() -> Inventory {
        let mut inv = Inventory::new();
        // Widget with all four field types + an Empty value.
        let mut f = BTreeMap::new();
        f.insert("color".to_string(), FieldValue::Text("red".to_string()));
        f.insert("qty".to_string(), FieldValue::Number(3.5));
        f.insert("active".to_string(), FieldValue::Bool(true));
        f.insert("when".to_string(), FieldValue::Date(1234));
        f.insert("blank".to_string(), FieldValue::Empty);
        let a = inv.add_instance("Widget", "thing", f, None, 5).unwrap();
        let b = inv
            .add_instance("Widget", "child", BTreeMap::new(), Some(a), 6)
            .unwrap();
        // A second class with an instance, plus an EMPTY class (no instances).
        inv.add_instance("Gadget", "g1", BTreeMap::new(), None, 7)
            .unwrap();
        inv.ensure_class("EmptyClass", 8);
        // tags, relationship, photo metadata.
        inv.add_tag(a, "fresh", 9).unwrap();
        inv.add_tag(a, "boxed", 9).unwrap();
        inv.add_relationship(a, "ref", b, 10).unwrap();
        inv.attach_photo(
            a,
            Photo {
                key: format!("{a}-0"),
                mime: "image/png".to_string(),
                name: "front.png".to_string(),
            },
            11,
        )
        .unwrap();
        inv
    }

    // --- value encoding -----------------------------------------------------

    #[test]
    fn field_value_cell_roundtrip_each_type() {
        // Present, typed values round-trip; explicit Empty round-trips via the
        // sentinel; absent (None) round-trips via a blank cell.
        let cases = [
            (Some(FieldValue::Text("hi".into())), FieldType::Text),
            (Some(FieldValue::Number(3.5)), FieldType::Number),
            (Some(FieldValue::Number(-0.0)), FieldType::Number),
            (Some(FieldValue::Bool(true)), FieldType::Bool),
            (Some(FieldValue::Bool(false)), FieldType::Bool),
            (Some(FieldValue::Date(1_700_000_000_000)), FieldType::Date),
            (Some(FieldValue::Empty), FieldType::Text),
            (Some(FieldValue::Empty), FieldType::Number),
            (None, FieldType::Text),
            (None, FieldType::Number),
        ];
        for (v, ty) in cases {
            let enc = encode_field_cell(v.as_ref());
            let back = decode_field_cell(&enc, ty).unwrap();
            assert_eq!(back, v, "roundtrip {v:?} via {ty:?}");
        }
        // Absent and explicit-Empty encode to distinct cells.
        assert_eq!(encode_field_cell(None), "");
        assert_eq!(encode_field_cell(Some(&FieldValue::Empty)), EMPTY_SENTINEL);
    }

    #[test]
    fn decode_bad_typed_cells_error() {
        assert!(decode_field_cell("notnum", FieldType::Number).is_err());
        assert!(decode_field_cell("maybe", FieldType::Bool).is_err());
        assert!(decode_field_cell("3.5", FieldType::Date).is_err());
    }

    #[test]
    fn tags_join_and_split_roundtrip() {
        let mut tags = BTreeSet::new();
        tags.insert("b".to_string());
        tags.insert("a".to_string());
        let cell = encode_tags(&tags);
        assert_eq!(cell, "a,b", "sorted, comma-joined");
        assert_eq!(decode_tags(&cell), tags);
        assert!(decode_tags("").is_empty(), "empty cell -> no tags");
    }

    // --- full grid roundtrip ------------------------------------------------

    #[test]
    fn inventory_grid_roundtrip_lossless() {
        let inv = rich_inventory();
        let grids = inventory_to_grids(&inv);
        let back = grids_to_inventory(&grids).unwrap();
        assert_eq!(inv, back, "load ∘ commit == identity");
    }

    #[test]
    fn empty_inventory_grid_roundtrip() {
        let inv = Inventory::new();
        let grids = inventory_to_grids(&inv);
        let back = grids_to_inventory(&grids).unwrap();
        assert_eq!(inv, back);
        assert_eq!(back.next_id, 1);
    }

    #[test]
    fn empty_class_columns_roundtrip() {
        // A class with declared fields but no instances must keep its field defs
        // (via _class_fields) and its header columns.
        let mut inv = Inventory::new();
        inv.ensure_class("C", 1);
        // Manually declare fields on the empty class.
        if let Some(c) = inv.classes.get_mut("C") {
            c.fields.push(FieldDef {
                name: "x".into(),
                field_type: FieldType::Number,
                required: true,
            });
            c.fields.push(FieldDef {
                name: "y".into(),
                field_type: FieldType::Text,
                required: false,
            });
        }
        let grids = inventory_to_grids(&inv);
        // Header carries the field columns in order.
        let header = &grids.get("C").unwrap()[0];
        assert_eq!(
            header,
            &vec!["id", "name", "parent", "tags", "x", "y", "created_at", "updated_at"]
        );
        let back = grids_to_inventory(&grids).unwrap();
        assert_eq!(inv, back);
        assert!(back.get_class("C").unwrap().fields[0].required);
    }

    #[test]
    fn class_tab_shape_and_special_tabs_present() {
        let inv = rich_inventory();
        let grids = inventory_to_grids(&inv);
        for special in [
            TAB_CLASSES,
            TAB_CLASS_FIELDS,
            TAB_RELATIONSHIPS,
            TAB_PHOTOS,
            TAB_META,
        ] {
            assert!(grids.contains_key(special), "missing {special}");
        }
        assert!(grids.contains_key("Widget"));
        assert!(grids.contains_key("Gadget"));
        assert!(grids.contains_key("EmptyClass"));

        // Widget header includes its fields in declared order.
        let header = &grids.get("Widget").unwrap()[0];
        assert_eq!(header[0], "id");
        assert_eq!(header[1], "name");
        assert_eq!(header[2], "parent");
        assert_eq!(header[3], "tags");
        assert_eq!(header[header.len() - 2], "created_at");
        assert_eq!(header[header.len() - 1], "updated_at");

        // _relationships has the edge a->b.
        let rels = grids.get(TAB_RELATIONSHIPS).unwrap();
        assert_eq!(rels[0], vec!["instance_id", "kind", "target"]);
        assert!(rels.iter().skip(1).any(|r| r[1] == "ref"));

        // _photos carries metadata.
        let photos = grids.get(TAB_PHOTOS).unwrap();
        assert_eq!(photos[0], vec!["key", "instance_id", "mime", "name"]);
        assert!(photos.iter().skip(1).any(|r| r[2] == "image/png"));

        // _meta carries version + next_id.
        assert_eq!(read_meta_value(&grids, "version").unwrap().as_deref(), Some("1"));
        assert_eq!(
            read_meta_value(&grids, "next_id").unwrap().as_deref(),
            Some(inv.next_id.to_string()).as_deref()
        );
    }

    // --- CSV parsing of a gviz fixture --------------------------------------

    #[test]
    fn parse_gviz_csv_fixture() {
        // gviz returns header + rows, with quoting for embedded commas/quotes.
        let csv = "id,name,parent,tags\n1,\"a, b\",,\"x,y\"\n2,plain,1,\n";
        let grid = parse_csv(csv);
        assert_eq!(grid.len(), 3);
        assert_eq!(grid[0], vec!["id", "name", "parent", "tags"]);
        assert_eq!(grid[1], vec!["1", "a, b", "", "x,y"]);
        assert_eq!(grid[2], vec!["2", "plain", "1", ""]);
    }

    #[test]
    fn parse_csv_quote_escapes_and_crlf() {
        let csv = "a,b\r\n\"she said \"\"hi\"\"\",2\r\n";
        let grid = parse_csv(csv);
        assert_eq!(grid[0], vec!["a", "b"]);
        assert_eq!(grid[1], vec!["she said \"hi\"", "2"]);
    }

    #[test]
    fn public_grids_reconstruct_inventory_via_csv() {
        // Round-trip an inventory through grids -> CSV text -> parse_csv -> grids
        // -> inventory, proving the CSV path reconstructs losslessly.
        let inv = rich_inventory();
        let grids = inventory_to_grids(&inv);
        let mut csv_grids: BTreeMap<String, Grid> = BTreeMap::new();
        for (tab, grid) in &grids {
            let csv = grid_to_csv(grid);
            csv_grids.insert(tab.clone(), parse_csv(&csv));
        }
        let back = grids_to_inventory(&csv_grids).unwrap();
        assert_eq!(inv, back);
    }

    /// Test-only CSV serializer (mirror of `parse_csv`) for the public-path test.
    fn grid_to_csv(grid: &Grid) -> String {
        let mut out = String::new();
        for row in grid {
            let cells: Vec<String> = row
                .iter()
                .map(|c| {
                    if c.contains(',') || c.contains('"') || c.contains('\n') {
                        format!("\"{}\"", c.replace('"', "\"\""))
                    } else {
                        c.clone()
                    }
                })
                .collect();
            out.push_str(&cells.join(","));
            out.push('\n');
        }
        out
    }

    // --- URL / spreadsheet-id parsing --------------------------------------

    #[test]
    fn spreadsheet_id_parsing_variants() {
        assert_eq!(
            parse_spreadsheet_id("https://docs.google.com/spreadsheets/d/ABC123/edit#gid=0")
                .unwrap(),
            "ABC123"
        );
        assert_eq!(
            parse_spreadsheet_id(
                "https://docs.google.com/spreadsheets/d/ABC123/export?format=csv"
            )
            .unwrap(),
            "ABC123"
        );
        assert_eq!(
            parse_spreadsheet_id(
                "https://docs.google.com/spreadsheets/d/e/2PACX-pub/pub?output=csv"
            )
            .unwrap(),
            "2PACX-pub"
        );
        assert_eq!(
            parse_spreadsheet_id("https://example/csv?id=QUERYID&x=1").unwrap(),
            "QUERYID"
        );
        assert!(parse_spreadsheet_id("https://example/nothing").is_err());
    }

    #[test]
    fn url_builders_escape() {
        let get = values_get_url("sheet-id", "Data!A1:A2");
        assert_eq!(
            get,
            "https://sheets.googleapis.com/v4/spreadsheets/sheet-id/values/Data%21A1%3AA2"
        );
        let gviz = gviz_csv_url("sid", "_meta");
        assert_eq!(
            gviz,
            "https://docs.google.com/spreadsheets/d/sid/gviz/tq?tqx=out:csv&sheet=_meta"
        );
        assert_eq!(auth_header("tok"), "Bearer tok");
    }

    // --- request/response body shapes --------------------------------------

    #[test]
    fn batch_update_body_shape() {
        let mut grids: BTreeMap<String, Grid> = BTreeMap::new();
        grids.insert(
            "_meta".into(),
            vec![vec!["key".into(), "value".into()], vec!["version".into(), "2".into()]],
        );
        let body = batch_update_body(&grids);
        assert_eq!(body["valueInputOption"], "RAW");
        assert_eq!(body["data"][0]["range"], "'_meta'!A1");
        assert_eq!(body["data"][0]["values"][0][0], "key");
        assert_eq!(body["data"][0]["values"][1][1], "2");
    }

    #[test]
    fn parse_values_get_into_grid() {
        let body = serde_json::json!({
            "range": "_meta!A1:Z",
            "values": [["key","value"],["version","3"]],
        });
        let grid = parse_values_get(&body);
        assert_eq!(grid, vec![vec!["key", "value"], vec!["version", "3"]]);
        // Missing values -> empty grid.
        assert!(parse_values_get(&serde_json::json!({"range": "x"})).is_empty());
    }

    #[test]
    fn parse_tab_titles_and_created_id() {
        let meta = serde_json::json!({
            "sheets": [
                {"properties": {"title": "_meta"}},
                {"properties": {"title": "Widget"}},
            ]
        });
        assert_eq!(parse_tab_titles(&meta), vec!["_meta", "Widget"]);
        let created = serde_json::json!({"spreadsheetId": "NEWID"});
        assert_eq!(parse_created_id(&created).unwrap(), "NEWID");
        assert!(parse_created_id(&serde_json::json!({})).is_err());
    }

    // --- fake transport for the optimistic loop ----------------------------

    /// A fake spreadsheet held in memory as `{tab -> Grid}`. The fake transport
    /// serves reads from it and applies batchUpdate writes to it, so the whole
    /// optimistic loop can run without a network. A `bump_version_before_verify`
    /// hook simulates a concurrent committer moving the version between the read
    /// and the verify re-read.
    struct FakeSheet {
        grids: Mutex<BTreeMap<String, Grid>>,
        get_calls: Mutex<usize>,
        /// On the Nth `spreadsheets.get` (1-based), bump the stored version right
        /// after serving it, to simulate a racing committer. 0 = never.
        bump_on_get: usize,
        posts: Mutex<usize>,
    }

    impl FakeSheet {
        fn new(grids: BTreeMap<String, Grid>) -> Arc<Self> {
            Arc::new(FakeSheet {
                grids: Mutex::new(grids),
                get_calls: Mutex::new(0),
                bump_on_get: 0,
                posts: Mutex::new(0),
            })
        }
        fn with_bump(grids: BTreeMap<String, Grid>, bump_on_get: usize) -> Arc<Self> {
            Arc::new(FakeSheet {
                grids: Mutex::new(grids),
                get_calls: Mutex::new(0),
                bump_on_get,
                posts: Mutex::new(0),
            })
        }
        fn bump_version(&self) {
            let mut g = self.grids.lock().unwrap();
            let v = version_from_grids(&g);
            set_meta_version(&mut g, v + 1);
        }
    }

    impl Transport for Arc<FakeSheet> {
        fn get_text(&self, _url: &str) -> Result<String, StoreError> {
            Err(StoreError::Backend("fake: get_text unused".into()))
        }

        fn get_json(&self, url: &str, _token: &str) -> Result<Value, StoreError> {
            // spreadsheets.get (no /values/) returns tab titles.
            if !url.contains("/values") {
                let g = self.grids.lock().unwrap();
                let sheets: Vec<Value> = g
                    .keys()
                    .map(|t| serde_json::json!({"properties": {"title": t}}))
                    .collect();
                // Track + optionally race on the metadata read (one per read cycle).
                let mut n = self.get_calls.lock().unwrap();
                *n += 1;
                let hit = *n;
                drop(n);
                let resp = serde_json::json!({ "sheets": sheets });
                if self.bump_on_get != 0 && hit == self.bump_on_get {
                    drop(g);
                    self.bump_version();
                }
                return Ok(resp);
            }
            // values.get for a specific tab: pull the tab name out of the range.
            let tab = url
                .rsplit("/values/")
                .next()
                .and_then(|r| {
                    // range is urlencoded "'<tab>'!A1:ZZ"
                    let decoded = url_decode(r);
                    decoded
                        .trim_start_matches('\'')
                        .split('\'')
                        .next()
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let g = self.grids.lock().unwrap();
            let grid = g.get(&tab).cloned().unwrap_or_default();
            Ok(serde_json::json!({ "values": grid }))
        }

        fn post_json(&self, url: &str, _token: &str, body: &Value) -> Result<Value, StoreError> {
            *self.posts.lock().unwrap() += 1;
            if url.ends_with("values:batchUpdate") {
                // Apply each range's values back into the in-memory grids.
                let mut g = self.grids.lock().unwrap();
                if let Some(data) = body.get("data").and_then(Value::as_array) {
                    for entry in data {
                        let range = entry["range"].as_str().unwrap_or_default();
                        // range is "'<tab>'!A1"
                        let tab = range
                            .trim_start_matches('\'')
                            .split('\'')
                            .next()
                            .unwrap_or_default()
                            .to_string();
                        let grid: Grid = entry["values"]
                            .as_array()
                            .map(|rows| {
                                rows.iter()
                                    .map(|r| {
                                        r.as_array()
                                            .map(|cs| {
                                                cs.iter()
                                                    .map(|c| {
                                                        c.as_str().unwrap_or_default().to_string()
                                                    })
                                                    .collect()
                                            })
                                            .unwrap_or_default()
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        g.insert(tab, grid);
                    }
                }
                return Ok(serde_json::json!({}));
            }
            // spreadsheets.create
            Ok(serde_json::json!({ "spreadsheetId": "CREATED" }))
        }
    }

    /// Minimal percent-decoder for the fake transport's range extraction.
    fn url_decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn oauth_store(fake: Arc<FakeSheet>) -> GSheetStore {
        GSheetStore::with_transport(
            Access::OAuth {
                spreadsheet_id: "sid".into(),
                token: "tok".into(),
            },
            Box::new(fake),
        )
    }

    #[test]
    fn transact_commits_when_version_unchanged() {
        let fake = FakeSheet::new(inventory_to_grids(&Inventory::new()));
        let store = oauth_store(fake.clone());

        let id = store
            .transact(&mut |inv| {
                inv.add_instance("Item", "thing", BTreeMap::new(), None, 1)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            })
            .expect("transact should commit");
        assert_eq!(id, 1);

        // The committed sheet now reconstructs the new instance, version bumped.
        let g = fake.grids.lock().unwrap().clone();
        let back = grids_to_inventory(&g).unwrap();
        assert_eq!(back.get(1).unwrap().name, "thing");
        assert_eq!(version_from_grids(&g), 2, "version 1 -> 2 on commit");
        assert_eq!(*fake.posts.lock().unwrap(), 1, "exactly one write");
    }

    #[test]
    fn transact_retries_then_commits_when_version_moves() {
        // Bump the version during the FIRST read cycle's metadata GET so the verify
        // disagrees and forces one retry; the second cycle is stable and commits.
        let fake = FakeSheet::with_bump(inventory_to_grids(&Inventory::new()), 1);
        let store = oauth_store(fake.clone());

        store
            .transact(&mut |inv| {
                inv.add_instance("Item", "thing", BTreeMap::new(), None, 1)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            })
            .expect("transact should eventually commit");

        // Exactly one successful write landed (the retry attempt did not write).
        assert_eq!(*fake.posts.lock().unwrap(), 1);
        let g = fake.grids.lock().unwrap().clone();
        assert_eq!(grids_to_inventory(&g).unwrap().get(1).unwrap().name, "thing");
    }

    #[test]
    fn transact_exhausts_retries_into_conflict() {
        // A transport whose verify version ALWAYS differs from the read version, so
        // no attempt ever commits and the loop exhausts into Conflict.
        struct AlwaysMoving {
            tick: Mutex<u64>,
        }
        impl Transport for Arc<AlwaysMoving> {
            fn get_text(&self, _url: &str) -> Result<String, StoreError> {
                unreachable!()
            }
            fn get_json(&self, url: &str, _t: &str) -> Result<Value, StoreError> {
                if !url.contains("/values") {
                    // metadata read: only _meta tab exists.
                    return Ok(serde_json::json!({"sheets":[{"properties":{"title":"_meta"}}]}));
                }
                // Every _meta read returns a monotonically increasing version, so
                // the verify never matches the read.
                let mut t = self.tick.lock().unwrap();
                *t += 1;
                let v = *t;
                Ok(serde_json::json!({
                    "values": [["key","value"],["version", v.to_string()],["next_id","1"]]
                }))
            }
            fn post_json(&self, _u: &str, _t: &str, _b: &Value) -> Result<Value, StoreError> {
                panic!("must never commit under sustained contention");
            }
        }
        let t = Arc::new(AlwaysMoving { tick: Mutex::new(0) });
        let store = GSheetStore::with_transport(
            Access::OAuth {
                spreadsheet_id: "sid".into(),
                token: "tok".into(),
            },
            Box::new(t),
        );
        let res = store.transact(&mut |inv| {
            inv.add_instance("Item", "thing", BTreeMap::new(), None, 1)
                .map_err(|e| StoreError::Backend(e.to_string()))
        });
        assert_eq!(res, Err(StoreError::Conflict));
    }

    #[test]
    fn load_reconstructs_via_fake_transport() {
        let inv = rich_inventory();
        let fake = FakeSheet::new(inventory_to_grids(&inv));
        let store = oauth_store(fake);
        let loaded = store.load().expect("load");
        assert_eq!(loaded, inv);
    }

    // --- access-mode / credential branch behavior --------------------------

    #[test]
    fn public_url_is_read_only() {
        // Constructs from a URL with no credential; writes are rejected.
        let store = GSheetStore::with_transport(
            Access::PublicUrl {
                url: "https://docs.google.com/spreadsheets/d/sid/edit".into(),
            },
            // transport unused on the write rejection path
            Box::new(FakeSheet::new(BTreeMap::new())),
        );
        let res = store.transact(&mut |inv| {
            inv.add_instance("Item", "x", BTreeMap::new(), None, 1)
                .map_err(|e| StoreError::Backend(e.to_string()))
        });
        match res {
            Err(StoreError::Backend(m)) => assert!(m.contains("read-only"), "{m}"),
            other => panic!("expected read-only Backend error, got {other:?}"),
        }
    }

    #[test]
    fn public_url_constructor_parses_id() {
        let store =
            GSheetStore::public_url("https://docs.google.com/spreadsheets/d/PUB/export?format=csv")
                .expect("public_url");
        match store.access {
            Access::PublicUrl { url } => assert_eq!(parse_spreadsheet_id(&url).unwrap(), "PUB"),
            _ => panic!("expected PublicUrl access"),
        }
    }

    #[test]
    fn public_url_unparseable_errors_at_read_time() {
        // The constructor defers id parsing (so `open` always succeeds for a
        // public URL); an unparseable URL surfaces as a gsheet Backend error on
        // the first read. The error message names the failure.
        assert!(parse_spreadsheet_id("https://example/no-id-here").is_err());
        let store = GSheetStore::with_transport(
            Access::PublicUrl {
                url: "https://example/no-id-here".into(),
            },
            Box::new(FakeSheet::new(BTreeMap::new())),
        );
        match store.load() {
            Err(StoreError::Backend(m)) => {
                assert!(m.contains("gsheet") && m.contains("spreadsheet id"), "{m}")
            }
            other => panic!("expected Backend(parse) error, got {other:?}"),
        }
    }

    #[test]
    fn oauth_without_credential_errors_actionably() {
        if std::env::var_os("INV_GSHEET_TOKEN").is_some()
            || std::env::var_os("INV_GSHEET_OAUTH_CLIENT").is_some()
        {
            return;
        }
        match GSheetStore::oauth("sid") {
            Err(StoreError::Backend(m)) => {
                assert!(
                    m.contains("INV_GSHEET_OAUTH_CLIENT") || m.contains("INV_GSHEET_TOKEN"),
                    "{m}"
                );
            }
            Err(other) => panic!("expected Backend(... INV_GSHEET ...), got {other:?}"),
            Ok(_) => panic!("expected an error when no OAuth credential is configured"),
        }
    }

    #[test]
    fn app_hosted_without_credential_errors_actionably() {
        if std::env::var_os("INV_GSHEET_SERVICE_ACCOUNT").is_some() {
            return;
        }
        match GSheetStore::app_hosted(None) {
            Err(StoreError::Backend(m)) => assert!(m.contains("INV_GSHEET_SERVICE_ACCOUNT"), "{m}"),
            Err(other) => panic!("expected Backend(... INV_GSHEET_SERVICE_ACCOUNT ...), got {other:?}"),
            Ok(_) => panic!("expected an error when no service account is configured"),
        }
    }

    #[test]
    fn app_hosted_creates_then_reuses_spreadsheet() {
        // With no spreadsheet id, the first write creates one (the fake returns
        // "CREATED"), and the id is remembered for subsequent ops.
        let fake = FakeSheet::new(BTreeMap::new());
        let store = GSheetStore::with_transport(
            Access::AppHosted {
                spreadsheet_id: std::sync::Mutex::new(None),
                service_account: "sa".into(),
            },
            Box::new(fake.clone()),
        );
        // Provide a token so the app-hosted write path proceeds in this test.
        // (Token minting is the live concern; here we use the override env var.)
        // Use a guard so we don't pollute other tests' environment expectations.
        let _guard = EnvGuard::set("INV_GSHEET_TOKEN", "test-token");

        let id = store
            .transact(&mut |inv| {
                inv.add_instance("Item", "thing", BTreeMap::new(), None, 1)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            })
            .expect("transact creates sheet then commits");
        assert_eq!(id, 1);

        // The created id was remembered.
        if let Access::AppHosted { spreadsheet_id, .. } = &store.access {
            assert_eq!(spreadsheet_id.lock().unwrap().as_deref(), Some("CREATED"));
        } else {
            panic!("expected AppHosted access");
        }
    }

    #[test]
    fn photos_are_unsupported() {
        let store = oauth_store(FakeSheet::new(BTreeMap::new()));
        for r in [
            store.get_photo("k").err(),
            store.put_photo("k", b"x").err(),
            store.delete_photo("k").err(),
        ] {
            match r {
                Some(StoreError::Backend(m)) => assert!(m.contains("not supported"), "{m}"),
                other => panic!("expected Backend(not supported), got {other:?}"),
            }
        }
    }

    #[test]
    fn store_is_send_sync() {
        fn require_send_sync<T: Send + Sync>() {}
        require_send_sync::<GSheetStore>();
    }

    /// Scoped env-var setter that restores the previous value on drop, so tests
    /// that need an env credential don't leak into the credential-absence tests.
    struct EnvGuard {
        key: String,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &str, val: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, val);
            EnvGuard {
                key: key.to_string(),
                prev,
            }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }

    // --- LIVE test, gated behind real Google credentials -------------------

    /// End-to-end against a real Google Sheet over the OAuth path. Skipped unless
    /// both `GSHEET_TEST_SPREADSHEET_ID` and `INV_GSHEET_TOKEN` are set; also
    /// `#[ignore]` so it never runs in a normal `cargo test`. UNVERIFIED in this
    /// environment (no credentials available).
    #[test]
    #[ignore = "requires real Google Sheets credentials (GSHEET_TEST_SPREADSHEET_ID + INV_GSHEET_TOKEN)"]
    fn live_roundtrip() {
        let Ok(sid) = std::env::var("GSHEET_TEST_SPREADSHEET_ID") else {
            eprintln!("skipping live_roundtrip: GSHEET_TEST_SPREADSHEET_ID not set");
            return;
        };
        if std::env::var_os("INV_GSHEET_TOKEN").is_none() {
            eprintln!("skipping live_roundtrip: INV_GSHEET_TOKEN not set");
            return;
        }
        let store = GSheetStore::oauth(&sid).expect("open live store");
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
