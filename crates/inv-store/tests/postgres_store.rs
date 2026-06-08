//! Live integration tests for [`inv_store::PostgresStore`] against a **real**,
//! ephemeral PostgreSQL cluster.
//!
//! Each run of this test binary spins up its own throwaway cluster with `initdb`
//! into a tempdir, starts it with `pg_ctl` listening only on a private unix
//! socket (no TCP, `--auth=trust`, no network exposure), runs every test against
//! it, and tears it down with `pg_ctl stop` on `Drop`. This makes the tests
//! hermetic: they do not depend on an already-running server, credentials, or a
//! shared database, and they cannot collide with anything else on the machine.
//!
//! If the cluster cannot be started (e.g. postgres tooling is missing in this
//! environment), the live tests are skipped with a clear message rather than
//! failing — but on this machine postgres 14 is installed and they run for real,
//! including the concurrency proof.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;

use inv_core::InventoryExt;
use inv_store::{Store, StoreError, StoreExt};

// ---------------------------------------------------------------------------
// Ephemeral cluster harness
// ---------------------------------------------------------------------------

/// Locate a postgres binary by name under the common Homebrew prefix or `PATH`.
fn find_bin(name: &str) -> Option<PathBuf> {
    let candidates = [
        format!("/opt/homebrew/bin/{name}"),
        format!("/usr/local/bin/{name}"),
        format!("/usr/lib/postgresql/14/bin/{name}"),
    ];
    for c in candidates {
        let p = PathBuf::from(&c);
        if p.exists() {
            return Some(p);
        }
    }
    // Fall back to PATH lookup via `which`.
    let out = Command::new("which").arg(name).output().ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(PathBuf::from(s));
        }
    }
    None
}

/// A running throwaway PostgreSQL cluster. Stops + removes itself on `Drop`.
struct EphemeralCluster {
    pg_ctl: PathBuf,
    data_dir: PathBuf,
    /// The socket directory passed to `-k`.
    socket_dir: PathBuf,
    /// The "port" — names the socket file `.s.PGSQL.<port>` for a socket-only
    /// server; chosen unique-per-binary so concurrent test binaries don't clash.
    port: u16,
    _tmp: tempfile::TempDir,
}

impl EphemeralCluster {
    /// Try to bring up a cluster; `Ok(None)` (with a printed reason) when the
    /// environment lacks postgres tooling, so callers can skip live tests.
    fn try_start() -> Result<Option<EphemeralCluster>, String> {
        let initdb = match find_bin("initdb") {
            Some(p) => p,
            None => {
                eprintln!("SKIP: `initdb` not found; skipping live postgres tests");
                return Ok(None);
            }
        };
        let pg_ctl = match find_bin("pg_ctl") {
            Some(p) => p,
            None => {
                eprintln!("SKIP: `pg_ctl` not found; skipping live postgres tests");
                return Ok(None);
            }
        };

        let tmp = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
        let data_dir = tmp.path().join("data");
        let socket_dir = tmp.path().join("sock");
        std::fs::create_dir_all(&socket_dir).map_err(|e| format!("mkdir sock: {e}"))?;

        // initdb with trust auth so no password is needed over the socket.
        let out = Command::new(&initdb)
            .arg("-D")
            .arg(&data_dir)
            .arg("-U")
            .arg("postgres")
            .arg("--auth=trust")
            .arg("-E")
            .arg("UTF8")
            .output()
            .map_err(|e| format!("spawn initdb: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "initdb failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }

        // A unique-ish port number names the socket file; pick from the pid so two
        // test binaries running at once don't share a socket path.
        let port: u16 = 49152 + (std::process::id() % 16000) as u16;

        let opts = format!(
            "-k {} -p {} -c listen_addresses=''",
            socket_dir.display(),
            port
        );
        let out = Command::new(&pg_ctl)
            .arg("-D")
            .arg(&data_dir)
            .arg("-o")
            .arg(&opts)
            .arg("-l")
            .arg(tmp.path().join("server.log"))
            .arg("-w")
            .arg("start")
            .output()
            .map_err(|e| format!("spawn pg_ctl start: {e}"))?;
        if !out.status.success() {
            let log = std::fs::read_to_string(tmp.path().join("server.log")).unwrap_or_default();
            return Err(format!(
                "pg_ctl start failed: {}\n--- server log ---\n{}",
                String::from_utf8_lossy(&out.stderr),
                log
            ));
        }

        Ok(Some(EphemeralCluster {
            pg_ctl,
            data_dir,
            socket_dir,
            port,
            _tmp: tmp,
        }))
    }

    /// libpq connection string for `dbname`, over this cluster's unix socket.
    fn conn_url(&self, dbname: &str) -> String {
        format!(
            "host={} port={} user=postgres dbname={}",
            self.socket_dir.display(),
            self.port,
            dbname
        )
    }

    /// Create a fresh database with a unique name and return its connection url.
    fn fresh_db(&self) -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let name = format!("inv_test_{}_{}", std::process::id(), n);
        let createdb = find_bin("createdb").expect("createdb present alongside initdb");
        let out = Command::new(&createdb)
            .arg("-h")
            .arg(&self.socket_dir)
            .arg("-p")
            .arg(self.port.to_string())
            .arg("-U")
            .arg("postgres")
            .arg(&name)
            .output()
            .expect("spawn createdb");
        assert!(
            out.status.success(),
            "createdb failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.conn_url(&name)
    }
}

impl Drop for EphemeralCluster {
    fn drop(&mut self) {
        let _ = Command::new(&self.pg_ctl)
            .arg("-D")
            .arg(&self.data_dir)
            .arg("-m")
            .arg("immediate")
            .arg("-w")
            .arg("stop")
            .output();
        // socket_dir lives inside the TempDir, removed when `_tmp` drops.
        let _ = &self.socket_dir;
    }
}

/// Process-wide singleton cluster shared by all tests in this binary, so we pay
/// the initdb/start cost once. `None` means the environment couldn't start one.
fn cluster() -> Option<Arc<EphemeralCluster>> {
    static INIT: Once = Once::new();
    static mut SLOT: Option<Arc<EphemeralCluster>> = None;
    static FAIL: Mutex<Option<String>> = Mutex::new(None);

    INIT.call_once(|| match EphemeralCluster::try_start() {
        Ok(Some(c)) => unsafe {
            SLOT = Some(Arc::new(c));
        },
        Ok(None) => {}
        Err(e) => {
            *FAIL.lock().unwrap() = Some(e);
        }
    });

    if let Some(e) = FAIL.lock().unwrap().as_ref() {
        panic!("could not start ephemeral postgres cluster: {e}");
    }
    // SAFETY: set exactly once inside call_once before any read; read-only after.
    #[allow(static_mut_refs)]
    unsafe {
        SLOT.clone()
    }
}

/// Run `body` with a freshly-created database url, or skip (return) if no cluster.
fn with_db(body: impl FnOnce(String)) {
    let Some(c) = cluster() else {
        eprintln!("SKIP: no postgres cluster available");
        return;
    };
    body(c.fresh_db());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn empty_load_yields_new_inventory() {
    with_db(|url| {
        let store = inv_store::PostgresStore::open(&url).expect("open");
        let inv = store.load().expect("load empty");
        assert!(inv.instances.is_empty(), "fresh store has no instances");
        assert!(inv.classes.is_empty(), "fresh store has no classes");
        assert_eq!(inv.next_id, 1, "fresh store next_id starts at 1");
    });
}

#[test]
fn transact_add_then_load_shows_it() {
    with_db(|url| {
        let store = inv_store::PostgresStore::open(&url).expect("open");

        let id = store
            .transact(&mut |inv| {
                inv.add_instance("Item", "thing", BTreeMap::new(), None, 1)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            })
            .expect("transact add");
        assert_eq!(id, 1, "first id handed out is 1");

        let inv = store.load().expect("reload");
        assert_eq!(inv.get(1).expect("instance present").name, "thing");
        assert_eq!(inv.next_id, 2, "counter advanced and was persisted");
    });
}

#[test]
fn transact_error_rolls_back() {
    with_db(|url| {
        let store = inv_store::PostgresStore::open(&url).expect("open");

        // Commit one instance first.
        store
            .transact(&mut |inv| {
                inv.add_instance("Item", "keep", BTreeMap::new(), None, 1)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            })
            .expect("first commit");

        // A transaction that mutates then returns Err must NOT persist.
        let res: Result<(), StoreError> = store.transact(&mut |inv| {
            inv.add_instance("Item", "discard", BTreeMap::new(), None, 2)
                .map_err(|e| StoreError::Backend(e.to_string()))?;
            Err(StoreError::Backend("boom".into()))
        });
        assert!(matches!(res, Err(StoreError::Backend(_))), "error propagates");

        let inv = store.load().expect("reload");
        assert_eq!(inv.instances.len(), 1, "rolled-back add did not persist");
        assert_eq!(inv.next_id, 2, "counter not advanced by rolled-back txn");
    });
}

#[test]
fn photo_put_get_delete_roundtrip() {
    with_db(|url| {
        let store = inv_store::PostgresStore::open(&url).expect("open");

        assert_eq!(store.get_photo("1-0").expect("get missing"), None);

        store.put_photo("1-0", b"hello").expect("put");
        assert_eq!(
            store.get_photo("1-0").expect("get").as_deref(),
            Some(&b"hello"[..])
        );

        // Overwrite (ON CONFLICT DO UPDATE).
        store.put_photo("1-0", b"world!!").expect("overwrite");
        assert_eq!(
            store.get_photo("1-0").expect("get2").as_deref(),
            Some(&b"world!!"[..])
        );

        // Binary bytes survive intact (not just UTF-8).
        let blob: Vec<u8> = (0u8..=255).collect();
        store.put_photo("bin", &blob).expect("put bin");
        assert_eq!(store.get_photo("bin").expect("get bin"), Some(blob));

        store.delete_photo("1-0").expect("delete");
        assert_eq!(store.get_photo("1-0").expect("get after delete"), None);
        // Deleting a missing key is a no-op.
        store.delete_photo("1-0").expect("delete missing is ok");
    });
}

/// The headline guarantee: `FOR UPDATE` row-locking serializes concurrent
/// transactors on the single inventory row, so there are NO lost updates.
///
/// N threads each run M transactions that add one instance to the SAME row.
/// If any update were lost we'd see fewer than N*M instances, or duplicate ids.
#[test]
fn concurrency_no_lost_updates() {
    with_db(|url| {
        const N: usize = 8;
        const M: usize = 10;

        // Each thread opens its OWN store handle over the same database url —
        // exactly how separate gateway workers would contend on one db.
        let mut handles = Vec::with_capacity(N);
        for t in 0..N {
            let url = url.clone();
            handles.push(thread::spawn(move || {
                let store = inv_store::PostgresStore::open(&url).expect("open in thread");
                for k in 0..M {
                    store
                        .transact(&mut |inv| {
                            let name = format!("t{t}-k{k}");
                            inv.add_instance("Item", &name, BTreeMap::new(), None, 1)
                                .map_err(|e| StoreError::Backend(e.to_string()))
                        })
                        .expect("concurrent transact");
                }
            }));
        }
        for h in handles {
            h.join().expect("thread join");
        }

        let inv = inv_store::PostgresStore::open(&url)
            .expect("open final")
            .load()
            .expect("final load");

        assert_eq!(
            inv.instances.len(),
            N * M,
            "exactly N*M instances => no lost updates"
        );
        assert_eq!(
            inv.next_id,
            (N * M) as i64 + 1,
            "id counter advanced exactly N*M times"
        );

        // Every id is unique and contiguous 1..=N*M (BTreeMap keys are the ids).
        let ids: Vec<i64> = inv.instances.keys().copied().collect();
        let expected: Vec<i64> = (1..=(N * M) as i64).collect();
        assert_eq!(ids, expected, "ids are unique and contiguous => no dup ids");
    });
}
