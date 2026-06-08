# Inventory — bring your own database

A nested inventory manager (object **classes** + **instances**, where instances can
contain other instances) with photos, tags, relationships, duplication and search.

**No accounts, no sign-up, no app-minted identity, no UUIDs.** You point the app at a
database *you* own — and that store *is* the identity. Three backends:

1. **Local JSON file** — a human-editable document on the server's filesystem. ✅ working
2. **Postgres** — your own PostgreSQL instance, stored in **real relational tables + views**. ✅ working (live-verified)
3. **Google Sheet** — "Excel as a database". **Public link = anonymous read-write** (no sign-in — replicates the web editor's private `/save` protocol; flat single-sheet layout; verified live). **OAuth** = read-write with per-class tabs (needs a one-time Google client). ✅

**Multi-user = multiple app instances on the same store with no race conditions.**
Every mutation goes through `Store::transact` (load → apply → atomic commit → retry on
conflict). The File backend uses an OS advisory lock + atomic rename; this is proven by
a concurrency test running 16 threads *and a separate OS process* against one file with
zero lost updates.

Pure **Rust + WASM** end to end (the only non-Rust artifact is the wasm-bindgen JS glue).

## Architecture

```
crates/
  inv-model   data types (i64 ids, name-keyed classes), serde JSON  [pure]
  inv-core    InventoryExt domain logic: add/edit/move/duplicate/search,
              containment (proven acyclic), cascade delete, auto-class inference
  inv-store   Store trait + race-free transact(); native adapters: FileStore (JSON),
              PostgresStore (relational tables + views), GSheetStore (one tab per class)
  inv-server  axum gateway: opens a store by descriptor, runs ops via transact,
              serves the WASM SPA. Stateless, store-cache, spawn_blocking.
  inv-app     Leptos CSR WASM UI: "open database" screen + inventory UI
```

The browser can't speak raw Postgres/Sheets, so the WASM UI talks to the local Rust
gateway, which brokers to whichever `Store` you selected.

## Run it (File backend)

```bash
# 1. build the WASM UI
(cd crates/inv-app && trunk build --release)

# 2. run the gateway, serving the UI
INV_STATIC_DIR=crates/inv-app/dist cargo run -p inv-server   # -> http://127.0.0.1:8080

# 3. open http://127.0.0.1:8080, choose "Local file", enter a path
#    (e.g. /tmp/inventory.json), and Open.
```

Env vars: `INV_ADDR` (default `127.0.0.1:8080`), `INV_STATIC_DIR` (default `./dist`).

## Test

```bash
cargo test            # 53 backend tests incl. proptest invariants + concurrency proofs
cargo clippy          # clean
```

Built test-first (TDD) throughout; see `crates/*/tests` and the in-crate `proptest`
invariants (acyclic containment, deep-duplicate isomorphism, cascade edge cleanup,
encrypt/decrypt roundtrips, cross-process concurrency).

## Status

Working end-to-end on the **File** and **Postgres** backends — both driven in a real
browser (`docs/e2e-*.png`), with Postgres additionally proven by a live concurrency
test (8×10 transacts → no lost updates via `SELECT … FOR UPDATE`). The **Google Sheet**
adapter maps the inventory onto native tabs (one tab per class, plus `_classes`/
`_class_fields`/`_relationships`/`_meta`), with optimistic version-cell concurrency —
the mapping + retry logic are unit-tested via an injected fake transport. Access modes:
**public-link** reads with zero setup; **OAuth** read-write needs a one-time Google
OAuth client (server-side), then the gated live test runs with
`GSHEET_TEST_SPREADSHEET_ID` + `GSHEET_TEST_TOKEN`. (Programmatically creating a Google
account is impossible — CAPTCHA/SMS/ToS — so the write path needs that one credential.)
