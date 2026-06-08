# PLAN.md — status & roadmap

Status as of commit `b39ebfa`. See `AGENTS.md` for architecture/commands,
`CONVERSATION.md` for history. **118 backend tests green, clippy clean.**

## Done ✅

**Core platform**
- Workspace of 5 crates (`inv-model`, `inv-core`, `inv-store`, `inv-server`,
  `inv-app`); `inv-crypto` is obsolete and idle.
- Data model: no-UUID, `i64` ids, classes keyed by name; human-editable pretty JSON.
- Domain logic (`InventoryExt`) with proptest-proven invariants: containment is
  always acyclic, `move` rejects cycles with no mutation, deep-duplicate is
  structurally isomorphic with fresh ids, cascade-delete leaves no dangling edges,
  class field inference, idempotent `ensure_class`.
- Full CRUD: add/edit/delete instances, containment (move, reparent/cascade),
  tags, relationships, photos, duplicate (shallow/deep), search, **change class**,
  **delete class** (guarded by `ClassInUse`).
- `Store` trait with race-free `transact` (load→apply→atomic-commit→retry).
- axum gateway (stateless-ish, store cache, `spawn_blocking`, CoreError→HTTP).
- Leptos WASM GUI: open-database screen (3 backends), add form (auto-class),
  navigator (containment + breadcrumb + move + delete), search, detail panel
  (edit name/fields/tags/photos/relationships, duplicate, **editable class**),
  deletable classes list.

**Backends**
- **File (JSON)** — ✅ working, browser-E2E'd; cross-process concurrency proof.
- **Postgres** — ✅ working, browser-E2E'd; real relational schema + views; live
  `pg_advisory_xact_lock` concurrency proof (8 threads × 10 ops → no lost updates).
- **Google Sheet — public link (anonymous)** — ✅ working, **verified live**;
  no OAuth; reverse-engineered web-editor `/save` protocol; separate
  `Meta`/`Classes`/`Instances` tabs.
- **Google Sheet — OAuth / AppHosted** — ⚠️ implemented (Sheets API v4, per-class
  tabs, optimistic concurrency) but **not verified** (no Google credentials here).

**Verification artifacts**: `docs/e2e-*.png` (browser walkthroughs: File + Postgres);
live Sheets round-trip via `tests/gsheet_anon_live.rs`.

## Known limitations / TODO (roughly prioritized)

1. **Google OAuth path is unverified + uses a static token.** `OAuth`/`AppHosted`
   read creds from server env (`INV_GSHEET_TOKEN`/`INV_GSHEET_OAUTH_CLIENT`/
   `INV_GSHEET_SERVICE_ACCOUNT`) and have a gated `#[ignore]` live test. The token
   path expects a ready access token (expires hourly). **Next:** implement a proper
   OAuth installed-app/loopback flow with refresh tokens, and a one-time setup doc.
   To verify: create a Google Cloud OAuth client, set the env, run the gated test.
2. **Sheets anonymous is brittle by nature.** Opcodes are tied to the Sheets editor
   build (`editors.spreadsheets-frontend_20260601`); a Google update can break
   writes. Re-capture procedure is in `AGENTS.md`. Also requires a browser
   User-Agent (Google throttles non-browser agents) and the sheet be
   "anyone-with-link-can-edit".
3. **Photos unsupported on the Sheets backend** (cell size limits). Supported on
   File (sidecar) and Postgres (`photos` bytea table).
4. **No live refresh / true CAS.** The UI reads on open + after each op, not on a
   timer — external edits to the store don't auto-appear (reopen to refresh).
   Concurrency is optimistic last-write-wins-with-retry, not field-level merge.
5. **Sheets layouts differ per mode** (anonymous = `Meta`/`Classes`/`Instances`
   tabs; OAuth = per-class tabs). A sheet written by one mode isn't readable by the
   other. Consider unifying, or per-class tabs for the anonymous path too (needs
   `delete-sheet`/`move-row` opcodes — more capture + brittleness).
6. **Remove `inv-crypto`** — dead code from the scrapped capability-URL design.
7. **Test-sheet id is hardcoded** in `gsheet_anon_live.rs`/examples — parameterize
   via env for the next dev's sheet.
8. **macOS Postgres test ergonomics** — ephemeral clusters leak shm on killed runs
   (see AGENTS.md gotcha). Consider an env opt-in to reuse an existing DB instead.

## Suggested next steps for the new owner

- Pick verification target: if you have a Google Cloud project, finish + verify the
  OAuth flow (item 1) — that's the most "production" Sheets path. If not, the
  anonymous path already works for personal use.
- Decide whether Sheets brittleness is acceptable; if not, lean on File/Postgres
  (both rock-solid) and keep Sheets as best-effort.
- `cargo test` + a browser smoke test (open File store, add nested items) before
  any change; keep TDD.
