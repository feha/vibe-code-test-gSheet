# AGENTS.md — working guide for this repo

Operational guide for a developer (and their AI assistant) picking this up. Read
`PLAN.md` for status/roadmap and `CONVERSATION.md` for *why* things are the way
they are. The user-facing overview is `README.md`.

## What this is

A **"bring your own database" inventory manager**: define object **classes** and
**instances**; instances can **contain** other instances (a tree); plus tags,
photos, relationships, duplication, search, edit. The user points the app at a
store **they own** — a JSON file, a Postgres DB, or a Google Sheet — and that
store *is* the identity. **No accounts, no sign-up, no UUIDs** (object identity is
a store-native `i64`; classes are keyed by name).

## Hard constraints (do not violate)

- **Pure Rust + WebAssembly only.** The only non-Rust artifact allowed is the
  `wasm-bindgen` JS glue Trunk emits. No JS/TS app code, no Python, etc.
- **TDD.** Every feature/fix starts with a failing test. The suite is the spec.
  Property tests (`proptest`) guard the core invariants. Keep it green.
- **No UUIDs.** Instances use `i64` ids assigned by the store/model; classes are
  keyed by `String` name.

## Architecture (workspace crates)

```
crates/
  inv-model    Pure data + serde. No I/O, no time, no platform calls (host+wasm).
  inv-core     Domain logic over an in-memory `Inventory` (trait `InventoryExt`).
  inv-store    `Store` trait + `transact` + 3 backend adapters (file/postgres/gsheet).
  inv-server   axum HTTP gateway: opens a store by descriptor, runs ops, serves the SPA.
  inv-app      Leptos CSR WASM frontend (the GUI).
  inv-crypto   OBSOLETE leftover (capability-URL era). Unused; out of default-members.
```

Data flow: **`inv-app` (browser/WASM) → HTTP → `inv-server` gateway → `Store`
adapter → file / Postgres / Google Sheet.** The browser can't speak raw
Postgres/Sheets, so the gateway brokers. Every backend is behind one `Store`
trait, so `inv-core`, the gateway, and the UI are backend-agnostic.

### Data model (`inv-model/src/lib.rs`)
- `Inventory { classes: BTreeMap<String,Class>, instances: BTreeMap<i64,Instance>, next_id: i64 }`
  — `new()`, `next_instance_id()`, `to_json_bytes()/from_json_bytes()` (pretty JSON; this is the File format).
- `Class { name: String, fields: Vec<FieldDef>, created_at: i64 }`
- `FieldDef { name, field_type: FieldType{Text,Number,Bool,Date}, required }`
- `FieldValue` — serde-tagged `#[serde(tag="kind", content="value", rename_all="snake_case")]`.
  **On the wire a text value is `{"kind":"text","value":"red"}`, NOT a bare string.** (Common mistake.)
- `Instance { id:i64, class:String, name, fields:BTreeMap<String,FieldValue>, tags:BTreeSet<String>,
  parent:Option<i64>, photos:Vec<Photo{key,mime,name}>, relationships:Vec<Relationship{kind,target:i64}>,
  created_at, updated_at }`

### Domain logic (`inv-core/src/lib.rs`)
`trait InventoryExt for Inventory`. **Time is injected** — every mutator takes
`now: i64` (unix millis); the model never calls the clock (keeps it deterministic
+ wasm-safe). Ops: `ensure_class`, `add_instance`, `get`, `children_of`,
`descendants_of`, `path_of`, `roots`, `edit_instance(InstancePatch)`,
`move_instance` (rejects cycles → `WouldCycle`), `remove_instance(RemoveMode::{Cascade,Reparent})`,
`add_tag/remove_tag`, `add_relationship/remove_relationship`, `attach_photo/detach_photo`,
`duplicate_instance(deep)`, `search(SearchQuery{text,tag,class})`, `change_class`,
`delete_class` (refuses with `ClassInUse` while instances reference it; idempotent
when empty). `CoreError { NotFound(i64), WouldCycle, InvalidParent(i64), ClassInUse }`.

### Storage (`inv-store/src/lib.rs` + `file_store.rs`, `postgres.rs`, `gsheet.rs`)
- `trait Store: Send + Sync` is **object-safe**: implementors implement the
  non-generic `transact_dyn(&self, &mut dyn FnMut(&mut Inventory) -> Result<(),StoreError>)`;
  the ergonomic generic `transact<T>` lives on `trait StoreExt: Store` (blanket
  impl) so it works on `FileStore` AND `Box<dyn Store>`. **Callers need
  `use inv_store::StoreExt;`.**
- `transact` = load → apply closure → atomic commit → **retry on conflict**. This
  is the no-race-conditions guarantee; mechanism differs per backend.
- `StoreError { Conflict, NotFound, Io(String), Backend(String) }`.
- `StoreDescriptor { File{path}, Postgres{url}, GSheet{ mode: GSheetMode } }` where
  `GSheetMode { PublicUrl{url}, OAuth{spreadsheet_id}, AppHosted{spreadsheet_id:Option<String>} }`.
  `open(&StoreDescriptor) -> Result<Box<dyn Store>, StoreError>`.

| Adapter | Layout | Concurrency | Verified |
|---|---|---|---|
| **FileStore** | one JSON doc | `fs4` advisory lock + atomic rename | ✅ live, incl. cross-process |
| **PostgresStore** | relational tables (`classes`,`class_fields`,`instances`(parent FK),`instance_fields`,`instance_tags`,`relationships`,`photos`,`meta`) + views `v_instances`/`v_instance_fields` | `pg_advisory_xact_lock` per write txn | ✅ live, incl. 8×10 concurrency |
| **GSheet OAuth/AppHosted** | per-class tabs via Sheets API v4 | optimistic version cell | ⚠️ implemented, **unverified** (needs Google creds) |
| **GSheet PublicUrl (anonymous)** | `Meta`/`Classes`/`Instances` tabs via reverse-engineered web-editor protocol | optimistic (server stale-rev reject + retry) | ✅ live |

### Gateway (`inv-server/src/lib.rs`, `main.rs`)
- Stateless-ish: each request carries a `StoreDescriptor`; opens+caches stores in
  `HashMap<String, Arc<dyn Store>>` keyed by the serialized descriptor. Store ops
  run inside `tokio::task::spawn_blocking` (the `Store` is blocking). `now` =
  server `SystemTime`.
- Endpoints: `GET /api/health`; `POST /api/inventory` (body = descriptor → full
  `Inventory`); `POST /api/op` (body = `{store, op}` → full updated `Inventory`);
  `POST /api/photo/put`, `POST /api/photo/get`; static SPA fallback.
- `Op` enum: serde-tagged `#[serde(tag="op", rename_all="snake_case")]` — variants
  `add_instance, edit_instance, move_instance, remove_instance, duplicate_instance,
  add_tag, remove_tag, add_relationship, remove_relationship, change_class, delete_class`.
- `CoreError` → HTTP: `WouldCycle`/`InvalidParent`/`ClassInUse` → **409**, `NotFound` → **404**, else 400/500.
- Env: `INV_ADDR` (default `127.0.0.1:8080`), `INV_STATIC_DIR` (default `./dist`),
  and for Google write modes `INV_GSHEET_TOKEN` / `INV_GSHEET_OAUTH_CLIENT` /
  `INV_GSHEET_SERVICE_ACCOUNT` (server-side; not yet exercised — see PLAN.md).

### Frontend (`inv-app/`)
Leptos CSR. `api.rs` mirrors `StoreDescriptor`+`Op` as serde types and calls the
gateway via `gloo-net`. `state.rs` holds `AppState` (a `provide_context` struct of
`RwSignal`s: `descriptor`, `inventory`, `selected`, `current_container`, toast)
with a `run_op` pattern (build `Op` → `apply_op` → replace `inventory` from the
response); `search`/`class_names` run client-side via `InventoryExt`. `app.rs` =
`OpenDatabase` screen (3 backends) + `Workspace` layout. `components/` =
`add_form`, `navigator`, `search`, `detail`, `classes`.

## Build / test / run

```bash
# Backend tests (default-members: inv-model, inv-core, inv-store, inv-server)
cargo test            # ~118 tests; includes proptest invariants + live-ish concurrency
cargo clippy --all-targets   # keep clean

# Frontend (WASM) — Trunk; run from the crate dir (workspace root has no root package)
(cd crates/inv-app && trunk build --release)   # -> crates/inv-app/dist/

# Run the whole app
INV_STATIC_DIR=crates/inv-app/dist cargo run -p inv-server   # http://127.0.0.1:8080
```

`inv-app` is **excluded from default-members** (it's wasm; build with Trunk or
`cargo build -p inv-app --target wasm32-unknown-unknown`). `inv-crypto` is in
members but out of default-members.

### Live / gated tests (need external services)
- **Postgres** integration tests spin an *ephemeral cluster* (`initdb`+`pg_ctl` on
  a private unix socket, `--auth=trust`). Requires the `postgres` toolchain
  installed (`/opt/homebrew/bin` here). See the shm gotcha below.
- **Google Sheets live** tests/examples are `#[ignore]`d and hit a real
  *anyone-with-link-can-edit* sheet hardcoded in `tests/gsheet_anon_live.rs` /
  `examples/gsheet_*.rs`. **Point them at your own throwaway sheet** before:
  `cargo test -p inv-store --test gsheet_anon_live -- --ignored`.

## Gotchas (read before debugging)

1. **`reqwest::blocking` panics inside the gateway.** It spins its own tokio
   runtime; dropping it inside `spawn_blocking` panics ("Cannot drop a runtime…").
   `inv-store` uses **`ureq`** (runtime-free) for all HTTP. Do not reintroduce reqwest.
2. **`FieldValue` wire shape** is tagged (`{"kind":"text","value":...}`), not a bare
   value. JSON payloads to `/api/op` must use it.
3. **Postgres + macOS shared memory.** Each Postgres cluster needs one SysV shm
   segment; macOS `kern.sysv.shmmni` is 32. Killed test runs leak clusters and
   exhaust shm → `initdb: could not create shared memory segment`. Reap leaked
   *ephemeral* clusters (NOT your real instance):
   `pkill -f 'bin/postgres -D /var/folders'` then re-run.
4. **gviz lies about missing tabs** on anonymous link-shared sheets — it silently
   serves the first sheet instead of erroring. The anon Sheets adapter detects
   tabs by parsing the `/edit` HTML captions, not via gviz.
5. **Sheets anonymous opcodes are build-versioned + brittle.** See re-capture below.
6. **Store object-safety**: implement `transact_dyn`; call `.transact()` via `StoreExt`.

## Reverse-engineered anonymous Google Sheets protocol (`gsheet.rs`)

The `PublicUrl` mode writes to an "anyone-with-link-can-edit" sheet with **no
OAuth**, by replicating the web editor's private `/save` protocol:

1. `GET /spreadsheets/d/<ID>/edit` (browser User-Agent) → jar cookies
   `COMPASS`/`NID`; parse `"sid":"<hex>"` (server-assigned), `"revision":N`, and
   the tab list (from `docs-sheet-tab-caption">NAME` in the HTML).
2. `POST /spreadsheets/d/<ID>/save` — `multipart/form-data` with `rev=N` and
   `bundles=[{"commands":[...],"sid":"<sid>","reqId":0}]`; headers
   `x-same-domain:1` + browser UA + cookies. On stale-`rev` rejection, re-GET and retry.

`/bind` is **not** needed; the `sid` must be the server's (client-chosen → 400/550).

**Command opcodes** (constants in `gsheet.rs`, flagged build-tied to
`editors.spreadsheets-frontend_20260601`):
- set cell: `[21299578, "[[\"<gid>\",r,r+1,c,c+1],[132274236,3,[2,\"<value>\"],null,null,0],[null,[[null,513,[0],...,0]]]]"]`
- add sheet: `[4444216, [[21350203,"[1,0,\"<gid>\",[[[0,0,\"<name>\"],...]],1000,26]"],[28950036,"[[[[4,0,null,null,<idx>]]]]"]]]`

Tabs use fixed gids: `Meta`=990001, `Classes`=990002, `Instances`=990003 (Sheet1=0).

**When Google ships a new editor build and writes start failing**, re-capture the
opcodes with Chrome DevTools (this is how they were obtained):
1. Open an anyone-can-edit sheet in an incognito/logged-out browser.
2. Open DevTools → Network. Edit a cell (or add a sheet).
3. Find the `POST .../save` request; read its multipart body — the `bundles`
   field contains the new opcodes/command shape. Update the constants in `gsheet.rs`.

## Extending

- **New `Op`**: add the variant to `inv-server`'s `Op` enum (+ `apply` arm + error
  mapping), mirror it in `inv-app/src/api.rs`, add an `AppState` method + UI, and
  back it with an `InventoryExt` method (TDD in `inv-core` first).
- **New backend**: implement `Store` (Send+Sync, `transact_dyn`+photos), add a
  `StoreDescriptor` variant + `open()` arm, add a UI option in `app.rs`. Add a
  concurrency test proving no lost updates.
