# CONVERSATION.md — how this project got here

A narrative handoff: the original brief, the course-corrections, and the reasoning
behind the current design. Pairs with `AGENTS.md` (how) and `PLAN.md` (what's next).
The whole thing was built test-first, with work fanned out to background agents.

## Original brief

"A GUI inventory app: define **object classes** and **instances** of those classes,
where each instance can contain other instances. Easily add things with photos,
auto-generated ids, tagging, placing inside containers, naming, auto-generating
classes, making relationships/duplicates, plus search and edit. Use TDD to prove
the UI and DB operations work. Written in **Rust/WASM only** (plus wasm-bindgen
glue)." Initial framing also mentioned "no registration… even encryption."

## Pivot 1 — "no registration" reinterpreted: bring your own database

The first build went the wrong way: it read "no registration" as *auto-provision an
identity for the user* and built a capability-URL / zero-knowledge design
(auto-minted workspace UUID + AES-GCM key in the URL fragment + a dumb encrypted
`redb` blob server). **The user corrected this:** "no registration / no UUIDs"
means the **opposite** — the user explicitly supplies a database they own, and the
app is a front-end over it. Three backends, all user-chosen:
1. a local **JSON file**, 2. a **Google Sheet** ("everyone loves Excel as a DB"),
3. a **Postgres** instance. Multi-user = multiple app instances on the same store
with **no race conditions**.

Consequences:
- Dropped: capability-URLs, app-minted workspace UUID, client-side encryption, the
  zero-knowledge blob server. `inv-crypto` + the old `inv-server` became dead code.
- **No UUIDs anywhere** → object identity became store-native `i64`, classes keyed
  by name (the "auto-generated uuids" from the brief became auto-`i64` ids).
- Salvaged: the domain logic (`inv-core`) and most types (`inv-model`), which are
  storage-agnostic.
- New architecture: a `Store` trait with `transact` (load→apply→atomic-commit→retry)
  as the no-race-conditions guarantee, an axum gateway brokering between the WASM UI
  and the chosen store (a browser can't speak raw Postgres/Sheets), and three adapters.
- Encryption was dropped from the main flow (a file/sheet must stay human-readable).

The File backend was built + browser-verified first (fastest path to a working
GUI), then Postgres (live-verified incl. a concurrency proof), with Sheets stubbed.

## Pivot 2 — use each backend's NATIVE structure, not a JSON blob

The first adapter pass stored the *entire* `Inventory` as a single JSON blob (one
`jsonb` row in Postgres / one cell in Sheets). **The user correctly rejected this:**
it throws away the whole point of SQL/spreadsheets. Reworked to native structure:
- **Postgres** → real relational tables (`classes`, `class_fields`, `instances`
  with a `parent` FK, `instance_fields`, `instance_tags`, `relationships`,
  `photos`) + convenience views (`v_instances`, `v_instance_fields`). SQL-queryable.
- **Sheets** → tabs/rows/columns (OAuth path: one tab per class). Verified that the
  data lands in real tables, not a blob.

## The Google Sheets saga (the hard part)

The user wanted to use a Sheet as a real read-**write** DB with **no OAuth token**.
This took several rounds and one genuinely-wrong claim from the assistant:
- First framing: "a public link can only be **read**; writes need OAuth, because
  the Sheets **API** has no anonymous write." Partly right, partly wrong.
- The user pushed back: "shared edit links are editable **anonymously** — no login."
  **They were right.** Verified: an "anyone-with-link-can-edit" sheet *is* editable
  by a logged-out person (Google assigns "Anonymous Animal" identities). The
  assistant's "must be logged in" was wrong. The nuance: that anonymous editing
  happens through Google's **private web-editor endpoints**, while the public
  **Sheets API v4** returns 403 for anonymous writes. So a *browser* can write
  anonymously; a normal API client cannot.
- Also clarified an impossibility: an app **cannot create a Google account**
  programmatically (CAPTCHA + SMS + ToS). So "app makes you a fresh account" isn't
  achievable. The user chose: support **OAuth + public-URL** modes (and "leave the
  service-account code in, don't waste tokens stripping it").
- The user then directed: *"learn how the gdrive webapp edits the doc and emulate
  those events."* So the assistant **reverse-engineered the web editor's private
  `/save` protocol** via Chrome DevTools against the user's live test sheet:
  captured the `set-cell` and `add-sheet` command bundles + the GET `/edit` →
  `/save` handshake (server-assigned `sid`, no `/bind` needed), and implemented it
  in Rust with `ureq` + a cookie jar. **Anonymous read-write works, verified live.**
- A bug surfaced and was fixed along the way: `reqwest::blocking` panics when used
  inside the gateway's tokio runtime → switched to `ureq` (runtime-free).
- Final round of fixes: classes weren't deletable and instances couldn't change
  class (added `delete_class`/`change_class` end-to-end), and classes+instances
  shared one Sheet tab → split into separate `Meta`/`Classes`/`Instances` tabs
  (required capturing the `add-sheet` opcode too).

See `AGENTS.md` → "Reverse-engineered anonymous Google Sheets protocol" for the
exact opcodes and the **re-capture procedure** (they're tied to the Sheets editor
build and will eventually break).

## Working style notes

- Built **test-first** throughout; `inv-core` carries `proptest` invariants.
- Work was done by **background agents/workflows** (the user explicitly wanted the
  main thread kept free and disliked long inline work). Contract-first each time:
  freeze the shared types, then fan out.
- The user is decisive, token-conscious, and steers by correction — prefer stating
  assumptions and proceeding over asking, but surface genuine impossibilities
  honestly (the Google-account-creation limit was a real one).

## Commit timeline

```
5289c91 Scaffold workspace + verified inv-model contract (TDD green)
714213c inv-core + inv-crypto + inv-server: TDD-complete, 52 tests green   # capability-URL era (pre-pivot)
56a49ef Pivot to bring-your-own-database paradigm: no UUIDs, store-backed   # PIVOT 1
69204d8 Storage gateway (axum over Store factory) + Leptos WASM UI on File backend
a84a7d9 E2E verified in real browser (File backend) + README
4d1ea4c PostgresStore (live-verified) + GSheetStore (mapping tested, live gated)
7bbebb9 Capstone: Postgres backend verified through the GUI
e701f5b Native-structure adapters: relational Postgres + per-class Sheet tabs   # PIVOT 2
be77f21 Verify gateway->Postgres writes to native relational tables
0fd24f1 Fix GSheet gateway panic: reqwest::blocking -> ureq (runtime-free)
d8eb2f3 Anonymous Google Sheets read-WRITE via web-editor /save protocol (no OAuth)   # the saga
b39ebfa Class delete/reclass + anonymous Sheets split into separate tabs
```
