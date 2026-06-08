//! [`PostgresStore`]: a [`Store`](crate::Store) backed by a PostgreSQL database
//! using a **native relational schema** — real tables, foreign keys, and views.
//!
//! ## Layout
//!
//! The [`Inventory`] is decomposed into normalized relational tables, NOT a single
//! JSON blob. This makes the data queryable with ordinary SQL and joins, while
//! remaining a **lossless** mirror of `Inventory` (`load ∘ commit == identity`).
//!
//! ```sql
//! meta(k text PRIMARY KEY, v text)                       -- holds 'next_id'
//! classes(name text PRIMARY KEY, created_at bigint NOT NULL)
//! class_fields(class text REFERENCES classes(name) ON DELETE CASCADE,
//!              name text, field_type text, required boolean, ord int,
//!              PRIMARY KEY(class, name))
//! instances(id bigint PRIMARY KEY, class text REFERENCES classes(name),
//!           name text, parent bigint REFERENCES instances(id) ON DELETE SET NULL,
//!           created_at bigint, updated_at bigint)
//! instance_fields(instance_id bigint REFERENCES instances(id) ON DELETE CASCADE,
//!                 name text, kind text, val_text text, val_num double precision,
//!                 val_bool boolean, val_date bigint,
//!                 PRIMARY KEY(instance_id, name))
//! instance_tags(instance_id bigint REFERENCES instances(id) ON DELETE CASCADE,
//!               tag text, PRIMARY KEY(instance_id, tag))
//! relationships(instance_id bigint REFERENCES instances(id) ON DELETE CASCADE,
//!               kind text, target bigint, ord int,
//!               PRIMARY KEY(instance_id, kind, target))
//! photos(key text PRIMARY KEY, instance_id bigint REFERENCES instances(id) ON DELETE CASCADE,
//!        mime text, name text, ord int, bytes bytea)
//! ```
//!
//! Two views expose the data for human / ad-hoc SQL querying:
//!
//! ```sql
//! v_instances(id, class, name, parent, parent_name, tag_list, created_at, updated_at)
//! v_instance_fields(instance_id, instance_name, field, value)
//! ```
//!
//! `class_fields.ord`, `relationships.ord`, and `photos.ord` preserve the original
//! `Vec` order of `Class::fields`, `Instance::relationships`, and
//! `Instance::photos` so reconstruction is byte-for-byte lossless even when those
//! vectors are not in any natural sort order. (`fields` and `tags` are
//! `BTreeMap`/`BTreeSet` in the model, so their canonical order is recovered by
//! `ORDER BY name/tag`.)
//!
//! ## Race-freedom
//!
//! [`transact_dyn`](PostgresStore::transact_dyn) runs the entire read-modify-write
//! inside one database transaction. The first statement takes a **transaction-level
//! advisory lock** (`pg_advisory_xact_lock`) on a single constant key, which
//! serializes all writers against each other:
//!
//! ```text
//! BEGIN
//!   SELECT pg_advisory_xact_lock(<WRITER_LOCK_KEY>)   -- serialize writers
//!   inv = reconstruct(tables)                         -- read INSIDE the lock
//!   f(&mut inv)
//!   sync(tables := inv)                               -- upsert present, delete absent
//!   meta.next_id := inv.next_id
//! COMMIT                                              -- advisory xact lock auto-releases
//! ```
//!
//! Because the advisory lock is held until `COMMIT`, any other transactor blocks at
//! its `pg_advisory_xact_lock` until we commit, then reads our committed state.
//! Critical sections are therefore strictly serialized, so there are **no lost
//! updates** across concurrent transactors — even from separate `PostgresStore`
//! handles / processes pointing at the same database.
//!
//! ## Connectivity / `Send + Sync`
//!
//! The blocking [`postgres::Client`] is `Send` but not `Sync`, so we store only the
//! immutable connection string (trivially `Send + Sync`) and open a short-lived
//! connection per call. This keeps the adapter dependency-light; the gateway
//! already serializes store work onto a blocking pool.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

use inv_model::{
    Class, FieldDef, FieldType, FieldValue, Instance, Inventory, Photo, Relationship,
};
use postgres::types::ToSql;
use postgres::{Client, GenericClient, NoTls, Transaction};

use crate::{Store, StoreError};

/// Advisory-lock key serializing concurrent schema creation (DDL on the system
/// catalogs can race). The exact value is irrelevant as long as it is stable.
const SCHEMA_LOCK_KEY: i64 = 0x696E_765F_7374_6F72; // ASCII "inv_stor"

/// Advisory-lock key (transaction scope) serializing all writers so the
/// read-modify-write critical section is strictly ordered — this is the
/// no-lost-updates guarantee. Distinct from the schema key.
const WRITER_LOCK_KEY: i64 = 0x696E_765F_7772_6974; // ASCII "inv_writ"

/// The `meta` key under which the inventory's `next_id` counter is persisted.
const META_NEXT_ID: &str = "next_id";

/// A [`Store`](crate::Store) backed by PostgreSQL with a relational schema.
///
/// Holds the connection URL (libpq keyword string or `postgres://` URL). Opens a
/// fresh connection per operation; see the module docs for the rationale.
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

/// Render a [`FieldType`] to its stored text discriminant.
fn field_type_to_str(t: FieldType) -> &'static str {
    match t {
        FieldType::Text => "text",
        FieldType::Number => "number",
        FieldType::Bool => "bool",
        FieldType::Date => "date",
    }
}

/// Parse a stored field-type discriminant back to a [`FieldType`].
fn field_type_from_str(s: &str) -> Result<FieldType, StoreError> {
    match s {
        "text" => Ok(FieldType::Text),
        "number" => Ok(FieldType::Number),
        "bool" => Ok(FieldType::Bool),
        "date" => Ok(FieldType::Date),
        other => Err(backend("field_type", format!("unknown field_type {other:?}"))),
    }
}

/// The text discriminant stored in `instance_fields.kind` for a [`FieldValue`].
fn field_value_kind(v: &FieldValue) -> &'static str {
    match v {
        FieldValue::Text(_) => "text",
        FieldValue::Number(_) => "number",
        FieldValue::Bool(_) => "bool",
        FieldValue::Date(_) => "date",
        FieldValue::Empty => "empty",
    }
}

impl PostgresStore {
    /// Open a store over the database at `url`.
    ///
    /// Connection is **lazy**: this only records the URL and never touches the
    /// network, so it cannot fail (matching the factory contract — an unreachable
    /// host surfaces as [`StoreError::Backend`] from the first real operation, not
    /// from `open`). The schema is created on demand on first use.
    pub fn open(url: &str) -> Result<Self, StoreError> {
        Ok(PostgresStore {
            url: url.to_string(),
            schema_ready: AtomicBool::new(false),
        })
    }

    /// Open a fresh blocking connection, ensuring the schema exists on first use.
    fn connect(&self) -> Result<Client, StoreError> {
        let mut client = Client::connect(&self.url, NoTls).map_err(|e| backend("connect", e))?;
        if !self.schema_ready.load(Ordering::Acquire) {
            Self::ensure_schema(&mut client)?;
            self.schema_ready.store(true, Ordering::Release);
        }
        Ok(client)
    }

    /// Create the relational tables and views if absent.
    ///
    /// Concurrent `CREATE TABLE IF NOT EXISTS` from multiple connections can race
    /// on the system catalogs, so the whole check-and-create is serialized behind a
    /// session-level advisory lock. Only one connection runs the DDL; the rest
    /// block and then find everything present.
    fn ensure_schema(client: &mut Client) -> Result<(), StoreError> {
        client
            .execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK_KEY])
            .map_err(|e| backend("schema lock", e))?;

        let res = (|| -> Result<(), StoreError> {
            client
                .batch_execute(
                    "CREATE TABLE IF NOT EXISTS meta (\
                         k text PRIMARY KEY, \
                         v text\
                     );\
                     CREATE TABLE IF NOT EXISTS classes (\
                         name       text   PRIMARY KEY, \
                         created_at bigint NOT NULL\
                     );\
                     CREATE TABLE IF NOT EXISTS class_fields (\
                         class      text    NOT NULL REFERENCES classes(name) ON DELETE CASCADE, \
                         name       text    NOT NULL, \
                         field_type text    NOT NULL, \
                         required   boolean NOT NULL, \
                         ord        int     NOT NULL, \
                         PRIMARY KEY (class, name)\
                     );\
                     CREATE TABLE IF NOT EXISTS instances (\
                         id         bigint PRIMARY KEY, \
                         class      text   NOT NULL REFERENCES classes(name), \
                         name       text   NOT NULL, \
                         parent     bigint REFERENCES instances(id) ON DELETE SET NULL, \
                         created_at bigint NOT NULL, \
                         updated_at bigint NOT NULL\
                     );\
                     CREATE TABLE IF NOT EXISTS instance_fields (\
                         instance_id bigint           NOT NULL REFERENCES instances(id) ON DELETE CASCADE, \
                         name        text             NOT NULL, \
                         kind        text             NOT NULL, \
                         val_text    text, \
                         val_num     double precision, \
                         val_bool    boolean, \
                         val_date    bigint, \
                         PRIMARY KEY (instance_id, name)\
                     );\
                     CREATE TABLE IF NOT EXISTS instance_tags (\
                         instance_id bigint NOT NULL REFERENCES instances(id) ON DELETE CASCADE, \
                         tag         text   NOT NULL, \
                         PRIMARY KEY (instance_id, tag)\
                     );\
                     CREATE TABLE IF NOT EXISTS relationships (\
                         instance_id bigint NOT NULL REFERENCES instances(id) ON DELETE CASCADE, \
                         kind        text   NOT NULL, \
                         target      bigint NOT NULL, \
                         ord         int    NOT NULL, \
                         PRIMARY KEY (instance_id, kind, target)\
                     );\
                     CREATE TABLE IF NOT EXISTS photos (\
                         key         text   PRIMARY KEY, \
                         instance_id bigint REFERENCES instances(id) ON DELETE CASCADE, \
                         mime        text   NOT NULL, \
                         name        text   NOT NULL, \
                         ord         int    NOT NULL DEFAULT 0, \
                         bytes       bytea  NOT NULL\
                     );\
                     CREATE OR REPLACE VIEW v_instances AS \
                         SELECT i.id, \
                                i.class, \
                                i.name, \
                                i.parent, \
                                p.name AS parent_name, \
                                COALESCE(\
                                    (SELECT string_agg(t.tag, ',' ORDER BY t.tag) \
                                       FROM instance_tags t WHERE t.instance_id = i.id), \
                                    '') AS tag_list, \
                                i.created_at, \
                                i.updated_at \
                           FROM instances i \
                           LEFT JOIN instances p ON p.id = i.parent;\
                     CREATE OR REPLACE VIEW v_instance_fields AS \
                         SELECT f.instance_id, \
                                i.name AS instance_name, \
                                f.name AS field, \
                                CASE f.kind \
                                    WHEN 'text'   THEN f.val_text \
                                    WHEN 'number' THEN f.val_num::text \
                                    WHEN 'bool'   THEN f.val_bool::text \
                                    WHEN 'date'   THEN f.val_date::text \
                                    ELSE NULL \
                                END AS value \
                           FROM instance_fields f \
                           JOIN instances i ON i.id = f.instance_id;",
                )
                .map_err(|e| backend("create schema", e))?;
            Ok(())
        })();

        // Always release the advisory lock, even if setup failed.
        let _ = client.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK_KEY]);
        res
    }

    /// Reconstruct a full [`Inventory`] from the relational tables using `client`
    /// (a `Client` or a `Transaction`). Restores byte-for-byte the inventory that
    /// was last `sync`ed.
    fn reconstruct<C: GenericClient>(client: &mut C) -> Result<Inventory, StoreError> {
        // --- classes + their fields ---------------------------------------
        let mut classes: BTreeMap<String, Class> = BTreeMap::new();
        for row in client
            .query("SELECT name, created_at FROM classes", &[])
            .map_err(|e| backend("load classes", e))?
        {
            let name: String = row.get(0);
            let created_at: i64 = row.get(1);
            classes.insert(
                name.clone(),
                Class {
                    name,
                    fields: Vec::new(),
                    created_at,
                },
            );
        }
        for row in client
            .query(
                "SELECT class, name, field_type, required FROM class_fields ORDER BY class, ord",
                &[],
            )
            .map_err(|e| backend("load class_fields", e))?
        {
            let class: String = row.get(0);
            let name: String = row.get(1);
            let field_type: String = row.get(2);
            let required: bool = row.get(3);
            if let Some(c) = classes.get_mut(&class) {
                c.fields.push(FieldDef {
                    name,
                    field_type: field_type_from_str(&field_type)?,
                    required,
                });
            }
        }

        // --- instances ----------------------------------------------------
        let mut instances: BTreeMap<i64, Instance> = BTreeMap::new();
        for row in client
            .query(
                "SELECT id, class, name, parent, created_at, updated_at FROM instances",
                &[],
            )
            .map_err(|e| backend("load instances", e))?
        {
            let id: i64 = row.get(0);
            let class: String = row.get(1);
            let name: String = row.get(2);
            let parent: Option<i64> = row.get(3);
            let created_at: i64 = row.get(4);
            let updated_at: i64 = row.get(5);
            instances.insert(
                id,
                Instance {
                    id,
                    class,
                    name,
                    fields: BTreeMap::new(),
                    tags: BTreeSet::new(),
                    parent,
                    photos: Vec::new(),
                    relationships: Vec::new(),
                    created_at,
                    updated_at,
                },
            );
        }

        // --- instance fields (BTreeMap order recovered by ORDER BY name) --
        for row in client
            .query(
                "SELECT instance_id, name, kind, val_text, val_num, val_bool, val_date \
                 FROM instance_fields ORDER BY instance_id, name",
                &[],
            )
            .map_err(|e| backend("load instance_fields", e))?
        {
            let instance_id: i64 = row.get(0);
            let name: String = row.get(1);
            let kind: String = row.get(2);
            let value = match kind.as_str() {
                "text" => {
                    let v: Option<String> = row.get(3);
                    FieldValue::Text(v.unwrap_or_default())
                }
                "number" => {
                    let v: Option<f64> = row.get(4);
                    FieldValue::Number(v.unwrap_or(0.0))
                }
                "bool" => {
                    let v: Option<bool> = row.get(5);
                    FieldValue::Bool(v.unwrap_or(false))
                }
                "date" => {
                    let v: Option<i64> = row.get(6);
                    FieldValue::Date(v.unwrap_or(0))
                }
                "empty" => FieldValue::Empty,
                other => return Err(backend("field kind", format!("unknown kind {other:?}"))),
            };
            if let Some(inst) = instances.get_mut(&instance_id) {
                inst.fields.insert(name, value);
            }
        }

        // --- tags (BTreeSet order recovered by ORDER BY tag) --------------
        for row in client
            .query(
                "SELECT instance_id, tag FROM instance_tags ORDER BY instance_id, tag",
                &[],
            )
            .map_err(|e| backend("load instance_tags", e))?
        {
            let instance_id: i64 = row.get(0);
            let tag: String = row.get(1);
            if let Some(inst) = instances.get_mut(&instance_id) {
                inst.tags.insert(tag);
            }
        }

        // --- relationships (Vec order recovered by ORDER BY ord) ----------
        for row in client
            .query(
                "SELECT instance_id, kind, target FROM relationships ORDER BY instance_id, ord",
                &[],
            )
            .map_err(|e| backend("load relationships", e))?
        {
            let instance_id: i64 = row.get(0);
            let kind: String = row.get(1);
            let target: i64 = row.get(2);
            if let Some(inst) = instances.get_mut(&instance_id) {
                inst.relationships.push(Relationship { kind, target });
            }
        }

        // --- photo metadata (Vec order recovered by ORDER BY ord) ---------
        // Only photos that belong to an instance reconstruct into the model; the
        // bytes themselves are fetched separately via get_photo.
        for row in client
            .query(
                "SELECT instance_id, key, mime, name FROM photos \
                 WHERE instance_id IS NOT NULL ORDER BY instance_id, ord, key",
                &[],
            )
            .map_err(|e| backend("load photos", e))?
        {
            let instance_id: i64 = row.get(0);
            let key: String = row.get(1);
            let mime: String = row.get(2);
            let name: String = row.get(3);
            if let Some(inst) = instances.get_mut(&instance_id) {
                inst.photos.push(Photo { key, mime, name });
            }
        }

        // --- next_id ------------------------------------------------------
        let next_id = Self::load_next_id(client, &instances)?;

        Ok(Inventory {
            classes,
            instances,
            next_id,
        })
    }

    /// Read `meta.next_id`, falling back to `max(instances.id) + 1` (or 1 when the
    /// store is empty) when the meta row is absent.
    fn load_next_id<C: GenericClient>(
        client: &mut C,
        instances: &BTreeMap<i64, Instance>,
    ) -> Result<i64, StoreError> {
        let row = client
            .query_opt("SELECT v FROM meta WHERE k = $1", &[&META_NEXT_ID])
            .map_err(|e| backend("load next_id", e))?;
        if let Some(row) = row {
            let v: String = row.get(0);
            return v
                .parse::<i64>()
                .map_err(|e| backend("parse next_id", e));
        }
        // No meta row: derive from the data.
        let max_id = instances.keys().copied().max().unwrap_or(0);
        Ok(max_id + 1)
    }

    /// Overwrite all relational tables so they exactly mirror `inv` (lossless).
    ///
    /// Strategy: delete every row, then re-insert from `inv`, respecting
    /// foreign-key order (parents/classes before children). A full rewrite is
    /// simple and correct; under the per-transaction advisory writer lock there is
    /// no concurrency to optimize against, and inventories are small.
    fn sync(tx: &mut Transaction, inv: &Inventory) -> Result<(), StoreError> {
        // Delete child tables first, then parents, to respect FK constraints.
        // (ON DELETE CASCADE would handle most, but we clear everything explicitly
        // so the end state is exactly `inv`.) Photos are preserved by key below.
        tx.batch_execute(
            "DELETE FROM relationships; \
             DELETE FROM instance_tags; \
             DELETE FROM instance_fields; \
             UPDATE photos SET instance_id = NULL; \
             DELETE FROM instances; \
             DELETE FROM class_fields; \
             DELETE FROM classes;",
        )
        .map_err(|e| backend("sync clear", e))?;

        // --- classes ------------------------------------------------------
        for class in inv.classes.values() {
            tx.execute(
                "INSERT INTO classes (name, created_at) VALUES ($1, $2)",
                &[&class.name, &class.created_at],
            )
            .map_err(|e| backend("sync class", e))?;
            for (ord, fd) in class.fields.iter().enumerate() {
                let ord = ord as i32;
                tx.execute(
                    "INSERT INTO class_fields (class, name, field_type, required, ord) \
                     VALUES ($1, $2, $3, $4, $5)",
                    &[
                        &class.name,
                        &fd.name,
                        &field_type_to_str(fd.field_type),
                        &fd.required,
                        &ord,
                    ],
                )
                .map_err(|e| backend("sync class_field", e))?;
            }
        }

        // --- instances (parents may reference other instances, but the FK is
        // ON DELETE SET NULL and we insert with the parent column directly; insert
        // all instances WITHOUT the parent FK risk by inserting in id order is not
        // enough since a parent can have a higher id. Defer the parent column: first
        // insert every instance with NULL parent, then set parents.) -----------
        for inst in inv.instances.values() {
            tx.execute(
                "INSERT INTO instances (id, class, name, parent, created_at, updated_at) \
                 VALUES ($1, $2, $3, NULL, $4, $5)",
                &[
                    &inst.id,
                    &inst.class,
                    &inst.name,
                    &inst.created_at,
                    &inst.updated_at,
                ],
            )
            .map_err(|e| backend("sync instance", e))?;
        }
        // Second pass: set parent pointers now that all rows exist.
        for inst in inv.instances.values() {
            if let Some(parent) = inst.parent {
                tx.execute(
                    "UPDATE instances SET parent = $1 WHERE id = $2",
                    &[&parent, &inst.id],
                )
                .map_err(|e| backend("sync parent", e))?;
            }
        }

        // --- instance fields ---------------------------------------------
        for inst in inv.instances.values() {
            for (name, value) in &inst.fields {
                let kind = field_value_kind(value);
                let val_text: Option<&str> = match value {
                    FieldValue::Text(s) => Some(s.as_str()),
                    _ => None,
                };
                let val_num: Option<f64> = match value {
                    FieldValue::Number(n) => Some(*n),
                    _ => None,
                };
                let val_bool: Option<bool> = match value {
                    FieldValue::Bool(b) => Some(*b),
                    _ => None,
                };
                let val_date: Option<i64> = match value {
                    FieldValue::Date(d) => Some(*d),
                    _ => None,
                };
                let params: [&(dyn ToSql + Sync); 7] = [
                    &inst.id, name, &kind, &val_text, &val_num, &val_bool, &val_date,
                ];
                tx.execute(
                    "INSERT INTO instance_fields \
                         (instance_id, name, kind, val_text, val_num, val_bool, val_date) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    &params,
                )
                .map_err(|e| backend("sync field", e))?;
            }

            // --- tags -----------------------------------------------------
            for tag in &inst.tags {
                tx.execute(
                    "INSERT INTO instance_tags (instance_id, tag) VALUES ($1, $2)",
                    &[&inst.id, tag],
                )
                .map_err(|e| backend("sync tag", e))?;
            }

            // --- relationships (preserve Vec order via ord) ---------------
            for (ord, rel) in inst.relationships.iter().enumerate() {
                let ord = ord as i32;
                tx.execute(
                    "INSERT INTO relationships (instance_id, kind, target, ord) \
                     VALUES ($1, $2, $3, $4)",
                    &[&inst.id, &rel.kind, &rel.target, &ord],
                )
                .map_err(|e| backend("sync relationship", e))?;
            }
        }

        // --- photo metadata: link photo rows to instances and set order. -----
        // Photo BYTES are managed independently via put_photo/get_photo; here we
        // only reconcile each photo's owning instance, mime/name, and ord so the
        // model's `Instance::photos` round-trips. A photo referenced by the model
        // but with no bytes row yet is created with empty bytes (bytes are filled
        // in later by put_photo). Photos NOT referenced by any instance keep their
        // bytes but are detached (instance_id stays NULL, set above).
        for inst in inv.instances.values() {
            for (ord, photo) in inst.photos.iter().enumerate() {
                let ord = ord as i32;
                let empty: &[u8] = &[];
                tx.execute(
                    "INSERT INTO photos (key, instance_id, mime, name, ord, bytes) \
                     VALUES ($1, $2, $3, $4, $5, $6) \
                     ON CONFLICT (key) DO UPDATE SET \
                         instance_id = EXCLUDED.instance_id, \
                         mime = EXCLUDED.mime, \
                         name = EXCLUDED.name, \
                         ord = EXCLUDED.ord",
                    &[
                        &photo.key,
                        &inst.id,
                        &photo.mime,
                        &photo.name,
                        &ord,
                        &empty,
                    ],
                )
                .map_err(|e| backend("sync photo meta", e))?;
            }
        }

        // --- persist next_id ---------------------------------------------
        let next_id_str = inv.next_id.to_string();
        tx.execute(
            "INSERT INTO meta (k, v) VALUES ($1, $2) \
             ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v",
            &[&META_NEXT_ID, &next_id_str],
        )
        .map_err(|e| backend("sync next_id", e))?;

        Ok(())
    }
}

impl Store for PostgresStore {
    fn load(&self) -> Result<Inventory, StoreError> {
        let mut client = self.connect()?;
        Self::reconstruct(&mut client)
    }

    fn transact_dyn(
        &self,
        f: &mut dyn FnMut(&mut Inventory) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut client = self.connect()?;
        // One database transaction for the whole read-modify-write. Dropping the
        // `Transaction` without `commit()` rolls back, so any early return (incl.
        // from `f`) cleanly aborts.
        let mut tx = client.transaction().map_err(|e| backend("begin", e))?;

        // Serialize all writers: this transaction-scoped advisory lock blocks any
        // other writer until we COMMIT (auto-release). No lost updates.
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&WRITER_LOCK_KEY])
            .map_err(|e| backend("writer lock", e))?;

        // Reconstruct the current state INSIDE the lock, so the read-modify-write
        // observes the latest committed data.
        let mut inv = Self::reconstruct(&mut tx)?;

        // Run the caller's mutation. On `Err` we return early; the still-open `tx`
        // is dropped, rolling the transaction back (no partial writes).
        f(&mut inv)?;

        // Sync the tables to exactly mirror the mutated inventory, then commit.
        Self::sync(&mut tx, &inv)?;
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
        // Insert/overwrite the bytes. If the photo row already exists (created by a
        // sync to carry metadata), keep its instance_id/mime/name/ord and just set
        // the bytes. If it does not exist yet, create a detached row (instance_id
        // NULL, blank mime/name) holding the bytes — a later sync attaches it.
        client
            .execute(
                "INSERT INTO photos (key, instance_id, mime, name, ord, bytes) \
                 VALUES ($1, NULL, '', '', 0, $2) \
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

    /// Mapping unit checks that don't need a live server.
    #[test]
    fn field_type_str_roundtrips() {
        for t in [
            FieldType::Text,
            FieldType::Number,
            FieldType::Bool,
            FieldType::Date,
        ] {
            assert_eq!(field_type_from_str(field_type_to_str(t)).unwrap(), t);
        }
    }

    #[test]
    fn field_type_from_str_rejects_garbage() {
        match field_type_from_str("bogus") {
            Err(StoreError::Backend(m)) => assert!(m.contains("postgres field_type")),
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[test]
    fn field_value_kind_discriminants() {
        assert_eq!(field_value_kind(&FieldValue::Text("x".into())), "text");
        assert_eq!(field_value_kind(&FieldValue::Number(1.0)), "number");
        assert_eq!(field_value_kind(&FieldValue::Bool(true)), "bool");
        assert_eq!(field_value_kind(&FieldValue::Date(1)), "date");
        assert_eq!(field_value_kind(&FieldValue::Empty), "empty");
    }
}
