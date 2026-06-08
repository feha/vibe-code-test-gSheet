//! [`PostgresStore`]: a [`Store`](crate::Store) backed by a PostgreSQL database.
//!
//! ## Layout
//!
//! The whole inventory lives in a single `jsonb` document in one row, mirroring
//! the [`FileStore`](crate::FileStore) single-document approach so the same
//! `inv-model`/`inv-core` logic drives both backends:
//!
//! ```sql
//! CREATE TABLE inventory (id int PRIMARY KEY, doc jsonb NOT NULL, version bigint NOT NULL);
//! CREATE TABLE photos    (key text PRIMARY KEY, bytes bytea NOT NULL);
//! ```
//!
//! The inventory is the single row `id = 1`. `version` is bumped on every commit
//! (useful for diagnostics / future optimistic paths; correctness here comes from
//! row-locking, not the version).
//!
//! ## Race-freedom
//!
//! [`transact_dyn`](PostgresStore::transact_dyn) runs the entire read-modify-write
//! inside one database transaction and takes a **`SELECT ... FOR UPDATE` row lock**
//! on the `id = 1` inventory row:
//!
//! ```text
//! BEGIN
//!   SELECT doc FROM inventory WHERE id = 1 FOR UPDATE   -- exclusive row lock
//!   inv = parse(doc)  (or Inventory::new() if no row yet)
//!   f(&mut inv)
//!   UPSERT doc = json(inv), version = version + 1
//! COMMIT
//! ```
//!
//! Because the row lock is held until `COMMIT`, any other transactor that reaches
//! its `FOR UPDATE` blocks until we commit and then observes our committed state.
//! Critical sections are therefore strictly serialized over the single row, so
//! there are **no lost updates** across concurrent transactors — even from
//! separate `PostgresStore` handles / processes pointing at the same database.
//! (When there is no row yet, the first writer's `INSERT` is itself serialized by
//! the primary-key; a loser retries and then takes the normal `FOR UPDATE` path.)
//!
//! ## Connectivity / `Send + Sync`
//!
//! The blocking [`postgres::Client`] is `Send` but not `Sync`, so it cannot be
//! held directly in a `Send + Sync` struct. We instead store the immutable
//! connection string and open a short-lived [`postgres::Client`] per call — the
//! struct holds only a `String`, which is trivially `Send + Sync`. (`r2d2`
//! pooling is an optional dependency and not enabled in this build; per-call
//! connect keeps the adapter dependency-light and is plenty for the gateway,
//! which already serializes store work onto a blocking pool.)

use std::sync::atomic::{AtomicBool, Ordering};

use inv_model::Inventory;
use postgres::{Client, NoTls};

use crate::{Store, StoreError};

/// Arbitrary constant key for the session advisory lock that serializes schema
/// creation across concurrent connections (see [`PostgresStore::ensure_schema`]).
/// The exact value is irrelevant as long as it is stable for this adapter.
const SCHEMA_LOCK_KEY: i64 = 0x696E_765F_7374_6F72; // ASCII "inv_stor"

/// A [`Store`](crate::Store) backed by PostgreSQL.
///
/// Holds the connection URL (libpq keyword string or `postgres://` URL). Opens a
/// fresh connection per operation; see the module docs for the race-freedom and
/// `Send + Sync` rationale.
pub struct PostgresStore {
    /// The PostgreSQL connection string (e.g. `postgres://user:pass@host/db` or
    /// `host=/tmp/sock port=5432 user=postgres dbname=inv`).
    url: String,
    /// Set once the schema has been ensured on this handle, so we skip the
    /// advisory-locked `CREATE TABLE IF NOT EXISTS` round-trip on later ops.
    schema_ready: AtomicBool,
}

/// Build a `StoreError::Backend` from any displayable error, tagged so callers
/// (and the routing test) can see it came from this adapter.
fn backend<E: std::fmt::Display>(ctx: &str, e: E) -> StoreError {
    StoreError::Backend(format!("postgres {ctx}: {e}"))
}

impl PostgresStore {
    /// Open a store over the database at `url`.
    ///
    /// Connection is **lazy**: this only records the URL and never touches the
    /// network, so it cannot fail (matching the factory contract — an
    /// unreachable host surfaces as a [`StoreError::Backend`] from the first real
    /// operation, not from `open`). The schema is created on demand by
    /// [`connect`](PostgresStore::connect) using `CREATE TABLE IF NOT EXISTS`.
    pub fn open(url: &str) -> Result<Self, StoreError> {
        Ok(PostgresStore {
            url: url.to_string(),
            schema_ready: AtomicBool::new(false),
        })
    }

    /// Open a fresh blocking connection to the configured database, ensuring the
    /// schema exists on first use. Every operation connects through here.
    fn connect(&self) -> Result<Client, StoreError> {
        let mut client = Client::connect(&self.url, NoTls).map_err(|e| backend("connect", e))?;
        if !self.schema_ready.load(Ordering::Acquire) {
            Self::ensure_schema(&mut client)?;
            self.schema_ready.store(true, Ordering::Release);
        }
        Ok(client)
    }

    /// Create the tables (if absent) and seed the singleton inventory row.
    ///
    /// Concurrent `CREATE TABLE IF NOT EXISTS` from multiple connections can race
    /// on the system catalogs (duplicate-key / "tuple concurrently updated"
    /// errors), so we serialize the whole check-and-create behind a session-level
    /// **advisory lock**. Only one connection at a time runs the DDL; the rest
    /// block on `pg_advisory_lock` and then find everything already present.
    ///
    /// Crucially we also **seed the `id = 1` row** with an empty inventory. That
    /// guarantees the row always exists, so [`transact_dyn`](Self::transact_dyn)'s
    /// `SELECT ... FOR UPDATE` always has a row to lock. Without the seed, the
    /// very first concurrent writers would each see "no row" (a `FOR UPDATE` over
    /// zero rows locks nothing), all start from `Inventory::new()`, and clobber
    /// each other on the initial `INSERT` — a lost-update window. The seed closes
    /// it: from the first transaction onward, contention is serialized by the row
    /// lock.
    fn ensure_schema(client: &mut Client) -> Result<(), StoreError> {
        client
            .execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK_KEY])
            .map_err(|e| backend("schema lock", e))?;

        let res = (|| -> Result<(), StoreError> {
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS inventory (\
                         id      int    PRIMARY KEY, \
                         doc     jsonb  NOT NULL, \
                         version bigint NOT NULL\
                     );\
                     CREATE TABLE IF NOT EXISTS photos (\
                         key   text  PRIMARY KEY, \
                         bytes bytea NOT NULL\
                     );",
                )
                .map_err(|e| backend("create schema", e))?;

            let empty = Self::doc_text(&Inventory::new())?;
            client
                .execute(
                    "INSERT INTO inventory (id, doc, version) VALUES (1, $1::text::jsonb, 0) \
                     ON CONFLICT (id) DO NOTHING",
                    &[&empty],
                )
                .map_err(|e| backend("seed row", e))?;
            Ok(())
        })();

        // Always release the advisory lock, even if setup failed.
        let _ = client.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK_KEY]);
        res
    }

    /// Parse a `jsonb` document (read back as text) into an [`Inventory`].
    fn parse_doc(text: &str) -> Result<Inventory, StoreError> {
        Inventory::from_json_bytes(text.as_bytes()).map_err(|e| backend("parse doc", e))
    }

    /// Serialize an [`Inventory`] to the JSON text we store in the `jsonb` column.
    fn doc_text(inv: &Inventory) -> Result<String, StoreError> {
        let bytes = inv.to_json_bytes().map_err(|e| backend("serialize doc", e))?;
        String::from_utf8(bytes).map_err(|e| backend("serialize doc utf8", e))
    }
}

impl Store for PostgresStore {
    fn load(&self) -> Result<Inventory, StoreError> {
        let mut client = self.connect()?;
        // Read the document as text and parse it; absent row => empty inventory.
        let row = client
            .query_opt("SELECT doc::text FROM inventory WHERE id = 1", &[])
            .map_err(|e| backend("load select", e))?;
        match row {
            None => Ok(Inventory::new()),
            Some(row) => {
                let text: String = row.get(0);
                Self::parse_doc(&text)
            }
        }
    }

    fn transact_dyn(
        &self,
        f: &mut dyn FnMut(&mut Inventory) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut client = self.connect()?;
        // One database transaction for the whole read-modify-write. Dropping the
        // `Transaction` without `commit()` rolls back, so any early return below
        // (including from `f`) cleanly aborts.
        let mut tx = client.transaction().map_err(|e| backend("begin", e))?;

        // Lock the single inventory row (if present) for the duration of the
        // transaction. Concurrent transactors block here until we commit => no
        // lost updates.
        let row = tx
            .query_opt("SELECT doc::text FROM inventory WHERE id = 1 FOR UPDATE", &[])
            .map_err(|e| backend("select for update", e))?;
        let mut inv = match &row {
            None => Inventory::new(),
            Some(row) => {
                let text: String = row.get(0);
                Self::parse_doc(&text)?
            }
        };

        // Run the caller's mutation. On `Err` we return early; the still-open
        // `tx` is dropped, which rolls the transaction back (no partial writes).
        f(&mut inv)?;

        // Commit the mutated document, bumping the version. UPSERT so the first
        // ever write inserts the row and later writes update it in place.
        let text = Self::doc_text(&inv)?;
        // `$1::text::jsonb` forces the bound parameter's type to `text` (the cast
        // *source*) so the driver sends our JSON string as text and the server
        // casts it to `jsonb`. Writing `$1::jsonb` would instead infer the param
        // as `jsonb`, which the blocking `postgres` crate cannot serialize from a
        // Rust `String` (the `with-serde_json` type feature is not enabled).
        tx.execute(
            "INSERT INTO inventory (id, doc, version) VALUES (1, $1::text::jsonb, 1) \
             ON CONFLICT (id) DO UPDATE \
                 SET doc = EXCLUDED.doc, version = inventory.version + 1",
            &[&text],
        )
        .map_err(|e| backend("upsert", e))?;

        tx.commit().map_err(|e| backend("commit", e))
    }

    fn get_photo(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let mut client = self.connect()?;
        let row = client
            .query_opt("SELECT bytes FROM photos WHERE key = $1", &[&key])
            .map_err(|e| backend("get_photo select", e))?;
        Ok(row.map(|row| {
            let bytes: Vec<u8> = row.get(0);
            bytes
        }))
    }

    fn put_photo(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError> {
        let mut client = self.connect()?;
        client
            .execute(
                "INSERT INTO photos (key, bytes) VALUES ($1, $2) \
                 ON CONFLICT (key) DO UPDATE SET bytes = EXCLUDED.bytes",
                &[&key, &bytes],
            )
            .map_err(|e| backend("put_photo", e))?;
        Ok(())
    }

    fn delete_photo(&self, key: &str) -> Result<(), StoreError> {
        let mut client = self.connect()?;
        // Deleting a missing key affects zero rows and is a no-op, as required.
        client
            .execute("DELETE FROM photos WHERE key = $1", &[&key])
            .map_err(|e| backend("delete_photo", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The struct must be `Send + Sync` so `Box<dyn Store>` is shareable.
    #[test]
    fn store_is_send_sync() {
        fn require<T: Send + Sync>() {}
        require::<PostgresStore>();
    }

    /// SQL-text / mapping unit checks that don't need a live server: the doc
    /// round-trips through the exact text<->Inventory conversion the adapter uses.
    #[test]
    fn doc_text_roundtrips_inventory() {
        use inv_core::InventoryExt;
        use std::collections::BTreeMap;

        let mut inv = Inventory::new();
        inv.add_instance("Item", "thing", BTreeMap::new(), None, 1)
            .unwrap();

        let text = PostgresStore::doc_text(&inv).expect("serialize");
        let back = PostgresStore::parse_doc(&text).expect("parse");
        assert_eq!(inv, back);
    }

    #[test]
    fn parse_doc_rejects_garbage() {
        let err = PostgresStore::parse_doc("not json").unwrap_err();
        match err {
            StoreError::Backend(m) => assert!(m.contains("postgres parse doc")),
            other => panic!("expected Backend, got {other:?}"),
        }
    }
}
