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
//! * **`PublicUrl`** — READ-**WRITE** over a link-shared ("anyone with link can
//!   edit") sheet with NO credential. Reads use the unauthenticated gviz CSV
//!   export; writes replicate the Sheets web-editor's anonymous `/edit` + `/save`
//!   protocol (cookies only). The inventory is spread across **three separate
//!   tabs** — `Meta`, `Classes`, `Instances` (see the "Multi-tab layout
//!   (anonymous mode)" section) — so the classes table and instances table live in
//!   their own tabs, not one flat `Sheet1`. The `/save` writer creates any missing
//!   tab (add-sheet command) and writes header + data rows; structured columns use
//!   the model's own serde JSON, round-tripping losslessly through gviz CSV.
//! * **`OAuth`** — read/write via Sheets API v4 with an OAuth access token read
//!   from `INV_GSHEET_TOKEN` (or an OAuth client from `INV_GSHEET_OAUTH_CLIENT`).
//!   Uses the multi-tab native layout above.
//! * **`AppHosted`** — read/write with a service account from
//!   `INV_GSHEET_SERVICE_ACCOUNT`; a missing `spreadsheet_id` means "create a new
//!   spreadsheet for me". Uses the multi-tab native layout above.
//!
//! ## Testability
//!
//! The grid mappings (multi-tab AND flat), CSV parsing, URL/id parsing, the
//! anonymous command/bundle encoding, the revision/sid HTML parsing, the save
//! response parsing, and the optimistic-retry decision are all pure /
//! fake-transport-driven so they are unit-tested without a network. The OAuth /
//! AppHosted live path uses [`UreqTransport`] and is **UNVERIFIED** (no Google
//! credentials). The anonymous PublicUrl live path is **VERIFIED** against a real
//! link-shared sheet by an `#[ignore]`d integration test (see
//! `tests/gsheet_anon_live.rs`); the probe in `examples/gsheet_probe.rs` proved it
//! end to end.

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
// Anonymous (no-OAuth) read-WRITE protocol for link-shared sheets.
//
// This replicates the Sheets web-editor's private `save` protocol, captured from
// a live anonymous browser session. Unlike the Sheets API v4 (which needs an
// OAuth token), it writes to a sheet shared "anyone with link can edit" using
// only the cookies the editor itself hands an anonymous visitor.
//
// PROVEN protocol (re-captured live; simpler than the first reverse-engineering
// notes suggested):
//
//   1. GET  /spreadsheets/d/<ID>/edit  -> Set-Cookie (COMPASS, NID) into the
//      agent's cookie jar; the HTML carries the current revision as
//      `"revision":<N>` and the **server-assigned** session id as `"sid":"<hex>"`.
//   2. POST /spreadsheets/d/<ID>/save  -> a `multipart/form-data` body of
//      {rev=<N>, bundles=<JSON>}; on success HTTP 200 and the revision advances.
//
// Two corrections vs. the original capture notes, both verified live:
//   * the `sid` is NOT client-chosen — only the sid the server put in the `/edit`
//     HTML is accepted (a random sid -> HTTP 400/550);
//   * the separate `/bind` handshake is NOT needed for a batch write (it returns
//     400 for a fresh page session, yet `/save` still succeeds).
// ===========================================================================

/// Outer command tag wrapping one cell mutation in a `bundles` command list.
///
/// MAGIC, build-version-tied: captured from build
/// `editors.spreadsheets-frontend_20260601`. If Google ships a new editor build
/// this opcode may change and must be re-captured from a live browser `/save`.
const OP_BUNDLE: i64 = 21299578;

/// Inner "set one cell" mutation tag inside a command.
///
/// MAGIC, build-version-tied (see [`OP_BUNDLE`]); re-capture on editor updates.
const OP_SET_CELL: i64 = 132274236;

/// Outer "add sheet" compound command tag. Wraps two inner commands: the
/// add-sheet body ([`OP_ADD_SHEET_BODY`]) and the index positioner
/// ([`OP_ADD_SHEET_INDEX`]).
///
/// MAGIC, build-version-tied (see [`OP_BUNDLE`]); re-capture on editor updates.
/// Verified live (probe: revision advanced, gviz showed the new tab).
const OP_ADD_SHEET: i64 = 4444216;

/// Inner add-sheet body tag: creates a tab with a client-chosen gid + name and
/// default 1000x26 dimensions.
///
/// MAGIC, build-version-tied (see [`OP_BUNDLE`]); re-capture on editor updates.
const OP_ADD_SHEET_BODY: i64 = 21350203;

/// Inner add-sheet index tag: positions the new tab at a given 0-based index.
///
/// MAGIC, build-version-tied (see [`OP_BUNDLE`]); re-capture on editor updates.
const OP_ADD_SHEET_INDEX: i64 = 28950036;

// The anonymous PublicUrl layout no longer stores data on the default first tab
// (`Sheet1`, gid `"0"`), but we never delete it: Sheets requires at least one tab
// and the captured opcode surface has no delete-sheet command.
//
// --- The three fixed tabs of the anonymous (PublicUrl) multi-tab layout. -----
//
// Each tab has a FIXED, client-chosen gid constant so the add-sheet command and
// the subsequent set-cell commands agree on the grid id within one /save bundle.
// The constants are large and arbitrary to avoid colliding with auto-assigned
// gids (Sheet1 is gid 0; the editor assigns large random gids to user tabs).

/// Tab name + gid for the key/value `Meta` tab (next_id, version).
const ANON_TAB_META: &str = "Meta";
const ANON_GID_META: &str = "990001";

/// Tab name + gid for the `Classes` tab (one row per class).
const ANON_TAB_CLASSES: &str = "Classes";
const ANON_GID_CLASSES: &str = "990002";

/// Tab name + gid for the `Instances` tab (one row per instance).
const ANON_TAB_INSTANCES: &str = "Instances";
const ANON_GID_INSTANCES: &str = "990003";

/// The three (name, gid) tab definitions, in the index order they are created.
const ANON_TABS: [(&str, &str); 3] = [
    (ANON_TAB_META, ANON_GID_META),
    (ANON_TAB_CLASSES, ANON_GID_CLASSES),
    (ANON_TAB_INSTANCES, ANON_GID_INSTANCES),
];

/// Parse the current revision from the `/edit` HTML (`"revision":<N>`).
fn parse_html_revision(html: &str) -> Option<i64> {
    let needle = "\"revision\":";
    let i = html.find(needle)? + needle.len();
    let digits: String = html[i..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Parse the set of existing tab names from the `/edit` HTML.
///
/// The editor renders each tab caption as
/// `<div class="...docs-sheet-tab-caption">NAME</div>`. We scan for that marker
/// and collect the text up to the next `<`. This is the source of truth for "does
/// tab X already exist" in the anonymous multi-tab write path: gviz cannot answer
/// it (a missing tab silently falls back to the first sheet rather than erroring
/// on this kind of link-shared sheet), but the rendered caption list always names
/// every tab.
///
/// Brittleness: this parses presentation HTML, so an editor markup change to the
/// caption class would break detection. The failure mode is safe-ish — a tab seen
/// as "missing" would get a duplicate add-sheet (which the server rejects, failing
/// the save loudly) rather than silent corruption.
fn parse_html_tab_names(html: &str) -> BTreeSet<String> {
    let marker = "docs-sheet-tab-caption\">";
    let mut names = BTreeSet::new();
    let mut rest = html;
    while let Some(i) = rest.find(marker) {
        let after = &rest[i + marker.len()..];
        let name: String = after.chars().take_while(|&c| c != '<').collect();
        if !name.is_empty() {
            names.insert(decode_html_entities(&name));
        }
        rest = after;
    }
    names
}

/// Decode the handful of HTML entities the editor uses in tab captions, so a tab
/// named e.g. `A&B` matches the model's plain string.
fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// Parse the server-assigned session id from the `/edit` HTML (`"sid":"<hex>"`).
/// This is the only sid `/save` accepts.
fn parse_html_sid(html: &str) -> Option<String> {
    let needle = "\"sid\":\"";
    let i = html.find(needle)? + needle.len();
    let val: String = html[i..].chars().take_while(|&c| c != '"').collect();
    if val.is_empty() {
        None
    } else {
        Some(val)
    }
}

/// Build the INNER (double-encoded) set-cell command string for a 0-based
/// `(row, col)` on grid `gid`, setting it to the string `v`.
///
/// The shape is the captured-verbatim command for "set one cell to a string":
/// `[[<gid>,row,row+1,col,col+1],[OP_SET_CELL,3,[2,"<v>"],null,null,0],
///   [null,[[null,513,[0],null,...,0]]]]`.
/// The `[2,"<v>"]` is the type-tagged value (tag 2 == string); ALL values are
/// written as strings, so numbers/bools/dates are stringified by the layout.
fn anon_inner_set_cell(gid: &str, row: i64, col: i64, v: &str) -> String {
    let n = Value::Null;
    serde_json::json!([
        [gid, row, row + 1, col, col + 1],
        [OP_SET_CELL, 3, [2, v], n, n, 0],
        [n, [[n, 513, [0], n, n, n, n, n, n, n, n, 0]]]
    ])
    .to_string()
}

/// Build the INNER add-sheet body command string for a new tab with grid id
/// `gid` and `name`, sized to the default 1000 rows x 26 columns.
///
/// Captured-verbatim shape (build-tied):
/// `[1,0,"<gid>",[[[0,0,"<name>"],[2,0,null,null,0],[3,0,null,null,null,0],
///   [4,0,null,null,null,null,0],[5,0,null,null,null,null,null,0],
///   [6,0,null,null,null,null,null,null,0]]],1000,26]`.
fn anon_inner_add_sheet(gid: &str, name: &str) -> String {
    let n = Value::Null;
    serde_json::json!([
        1, 0, gid,
        [[
            [0, 0, name],
            [2, 0, n, n, 0],
            [3, 0, n, n, n, 0],
            [4, 0, n, n, n, n, 0],
            [5, 0, n, n, n, n, n, 0],
            [6, 0, n, n, n, n, n, n, 0]
        ]],
        1000, 26
    ])
    .to_string()
}

/// Build the INNER add-sheet index command string positioning a new tab at the
/// 0-based `index`. Captured-verbatim shape: `[[[[4,0,null,null,<index>]]]]`.
fn anon_inner_add_index(index: i64) -> String {
    let n = Value::Null;
    serde_json::json!([[[[4, 0, n, n, index]]]]).to_string()
}

/// A single mutation to include in a `/save` bundle.
enum AnonCmd {
    /// Create a tab `name` with grid id `gid`, positioned at 0-based `index`.
    AddSheet { gid: String, name: String, index: i64 },
    /// Set cell `(row, col)` of grid `gid` to the string `value`.
    SetCell { gid: String, row: i64, col: i64, value: String },
}

/// Encode one [`AnonCmd`] into its outer `bundles` command value.
fn anon_command_value(cmd: &AnonCmd) -> Value {
    match cmd {
        AnonCmd::AddSheet { gid, name, index } => serde_json::json!([
            OP_ADD_SHEET,
            [
                [OP_ADD_SHEET_BODY, anon_inner_add_sheet(gid, name)],
                [OP_ADD_SHEET_INDEX, anon_inner_add_index(*index)]
            ]
        ]),
        AnonCmd::SetCell {
            gid,
            row,
            col,
            value,
        } => serde_json::json!([OP_BUNDLE, anon_inner_set_cell(gid, *row, *col, value)]),
    }
}

/// Build the `bundles` POST field from a heterogeneous command list (add-sheet
/// and/or set-cell commands), all in one bundle.
/// `[{"commands":[<cmd>, ...],"sid":"<sid>","reqId":<id>}]`.
fn anon_build_bundles(sid: &str, req_id: i64, cmds: &[AnonCmd]) -> String {
    let commands: Vec<Value> = cmds.iter().map(anon_command_value).collect();
    serde_json::json!([{ "commands": commands, "sid": sid, "reqId": req_id }]).to_string()
}

/// Encode a `multipart/form-data` body from string fields.
/// Returns `(content_type_header_value, body_bytes)`.
fn anon_multipart(fields: &[(&str, &str)]) -> (String, Vec<u8>) {
    // A boundary token unlikely to appear in JSON field values.
    let boundary = "----invstoreGSheetAnon7e1081f93830126";
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// Build the anonymous `/edit` URL for a spreadsheet id.
fn anon_edit_url(id: &str) -> String {
    format!("https://docs.google.com/spreadsheets/d/{id}/edit")
}

/// Build the anonymous `/save` URL for a spreadsheet id and (server) sid.
fn anon_save_url(id: &str, sid: &str) -> String {
    format!(
        "https://docs.google.com/spreadsheets/d/{id}/save?\
         id={id}&sid={sid}&vc=1&c=1&w=1&flr=0&smv=2147483647&smb=%5B2147483647%2C%20APxr%5D\
         &includes_info_params=true&cros_files=false&nded=false"
    )
}

/// A `/save` response is `)]}'` then JSON; the revision advanced when the JSON
/// carries a `revisionRanges`. Returns the highest committed revision, or an
/// error if the body looks like a channel error (`["er",...]`).
fn parse_save_response(text: &str) -> Result<i64, StoreError> {
    let json = text.trim_start_matches(")]}'").trim();
    if json.starts_with("[[\"er\"") || json.contains("[\"er\",") {
        return Err(StoreError::Backend(format!(
            "gsheet anonymous save rejected by server: {json}"
        )));
    }
    let v: Value = serde_json::from_str(json)
        .map_err(|e| StoreError::Backend(format!("gsheet: bad save response {json:?}: {e}")))?;
    // revisionRanges: [[lo,hi], ...]; take the maximum hi.
    let hi = v
        .get("revisionRanges")
        .and_then(Value::as_array)
        .and_then(|ranges| {
            ranges
                .iter()
                .filter_map(|r| r.as_array().and_then(|p| p.get(1)).and_then(Value::as_i64))
                .max()
        });
    hi.ok_or_else(|| {
        StoreError::Backend(format!(
            "gsheet: save response carried no revisionRanges: {json}"
        ))
    })
}

// ---------------------------------------------------------------------------
// Multi-tab layout (anonymous mode).
//
// The Inventory is split across THREE separate tabs (each its own grid the user
// can read in the Sheets UI), instead of one flat `Sheet1`:
//
//   * "Meta"      — header [key, value]; rows: next_id, version.
//   * "Classes"   — header [name, created_at, fields_json]; one row per class.
//   * "Instances" — header [id, class, name, parent, tags_json, fields_json,
//                   relationships_json, photos_json, created_at, updated_at];
//                   one row per instance.
//
// Each tab has a header row so a human reads native columns. Structured parts
// (field defs / field values / tags / relationships / photos) are stored as the
// model's own serde JSON in a single cell, so they round-trip losslessly and
// never collide with the CSV delimiter or get number-coerced by gviz. The gviz
// CSV read and the cell-by-cell `/save` write agree on the same per-tab shapes.
// ---------------------------------------------------------------------------

/// Header row of the `Meta` tab.
fn anon_meta_header() -> Vec<String> {
    vec!["key".into(), "value".into()]
}

/// Header row of the `Classes` tab.
fn anon_classes_header() -> Vec<String> {
    vec!["name".into(), "created_at".into(), "fields_json".into()]
}

/// Header row of the `Instances` tab.
fn anon_instances_header() -> Vec<String> {
    vec![
        "id".into(),
        "class".into(),
        "name".into(),
        "parent".into(),
        "tags_json".into(),
        "fields_json".into(),
        "relationships_json".into(),
        "photos_json".into(),
        "created_at".into(),
        "updated_at".into(),
    ]
}

/// Serialize the [`Inventory`] into the three anonymous tabs `{name -> Grid}`,
/// each with a header row. Inverse of [`anon_grids_to_inventory`].
fn inventory_to_anon_grids(inv: &Inventory) -> BTreeMap<String, Grid> {
    let mut grids: BTreeMap<String, Grid> = BTreeMap::new();

    // Meta.
    let meta: Grid = vec![
        anon_meta_header(),
        vec!["next_id".into(), inv.next_id.to_string()],
        vec!["version".into(), SCHEMA_VERSION.to_string()],
    ];
    grids.insert(ANON_TAB_META.to_string(), meta);

    // Classes (ordered by name via BTreeMap).
    let mut classes: Grid = vec![anon_classes_header()];
    for class in inv.classes.values() {
        let fields_json = serde_json::to_string(&class.fields).unwrap_or_else(|_| "[]".into());
        classes.push(vec![
            class.name.clone(),
            class.created_at.to_string(),
            fields_json,
        ]);
    }
    grids.insert(ANON_TAB_CLASSES.to_string(), classes);

    // Instances (ordered by id via BTreeMap).
    let mut instances: Grid = vec![anon_instances_header()];
    for inst in inv.instances.values() {
        let tags_json = serde_json::to_string(&inst.tags).unwrap_or_else(|_| "[]".into());
        let fields_json = serde_json::to_string(&inst.fields).unwrap_or_else(|_| "{}".into());
        let rels_json = serde_json::to_string(&inst.relationships).unwrap_or_else(|_| "[]".into());
        let photos_json = serde_json::to_string(&inst.photos).unwrap_or_else(|_| "[]".into());
        instances.push(vec![
            inst.id.to_string(),
            inst.class.clone(),
            inst.name.clone(),
            encode_opt_id(inst.parent),
            tags_json,
            fields_json,
            rels_json,
            photos_json,
            inst.created_at.to_string(),
            inst.updated_at.to_string(),
        ]);
    }
    grids.insert(ANON_TAB_INSTANCES.to_string(), instances);

    grids
}

/// Reconstruct an [`Inventory`] from the three anonymous tab grids. Inverse of
/// [`inventory_to_anon_grids`]. Missing/blank tabs default to empty so a
/// never-written sheet reconstructs [`Inventory::new`].
fn anon_grids_to_inventory(grids: &BTreeMap<String, Grid>) -> Result<Inventory, StoreError> {
    let mut inv = Inventory::new();

    // Meta: next_id (version is read separately for the optimistic loop).
    if let Some(grid) = grids.get(ANON_TAB_META) {
        let idx = header_index(grid);
        for row in grid.iter().skip(1) {
            if cell(row, &idx, "key") == "next_id" {
                let next = cell(row, &idx, "value").trim();
                if !next.is_empty() {
                    inv.next_id = next.parse::<i64>().map_err(|e| {
                        StoreError::Backend(format!("gsheet: bad next_id {next:?}: {e}"))
                    })?;
                }
            }
        }
    }

    // Classes.
    if let Some(grid) = grids.get(ANON_TAB_CLASSES) {
        let idx = header_index(grid);
        for row in grid.iter().skip(1) {
            let name = cell(row, &idx, "name").to_string();
            if name.is_empty() {
                continue;
            }
            let created_at = parse_id(cell(row, &idx, "created_at"), "class created_at")?;
            let fields: Vec<FieldDef> = parse_json_cell(cell(row, &idx, "fields_json"), "class fields")?;
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

    // Instances.
    if let Some(grid) = grids.get(ANON_TAB_INSTANCES) {
        let idx = header_index(grid);
        for row in grid.iter().skip(1) {
            // Skip wholly blank rows that gviz/Sheets sometimes leaves behind.
            if row.iter().all(|c| c.trim().is_empty()) {
                continue;
            }
            let id_cell = cell(row, &idx, "id");
            if id_cell.trim().is_empty() {
                continue;
            }
            let id = parse_id(id_cell, "instance")?;
            let class = cell(row, &idx, "class").to_string();
            let name = cell(row, &idx, "name").to_string();
            let parent = decode_opt_id(cell(row, &idx, "parent"))?;
            let tags: BTreeSet<String> =
                parse_json_cell(cell(row, &idx, "tags_json"), "instance tags")?;
            let fields: BTreeMap<String, FieldValue> =
                parse_json_cell(cell(row, &idx, "fields_json"), "instance fields")?;
            let relationships: Vec<Relationship> =
                parse_json_cell(cell(row, &idx, "relationships_json"), "instance relationships")?;
            let photos: Vec<Photo> =
                parse_json_cell(cell(row, &idx, "photos_json"), "instance photos")?;
            let created_at = parse_id(cell(row, &idx, "created_at"), "instance created_at")?;
            let updated_at = parse_id(cell(row, &idx, "updated_at"), "instance updated_at")?;
            inv.instances.insert(
                id,
                Instance {
                    id,
                    class,
                    name,
                    fields,
                    tags,
                    parent,
                    photos,
                    relationships,
                    created_at,
                    updated_at,
                },
            );
        }
    }

    Ok(inv)
}

/// Parse a JSON cell into `T`, defaulting an empty cell to `T::default()`.
fn parse_json_cell<T>(cell: &str, ctx: &str) -> Result<T, StoreError>
where
    T: serde::de::DeserializeOwned + Default,
{
    let t = cell.trim();
    if t.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(t)
        .map_err(|e| StoreError::Backend(format!("gsheet: bad {ctx} JSON {cell:?}: {e}")))
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
/// Accepts the common shapes, including the standard share/edit URL a user
/// copies straight out of the browser address bar:
/// * `https://docs.google.com/spreadsheets/d/<ID>/edit?usp=sharing`
/// * `https://docs.google.com/spreadsheets/d/<ID>/edit#gid=0`
/// * `https://docs.google.com/spreadsheets/d/<ID>/export?format=csv`
/// * `https://docs.google.com/spreadsheets/d/e/<PUBLISHED_ID>/pub?output=csv`
///
/// Falls back to a `?id=<ID>` / `&id=<ID>` query parameter. Returns
/// [`StoreError::Backend`] when no id can be found.
fn parse_spreadsheet_id(url: &str) -> Result<String, StoreError> {
    // A path segment ends at the next `/`, `?` or `#`.
    let segment = |s: &str| -> String {
        s.chars()
            .take_while(|&c| c != '/' && c != '?' && c != '#')
            .collect()
    };
    // Path form: .../d/<id>/...  (the published form .../d/e/<id> is also valid;
    // we take whatever segment follows /d/, which for /d/e/<id> would be "e" —
    // so special-case the published shape first.)
    if let Some(after) = url.split("/d/e/").nth(1) {
        let id = segment(after);
        if !id.is_empty() {
            return Ok(id);
        }
    }
    if let Some(after) = url.split("/d/").nth(1) {
        let id = segment(after);
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
/// [`UreqTransport`]; implemented by a fake in tests so the grid mapping and
/// optimistic retry loop run without a network.
trait Transport: Send + Sync {
    /// `GET url`, returning the raw response text (used for the public CSV path).
    fn get_text(&self, url: &str) -> Result<String, StoreError>;

    /// `GET url` with an OAuth bearer `token`, returning the parsed JSON body.
    fn get_json(&self, url: &str, token: &str) -> Result<Value, StoreError>;

    /// `POST url` with an OAuth bearer `token` and JSON `body`, returning the
    /// parsed JSON response.
    fn post_json(&self, url: &str, token: &str, body: &Value) -> Result<Value, StoreError>;

    /// `GET url` like a browser: send a browser User-Agent and `x-same-domain: 1`,
    /// and *persist any Set-Cookie into the shared cookie jar* so the subsequent
    /// `post_multipart` replays them. Returns the raw response text.
    ///
    /// Used by the anonymous (no-OAuth) write path to fetch `/edit` (capturing the
    /// COMPASS/NID cookies + the server-assigned sid and revision).
    fn get_browser(&self, url: &str) -> Result<String, StoreError>;

    /// `POST url` like a browser: a `multipart/form-data` body with a browser
    /// User-Agent, `x-same-domain: 1`, and the jarred cookies. Returns the raw
    /// response text. Used by the anonymous write path to call `/save`.
    fn post_multipart(
        &self,
        url: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<String, StoreError>;
}

/// Browser-like User-Agent. Google's private editor endpoints (`/edit`, `/save`)
/// throttle/refuse non-browser agents, so the anonymous write path must look like
/// a browser.
const BROWSER_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                          AppleWebKit/537.36 (KHTML, like Gecko) \
                          Chrome/126.0.0.0 Safari/537.36";

/// The live [`Transport`] over [`ureq`].
///
/// `ureq` performs **pure blocking I/O with no internal async runtime**, so —
/// unlike `reqwest::blocking`, whose embedded tokio runtime panics when dropped
/// inside the gateway's `spawn_blocking` context — it is safe to call from
/// within tokio's blocking pool. This is the entire reason the backend uses
/// `ureq` instead of `reqwest`.
///
/// The held [`ureq::Agent`] carries an automatic cookie jar (enabled by the
/// `cookies` feature). The anonymous write protocol depends on this: the
/// COMPASS/NID cookies set by `GET /edit` are replayed on the `POST /save` that
/// follows.
struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    fn new() -> Result<Self, StoreError> {
        let agent = ureq::AgentBuilder::new()
            .user_agent(BROWSER_UA)
            .redirects(5)
            .build();
        Ok(UreqTransport { agent })
    }
}

/// Translate a `ureq` error into a clear [`StoreError::Backend`].
///
/// `ureq` models an HTTP 4xx/5xx as `Error::Status(code, response)` and a
/// connect/transport failure (refused connection, DNS, TLS, timeout) as
/// `Error::Transport(_)`. Both become a `Backend` error with the status/body or
/// the transport message, so callers see a normal error and the worker never
/// panics.
fn ureq_error(verb: &str, url: &str, err: ureq::Error) -> StoreError {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            StoreError::Backend(format!("gsheet: {verb} {url} -> {code}: {body}"))
        }
        ureq::Error::Transport(t) => {
            StoreError::Backend(format!("gsheet: {verb} {url}: {t}"))
        }
    }
}

impl Transport for UreqTransport {
    fn get_text(&self, url: &str) -> Result<String, StoreError> {
        let resp = self
            .agent
            .get(url)
            .call()
            .map_err(|e| ureq_error("GET", url, e))?;
        resp.into_string()
            .map_err(|e| StoreError::Backend(format!("gsheet: read GET {url}: {e}")))
    }

    fn get_json(&self, url: &str, token: &str) -> Result<Value, StoreError> {
        let resp = self
            .agent
            .get(url)
            .set("Authorization", &auth_header(token))
            .call()
            .map_err(|e| ureq_error("GET", url, e))?;
        resp.into_json::<Value>()
            .map_err(|e| StoreError::Backend(format!("gsheet: decode GET {url}: {e}")))
    }

    fn post_json(&self, url: &str, token: &str, body: &Value) -> Result<Value, StoreError> {
        let resp = self
            .agent
            .post(url)
            .set("Authorization", &auth_header(token))
            .send_json(body.clone())
            .map_err(|e| ureq_error("POST", url, e))?;
        resp.into_json::<Value>()
            .map_err(|e| StoreError::Backend(format!("gsheet: decode POST {url}: {e}")))
    }

    fn get_browser(&self, url: &str) -> Result<String, StoreError> {
        // The agent already sends BROWSER_UA and jars Set-Cookie automatically.
        let resp = self
            .agent
            .get(url)
            .set("x-same-domain", "1")
            .call()
            .map_err(|e| ureq_error("GET", url, e))?;
        resp.into_string()
            .map_err(|e| StoreError::Backend(format!("gsheet: read GET {url}: {e}")))
    }

    fn post_multipart(
        &self,
        url: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<String, StoreError> {
        let resp = self
            .agent
            .post(url)
            .set("content-type", content_type)
            .set("x-same-domain", "1")
            .send_bytes(body)
            .map_err(|e| ureq_error("POST", url, e))?;
        resp.into_string()
            .map_err(|e| StoreError::Backend(format!("gsheet: read POST {url}: {e}")))
    }
}

// ===========================================================================
// Credentials and access modes.
// ===========================================================================

/// Which access mode a [`GSheetStore`] was constructed for, with its
/// server-resolved credential and target.
enum Access {
    /// Read **and write** a link-shared sheet ("anyone with link can edit") with
    /// NO credential. The raw public URL is kept and the spreadsheet id is parsed
    /// lazily at read time, so constructing the store never fails (matching the
    /// factory contract that a public URL always `open`s; an unparseable URL
    /// surfaces as a [`StoreError::Backend`] from the first `load`).
    ///
    /// Reads use the unauthenticated gviz CSV export; writes use the anonymous
    /// `/edit` + `/save` editor protocol (see the "Anonymous read-WRITE protocol"
    /// section). Both speak the multi-tab layout (`Meta`/`Classes`/`Instances`).
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
    /// Open a READ-WRITE store over a link-shared ("anyone with link can edit")
    /// sheet at `url`, with NO credential.
    ///
    /// The spreadsheet id is parsed from the URL. Reads use the unauthenticated
    /// gviz CSV export; writes replicate the Sheets web-editor's anonymous
    /// `/edit` + `/save` protocol (cookies only). Both use the multi-tab layout
    /// (`Meta`/`Classes`/`Instances`).
    pub fn public_url(url: &str) -> Result<Self, StoreError> {
        Ok(GSheetStore {
            access: Access::PublicUrl {
                url: url.to_string(),
            },
            transport: Box::new(UreqTransport::new()?),
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
            transport: Box::new(UreqTransport::new()?),
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
            transport: Box::new(UreqTransport::new()?),
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
    ///
    /// Only the OAuth / AppHosted (Sheets API v4) paths use this NATIVE multi-tab
    /// grid layout (per-class tabs + `_classes`/`_class_fields`/...). The PublicUrl
    /// path uses its own three-tab anonymous layout via [`read_public_inventory`] /
    /// [`write_public_inventory`] instead, and never reaches here.
    fn read_all_grids(&self) -> Result<BTreeMap<String, Grid>, StoreError> {
        match &self.access {
            Access::PublicUrl { .. } => unreachable!(
                "PublicUrl uses the anonymous multi-tab read path \
                 (read_public_inventory), not the native API grid path"
            ),
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

    // --- Anonymous (PublicUrl) multi-tab read/write -------------------------

    /// Read the three anonymous tabs (`Meta`, `Classes`, `Instances`) of a
    /// link-shared sheet via gviz CSV and reconstruct the [`Inventory`]. A
    /// never-written (blank) sheet reads back as an empty inventory.
    ///
    /// Each tab is fetched by name. On this kind of anonymous link-shared sheet a
    /// gviz request for a missing tab silently returns the first sheet rather than
    /// an error; that is harmless here because (a) `Instances`/`Classes` rows are
    /// validated by header column (a stray `Sheet1` doc has no `id`/`name` header,
    /// so its rows are skipped) and (b) `Meta` only consumes a `next_id` row that a
    /// foreign sheet will not carry. So a partially-created sheet still reads
    /// cleanly; the first write creates whatever tabs are missing.
    fn read_public_inventory(&self, id: &str) -> Result<Inventory, StoreError> {
        let mut grids: BTreeMap<String, Grid> = BTreeMap::new();
        for (tab, _gid) in ANON_TABS {
            let csv = self.transport.get_text(&gviz_csv_url(id, tab))?;
            grids.insert(tab.to_string(), parse_csv(&csv));
        }
        anon_grids_to_inventory(&grids)
    }

    /// Write the full [`Inventory`] back to the `Meta`/`Classes`/`Instances` tabs
    /// of a link-shared sheet using the anonymous `/edit` + `/save` editor
    /// protocol (no credential).
    ///
    /// Steps: GET `/edit` (jars cookies, parses the server `sid` + current
    /// `revision`, and the list of existing tab names); then POST one `/save`
    /// bundle. The bundle first emits an add-sheet command for any of the three
    /// tabs that don't exist yet, then emits set-cell commands writing each tab's
    /// header + data rows, blanking any trailing cells a previously-larger tab
    /// left behind. Add-sheet commands precede set-cells so a freshly-created
    /// tab's cells land in the same bundle (verified live).
    fn write_public_inventory(&self, id: &str, inv: &Inventory) -> Result<(), StoreError> {
        // 1. GET /edit -> cookies + sid + revision + existing tab names.
        let html = self.transport.get_browser(&anon_edit_url(id))?;
        let rev = parse_html_revision(&html).ok_or_else(|| {
            StoreError::Backend(
                "gsheet anonymous write: could not parse \"revision\" from /edit HTML \
                 (is the sheet reachable and link-shared?)"
                    .to_string(),
            )
        })?;
        let sid = parse_html_sid(&html).ok_or_else(|| {
            StoreError::Backend(
                "gsheet anonymous write: could not parse server \"sid\" from /edit HTML"
                    .to_string(),
            )
        })?;
        let existing = parse_html_tab_names(&html);

        // 2. Build the command list: add-sheet for missing tabs FIRST, then
        //    set-cell for every cell across all three tabs.
        let new_grids = inventory_to_anon_grids(inv);
        let mut cmds: Vec<AnonCmd> = Vec::new();

        // Add-sheet commands for any missing tab, positioned after Sheet1.
        for (i, (tab, gid)) in ANON_TABS.iter().enumerate() {
            if !existing.contains(*tab) {
                cmds.push(AnonCmd::AddSheet {
                    gid: gid.to_string(),
                    name: tab.to_string(),
                    index: (i + 1) as i64,
                });
            }
        }

        // Set-cell commands per tab. For each tab, write every cell of the new
        // grid, then blank any trailing rows the OLD tab had beyond the new
        // content (read per-tab via gviz, only for tabs that already existed).
        for (tab, gid) in ANON_TABS {
            let new_grid = new_grids.get(tab).cloned().unwrap_or_default();
            let new_cols = new_grid.iter().map(Vec::len).max().unwrap_or(0);
            let old_grid = if existing.contains(tab) {
                let csv = self.transport.get_text(&gviz_csv_url(id, tab)).unwrap_or_default();
                parse_csv(&csv)
            } else {
                Vec::new()
            };

            for (r, row) in new_grid.iter().enumerate() {
                for (c, val) in row.iter().enumerate() {
                    cmds.push(AnonCmd::SetCell {
                        gid: gid.to_string(),
                        row: r as i64,
                        col: c as i64,
                        value: val.clone(),
                    });
                }
            }
            // Blank stale trailing rows that existed before but not now.
            for (r, old_row) in old_grid.iter().enumerate().skip(new_grid.len()) {
                let old_cols = old_row.len().max(new_cols);
                for c in 0..old_cols {
                    cmds.push(AnonCmd::SetCell {
                        gid: gid.to_string(),
                        row: r as i64,
                        col: c as i64,
                        value: String::new(),
                    });
                }
            }
        }

        // 3. POST /save.
        let bundles = anon_build_bundles(&sid, 0, &cmds);
        let (ct, body) = anon_multipart(&[("rev", &rev.to_string()), ("bundles", &bundles)]);
        let resp = self
            .transport
            .post_multipart(&anon_save_url(id, &sid), &ct, &body)?;
        let new_rev = parse_save_response(&resp)?;
        if new_rev <= rev {
            return Err(StoreError::Backend(format!(
                "gsheet anonymous write: revision did not advance (was {rev}, got {new_rev})"
            )));
        }
        Ok(())
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
            // PublicUrl writes go through write_public_inventory (anonymous
            // multi-tab layout), never this native API grid path.
            Access::PublicUrl { .. } => unreachable!(
                "PublicUrl uses the anonymous multi-tab write path (write_public_inventory)"
            ),
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
        // PublicUrl uses the anonymous multi-tab layout (gviz read of the
        // Meta/Classes/Instances tabs).
        if let Access::PublicUrl { url } = &self.access {
            let id = parse_spreadsheet_id(url)?;
            return self.read_public_inventory(&id);
        }
        let grids = self.read_all_grids()?;
        grids_to_inventory(&grids)
    }

    fn transact_dyn(
        &self,
        f: &mut dyn FnMut(&mut Inventory) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        // PublicUrl: anonymous (no-OAuth) read-modify-write over the multi-tab
        // (Meta/Classes/Instances) layout.
        //
        // Sheets has no CAS, so this is the same optimistic loop as the API path,
        // keyed on the on-sheet `next_id`-bearing inventory state rather than a
        // separate version row: read, mutate, re-read to verify nothing changed,
        // then write. (The save itself fails if the revision moved under us, so a
        // racing write surfaces as a Backend error and we retry.)
        if let Access::PublicUrl { url } = &self.access {
            let id = parse_spreadsheet_id(url)?;
            for _ in 0..MAX_ATTEMPTS {
                let before = self.read_public_inventory(&id)?;
                let mut inv = before.clone();
                f(&mut inv)?;
                // Optimistic verify: re-read; only write if unchanged meanwhile.
                let observed = self.read_public_inventory(&id)?;
                if observed != before {
                    continue;
                }
                match self.write_public_inventory(&id, &inv) {
                    Ok(()) => return Ok(()),
                    // A stale revision (a concurrent committer) is reported by the
                    // server; retry by reloading the latest state.
                    Err(StoreError::Backend(m)) if m.contains("revision") => continue,
                    Err(e) => return Err(e),
                }
            }
            return Err(StoreError::Conflict);
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

    // --- anonymous protocol: pure encoding / parsing -----------------------

    #[test]
    fn anon_multi_tab_roundtrip_lossless() {
        // The multi-tab (Meta/Classes/Instances) layout round-trips an arbitrary
        // inventory.
        let inv = rich_inventory();
        let grids = inventory_to_anon_grids(&inv);
        let back = anon_grids_to_inventory(&grids).unwrap();
        assert_eq!(inv, back, "multi-tab layout: to ∘ from == identity");
    }

    #[test]
    fn anon_multi_tab_has_three_named_tabs() {
        let inv = rich_inventory();
        let grids = inventory_to_anon_grids(&inv);
        // Exactly the three fixed tabs, each with its header row.
        let names: BTreeSet<&str> = grids.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [ANON_TAB_META, ANON_TAB_CLASSES, ANON_TAB_INSTANCES]
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
        assert_eq!(grids[ANON_TAB_META][0], anon_meta_header());
        assert_eq!(grids[ANON_TAB_CLASSES][0], anon_classes_header());
        assert_eq!(grids[ANON_TAB_INSTANCES][0], anon_instances_header());
        // Classes/Instances each carry the right number of data rows.
        assert_eq!(grids[ANON_TAB_CLASSES].len() - 1, inv.classes.len());
        assert_eq!(grids[ANON_TAB_INSTANCES].len() - 1, inv.instances.len());
    }

    #[test]
    fn anon_multi_tab_empty_inventory_roundtrip() {
        let inv = Inventory::new();
        let grids = inventory_to_anon_grids(&inv);
        // Each tab is header-only (no data rows) except Meta's key/value rows.
        assert_eq!(grids[ANON_TAB_CLASSES].len(), 1, "Classes header only");
        assert_eq!(grids[ANON_TAB_INSTANCES].len(), 1, "Instances header only");
        let back = anon_grids_to_inventory(&grids).unwrap();
        assert_eq!(inv, back);
        assert_eq!(back.next_id, 1);
    }

    #[test]
    fn anon_multi_tab_through_gviz_csv_roundtrip() {
        // Prove the read path (gviz CSV) and write path (grids) agree per tab:
        // encode -> CSV -> parse_csv -> reconstruct losslessly.
        let inv = rich_inventory();
        let grids = inventory_to_anon_grids(&inv);
        let mut csv_grids: BTreeMap<String, Grid> = BTreeMap::new();
        for (tab, grid) in &grids {
            let csv = flat_grid_to_gviz_csv(grid);
            csv_grids.insert(tab.clone(), parse_csv(&csv));
        }
        let back = anon_grids_to_inventory(&csv_grids).unwrap();
        assert_eq!(inv, back, "multi-tab layout survives a gviz CSV round-trip");
    }

    #[test]
    fn anon_missing_tabs_read_as_empty() {
        // A sheet where Classes/Instances tabs do not yet exist (gviz silently
        // serves an unrelated doc) must not corrupt the read: foreign rows lacking
        // the expected headers are skipped, yielding an empty inventory.
        let mut grids: BTreeMap<String, Grid> = BTreeMap::new();
        // Simulate gviz returning Sheet1's (empty) doc for every named tab.
        grids.insert(ANON_TAB_META.into(), Vec::new());
        grids.insert(ANON_TAB_CLASSES.into(), vec![vec!["stray".into()]]);
        grids.insert(ANON_TAB_INSTANCES.into(), vec![vec!["stray".into()]]);
        let inv = anon_grids_to_inventory(&grids).unwrap();
        assert!(inv.classes.is_empty());
        assert!(inv.instances.is_empty());
        assert_eq!(inv.next_id, 1);
    }

    #[test]
    fn anon_inner_set_cell_matches_captured_shape() {
        // Captured verbatim: A1="claudeprobe777" on gid "0".
        let inner = anon_inner_set_cell("0", 0, 0, "claudeprobe777");
        assert_eq!(
            inner,
            "[[\"0\",0,1,0,1],[132274236,3,[2,\"claudeprobe777\"],null,null,0],\
             [null,[[null,513,[0],null,null,null,null,null,null,null,null,0]]]]"
        );
    }

    #[test]
    fn anon_inner_add_sheet_matches_captured_shape() {
        // Captured verbatim: add "Sheet2" gid 1645475122 at index 2.
        let inner = anon_inner_add_sheet("1645475122", "Sheet2");
        assert_eq!(
            inner,
            "[1,0,\"1645475122\",[[[0,0,\"Sheet2\"],[2,0,null,null,0],\
             [3,0,null,null,null,0],[4,0,null,null,null,null,0],\
             [5,0,null,null,null,null,null,0],[6,0,null,null,null,null,null,null,0]]],1000,26]"
        );
        // Captured verbatim: index positioner for index 2.
        assert_eq!(anon_inner_add_index(2), "[[[[4,0,null,null,2]]]]");
    }

    #[test]
    fn anon_build_bundles_set_cell_shape() {
        let cmds = vec![AnonCmd::SetCell {
            gid: ANON_GID_CLASSES.into(),
            row: 1,
            col: 1,
            value: "v".into(),
        }];
        let bundles = anon_build_bundles("mysid", 0, &cmds);
        let v: Value = serde_json::from_str(&bundles).unwrap();
        assert_eq!(v[0]["sid"], "mysid");
        assert_eq!(v[0]["reqId"], 0);
        // commands[0] = [OP_BUNDLE, "<inner string>"]
        assert_eq!(v[0]["commands"][0][0], OP_BUNDLE);
        let inner = v[0]["commands"][0][1].as_str().unwrap();
        assert_eq!(inner, anon_inner_set_cell(ANON_GID_CLASSES, 1, 1, "v"));
    }

    #[test]
    fn anon_build_bundles_add_sheet_then_set_cell() {
        // A mixed bundle: an add-sheet command followed by a set-cell into it.
        let cmds = vec![
            AnonCmd::AddSheet {
                gid: ANON_GID_CLASSES.into(),
                name: ANON_TAB_CLASSES.into(),
                index: 2,
            },
            AnonCmd::SetCell {
                gid: ANON_GID_CLASSES.into(),
                row: 0,
                col: 0,
                value: "name".into(),
            },
        ];
        let bundles = anon_build_bundles("s", 4, &cmds);
        let v: Value = serde_json::from_str(&bundles).unwrap();
        assert_eq!(v[0]["reqId"], 4);
        let cmds_arr = v[0]["commands"].as_array().unwrap();
        assert_eq!(cmds_arr.len(), 2);
        // First command is the compound add-sheet.
        assert_eq!(cmds_arr[0][0], OP_ADD_SHEET);
        assert_eq!(cmds_arr[0][1][0][0], OP_ADD_SHEET_BODY);
        assert_eq!(cmds_arr[0][1][1][0], OP_ADD_SHEET_INDEX);
        // Second is the set-cell.
        assert_eq!(cmds_arr[1][0], OP_BUNDLE);
    }

    #[test]
    fn anon_parse_tab_names_from_html() {
        let html = "x<div class=\"goog-inline-block docs-sheet-tab-caption\">Sheet1</div>\
                    y<span class=\"foo docs-sheet-tab-caption\">Classes</span>\
                    z<div class=\"docs-sheet-tab-caption\">A&amp;B</div>";
        let names = parse_html_tab_names(html);
        assert!(names.contains("Sheet1"));
        assert!(names.contains("Classes"));
        assert!(names.contains("A&B"), "entities decoded");
        assert!(!names.contains("Instances"));
    }

    #[test]
    fn anon_parse_revision_and_sid_from_html() {
        let html = "junk...\"revision\":42,\"sid\":\"abc123def456\",\"oui\":\"ANONYMOUS_9\"...end";
        assert_eq!(parse_html_revision(html), Some(42));
        assert_eq!(parse_html_sid(html).as_deref(), Some("abc123def456"));
        // Missing fields -> None.
        assert_eq!(parse_html_revision("nothing here"), None);
        assert_eq!(parse_html_sid("nothing here"), None);
    }

    #[test]
    fn anon_parse_save_response_success_and_error() {
        let ok = ")]}'\n{\"revisionRanges\":[[7,7]],\"metadata\":{\"serverRevision\":6}}";
        assert_eq!(parse_save_response(ok).unwrap(), 7);
        // Multi-command save: take the max hi.
        let multi = ")]}'\n{\"revisionRanges\":[[10,11],[12,14]]}";
        assert_eq!(parse_save_response(multi).unwrap(), 14);
        // Channel error response surfaces as a Backend error.
        let err = ")]}'\n\n[[\"er\",null,null,null,null,550,null,null,null,13],[\"di\",37]]";
        assert!(parse_save_response(err).is_err());
        // No revisionRanges is also an error.
        let nope = ")]}'\n{\"metadata\":{}}";
        assert!(parse_save_response(nope).is_err());
    }

    #[test]
    fn anon_multipart_encodes_fields() {
        let (ct, body) = anon_multipart(&[("rev", "6"), ("bundles", "[{}]")]);
        assert!(ct.starts_with("multipart/form-data; boundary="));
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("name=\"rev\"\r\n\r\n6\r\n"));
        assert!(text.contains("name=\"bundles\"\r\n\r\n[{}]\r\n"));
        assert!(text.trim_end().ends_with("--"));
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
        // The standard share/edit URL copied straight from the browser.
        assert_eq!(
            parse_spreadsheet_id(
                "https://docs.google.com/spreadsheets/d/ABC123/edit?usp=sharing"
            )
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

        // The OAuth/AppHosted fake never exercises the anonymous browser path.
        fn get_browser(&self, _url: &str) -> Result<String, StoreError> {
            Err(StoreError::Backend("fake: get_browser unused".into()))
        }
        fn post_multipart(
            &self,
            _url: &str,
            _content_type: &str,
            _body: &[u8],
        ) -> Result<String, StoreError> {
            Err(StoreError::Backend("fake: post_multipart unused".into()))
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

    // --- fake transport for the anonymous (PublicUrl) multi-tab path ---------

    /// A fake link-shared sheet held in memory as a set of named tabs (each a
    /// [`Grid`], also keyed by its gid). It serves:
    /// * `get_text`      (gviz CSV) -> the named tab's grid as CSV (a *missing*
    ///   tab silently serves the default first tab `Sheet1`, mimicking the real
    ///   anonymous gviz fallback);
    /// * `get_browser`   (`/edit`)  -> minimal HTML carrying `"revision":N`, a
    ///   server `"sid"`, and one `docs-sheet-tab-caption` marker per existing tab;
    /// * `post_multipart`(`/save`)  -> parses the `bundles` field, applies each
    ///   add-sheet (creates a tab keyed by gid+name) and set-cell command, bumps
    ///   the revision, returns a `/save`-shaped response. This drives the whole
    ///   anonymous multi-tab transact loop without a network.
    struct FakeAnonSheet {
        /// tab name -> grid.
        tabs: Mutex<BTreeMap<String, Grid>>,
        /// gid -> tab name (so set-cell, addressed by gid, finds its tab).
        gids: Mutex<BTreeMap<String, String>>,
        revision: Mutex<i64>,
        sid: String,
    }

    impl FakeAnonSheet {
        /// A brand-new link-shared sheet: just `Sheet1` (gid "0"), empty, like a
        /// freshly link-shared sheet the multi-tab layout has never touched.
        fn new(_inv: Inventory) -> Arc<Self> {
            let mut tabs = BTreeMap::new();
            tabs.insert("Sheet1".to_string(), Vec::new());
            let mut gids = BTreeMap::new();
            gids.insert("0".to_string(), "Sheet1".to_string());
            Arc::new(FakeAnonSheet {
                tabs: Mutex::new(tabs),
                gids: Mutex::new(gids),
                revision: Mutex::new(6),
                sid: "0123456789abcdef".to_string(),
            })
        }
        fn revision(&self) -> i64 {
            *self.revision.lock().unwrap()
        }
        /// Snapshot a named tab's grid (empty if absent).
        fn tab(&self, name: &str) -> Grid {
            self.tabs.lock().unwrap().get(name).cloned().unwrap_or_default()
        }
    }

    /// Extract the `sheet=<name>` query parameter from a gviz URL (urldecoded).
    fn gviz_tab_from_url(url: &str) -> String {
        url.split("sheet=")
            .nth(1)
            .map(|s| {
                let raw: String = s.chars().take_while(|&c| c != '&').collect();
                url_decode(&raw)
            })
            .unwrap_or_default()
    }

    /// Serialize a grid to gviz-style CSV (quoting every cell, like gviz does),
    /// trimming trailing fully-empty rows the way gviz output does.
    fn flat_grid_to_gviz_csv(grid: &Grid) -> String {
        // Drop trailing all-empty rows.
        let last = grid
            .iter()
            .rposition(|r| r.iter().any(|c| !c.is_empty()))
            .map(|i| i + 1)
            .unwrap_or(0);
        let mut out = String::new();
        for row in &grid[..last] {
            // Trim trailing empty cells per row (gviz does this too).
            let last_col = row
                .iter()
                .rposition(|c| !c.is_empty())
                .map(|i| i + 1)
                .unwrap_or(0);
            let cells: Vec<String> = row[..last_col]
                .iter()
                .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                .collect();
            out.push_str(&cells.join(","));
            out.push('\n');
        }
        out
    }

    impl Transport for Arc<FakeAnonSheet> {
        fn get_text(&self, url: &str) -> Result<String, StoreError> {
            let name = gviz_tab_from_url(url);
            let tabs = self.tabs.lock().unwrap();
            // Mimic the real anonymous gviz fallback: a missing named tab serves
            // the default first sheet (Sheet1) instead of erroring.
            let grid = tabs
                .get(&name)
                .or_else(|| tabs.get("Sheet1"))
                .cloned()
                .unwrap_or_default();
            Ok(flat_grid_to_gviz_csv(&grid))
        }
        fn get_json(&self, _u: &str, _t: &str) -> Result<Value, StoreError> {
            unreachable!("anon path never calls get_json")
        }
        fn post_json(&self, _u: &str, _t: &str, _b: &Value) -> Result<Value, StoreError> {
            unreachable!("anon path never calls post_json")
        }
        fn get_browser(&self, _url: &str) -> Result<String, StoreError> {
            let rev = *self.revision.lock().unwrap();
            // Render one tab-caption marker per existing tab so the write path can
            // discover which of Meta/Classes/Instances already exist.
            let captions: String = self
                .tabs
                .lock()
                .unwrap()
                .keys()
                .map(|n| format!("<div class=\"docs-sheet-tab-caption\">{n}</div>"))
                .collect();
            Ok(format!(
                "<html>...{captions}...\"revision\":{rev},\"sid\":\"{}\",\
                 \"oui\":\"ANONYMOUS_1\"...</html>",
                self.sid
            ))
        }
        fn post_multipart(
            &self,
            _url: &str,
            _content_type: &str,
            body: &[u8],
        ) -> Result<String, StoreError> {
            let body = String::from_utf8_lossy(body);
            // Extract the `bundles` multipart field value (between its blank line
            // and the trailing CRLF before the next boundary).
            let marker = "name=\"bundles\"\r\n\r\n";
            let start = body.find(marker).expect("bundles field present") + marker.len();
            let rest = &body[start..];
            let end = rest.find("\r\n--").unwrap_or(rest.len());
            let bundles_json = &rest[..end];

            let bundles: Value = serde_json::from_str(bundles_json).expect("bundles JSON");
            let mut tabs = self.tabs.lock().unwrap();
            let mut gids = self.gids.lock().unwrap();
            for bundle in bundles.as_array().unwrap() {
                for cmd in bundle["commands"].as_array().unwrap() {
                    let op = cmd[0].as_i64().unwrap();
                    if op == OP_ADD_SHEET {
                        // cmd = [OP_ADD_SHEET, [[OP_ADD_SHEET_BODY, "<body>"], ...]]
                        let body_str = cmd[1][0][1].as_str().unwrap();
                        let inner: Value = serde_json::from_str(body_str).unwrap();
                        // inner = [1,0,"<gid>",[[[0,0,"<name>"],...]],1000,26]
                        let gid = inner[2].as_str().unwrap().to_string();
                        let name = inner[3][0][0][2].as_str().unwrap().to_string();
                        assert!(
                            !tabs.contains_key(&name),
                            "add-sheet for already-existing tab {name:?}"
                        );
                        tabs.insert(name.clone(), Vec::new());
                        gids.insert(gid, name);
                        continue;
                    }
                    // cmd = [OP_BUNDLE, "<inner JSON string>"] (set-cell)
                    let inner_str = cmd[1].as_str().unwrap();
                    let inner: Value = serde_json::from_str(inner_str).unwrap();
                    // inner[0] = [gid, row, row+1, col, col+1]
                    let coords = inner[0].as_array().unwrap();
                    let gid = coords[0].as_str().unwrap().to_string();
                    let row = coords[1].as_i64().unwrap() as usize;
                    let c = coords[3].as_i64().unwrap() as usize;
                    // inner[1] = [OP_SET_CELL, 3, [2, "<v>"], ...]
                    let v = inner[1][2][1].as_str().unwrap().to_string();
                    let name = gids
                        .get(&gid)
                        .cloned()
                        .unwrap_or_else(|| panic!("set-cell on unknown gid {gid:?}"));
                    let grid = tabs.entry(name).or_default();
                    while grid.len() <= row {
                        grid.push(Vec::new());
                    }
                    while grid[row].len() <= c {
                        grid[row].push(String::new());
                    }
                    grid[row][c] = v;
                }
            }
            drop(tabs);
            drop(gids);
            let mut rev = self.revision.lock().unwrap();
            *rev += 1;
            let new = *rev;
            Ok(format!(
                ")]}}'\n{{\"revisionRanges\":[[{new},{new}]],\"metadata\":{{\"serverRevision\":{}}}}}",
                new - 1
            ))
        }
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
            fn get_browser(&self, _u: &str) -> Result<String, StoreError> {
                unreachable!()
            }
            fn post_multipart(&self, _u: &str, _c: &str, _b: &[u8]) -> Result<String, StoreError> {
                unreachable!()
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
    fn public_url_read_write_roundtrip_via_fake_anon_transport() {
        // PublicUrl is now READ-WRITE: the anonymous multi-tab transact reads the
        // gviz CSV of each tab, applies the closure, and writes back via the
        // /edit+/save protocol (creating the Meta/Classes/Instances tabs). A fake
        // transport models the sheet as a set of named tabs in memory.
        let fake = FakeAnonSheet::new(Inventory::new());
        let store = GSheetStore::with_transport(
            Access::PublicUrl {
                url: "https://docs.google.com/spreadsheets/d/SID/edit".into(),
            },
            Box::new(fake.clone()),
        );

        // A fresh sheet loads as empty.
        assert!(store.load().unwrap().instances.is_empty());

        // Write through transact.
        let id = store
            .transact(&mut |inv| {
                inv.add_instance("Item", "thing", BTreeMap::new(), None, 1)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            })
            .expect("anonymous transact should commit");
        assert_eq!(id, 1);

        // Read it back through the same store.
        let back = store.load().expect("reload");
        assert_eq!(back.get(1).unwrap().name, "thing");
        // The revision advanced on the underlying fake.
        assert!(fake.revision() > 6);

        // The three tabs now exist as SEPARATE grids, and the instance lives on
        // the Instances tab (not Classes/Meta).
        for tab in [ANON_TAB_META, ANON_TAB_CLASSES, ANON_TAB_INSTANCES] {
            assert!(!fake.tab(tab).is_empty(), "tab {tab} should exist with a header");
        }
        let instances = fake.tab(ANON_TAB_INSTANCES);
        assert_eq!(instances[0], anon_instances_header());
        assert!(
            instances.iter().skip(1).any(|r| r.get(2).map(String::as_str) == Some("thing")),
            "instance 'thing' is on the Instances tab"
        );
        let classes = fake.tab(ANON_TAB_CLASSES);
        assert_eq!(classes[0], anon_classes_header());
        assert!(
            classes.iter().skip(1).any(|r| r.first().map(String::as_str) == Some("Item")),
            "class 'Item' is on the Classes tab"
        );
    }

    #[test]
    fn public_anon_lossless_roundtrip_rich_inventory() {
        // The full rich inventory round-trips through the multi-tab anon path.
        let fake = FakeAnonSheet::new(Inventory::new());
        let store = GSheetStore::with_transport(
            Access::PublicUrl {
                url: "https://docs.google.com/spreadsheets/d/SID/edit".into(),
            },
            Box::new(fake.clone()),
        );
        let inv = rich_inventory();
        let target = inv.clone();
        store
            .transact(&mut |cur| {
                *cur = target.clone();
                Ok(())
            })
            .expect("write rich inventory");
        let back = store.load().expect("reload rich inventory");
        assert_eq!(back, inv, "anon multi-tab round-trip is lossless");
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
    fn public_blank_sheet_reads_as_empty_inventory() {
        // A brand-new blank link-shared sheet (gviz returns an empty document for
        // every tab) now reads back as an empty inventory — not an error — because
        // the multi-tab layout tolerates never-written Meta/Classes/Instances tabs
        // and the anon path creates them on first write.

        /// A fake transport whose gviz CSV read returns an empty document.
        struct BlankPublic;
        impl Transport for BlankPublic {
            fn get_text(&self, _url: &str) -> Result<String, StoreError> {
                Ok(String::new())
            }
            fn get_json(&self, _url: &str, _token: &str) -> Result<Value, StoreError> {
                unreachable!("public path never calls get_json")
            }
            fn post_json(
                &self,
                _url: &str,
                _token: &str,
                _body: &Value,
            ) -> Result<Value, StoreError> {
                unreachable!("public path never calls post_json")
            }
            fn get_browser(&self, _url: &str) -> Result<String, StoreError> {
                unreachable!("load() never calls get_browser")
            }
            fn post_multipart(
                &self,
                _url: &str,
                _content_type: &str,
                _body: &[u8],
            ) -> Result<String, StoreError> {
                unreachable!("load() never calls post_multipart")
            }
        }

        let store = GSheetStore::with_transport(
            Access::PublicUrl {
                url: "https://docs.google.com/spreadsheets/d/BLANK/edit?usp=sharing".into(),
            },
            Box::new(BlankPublic),
        );
        let inv = store.load().expect("blank sheet loads as empty inventory");
        assert!(inv.instances.is_empty());
        assert!(inv.classes.is_empty());
        assert_eq!(inv.next_id, 1);
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
