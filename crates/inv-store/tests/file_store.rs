//! Integration tests for [`FileStore`] and the [`Store`] contract.
//!
//! The headline test is `concurrency_proof_no_lost_updates`: N OS threads each
//! perform M transactions; afterwards the on-disk inventory must contain exactly
//! N*M instances with unique ids (zero lost updates).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::thread;

use inv_core::InventoryExt;
use inv_model::{Inventory, Photo};
use inv_store::{FileStore, Store, StoreError};

use tempfile::tempdir;

/// load() on a missing file yields an empty inventory.
#[test]
fn load_missing_file_is_empty_inventory() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("inv.json");
    let store = FileStore::new(&path);

    let inv = store.load().unwrap();
    assert_eq!(inv, Inventory::new());
    assert!(inv.instances.is_empty());
    assert!(inv.classes.is_empty());
    assert_eq!(inv.next_id, 1);
    // load() is read-only: it must not create the file.
    assert!(!path.exists(), "load() must not create the backing file");
}

/// A single transaction is loaded back, and the file is written as pretty JSON.
#[test]
fn transact_commits_and_persists_pretty_json() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("inv.json");
    let store = FileStore::new(&path);

    let id = store
        .transact(&mut |inv| {
            inv.add_instance("Bin", "Screws", Default::default(), None, 1000)
                .map_err(|e| StoreError::Backend(e.to_string()))
        })
        .unwrap();
    assert_eq!(id, 1);

    assert!(path.exists(), "commit must create the file");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains('\n'), "on-disk JSON must be pretty-printed");
    assert!(text.contains("\"Screws\""));

    // A fresh store over the same path observes the committed change.
    let store2 = FileStore::new(&path);
    let inv = store2.load().unwrap();
    assert_eq!(inv.instances.len(), 1);
    assert_eq!(inv.get(1).unwrap().name, "Screws");
    assert_eq!(inv.next_id, 2);
}

/// A second transactor observes the first transactor's committed change.
#[test]
fn second_transactor_observes_first_commit() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("inv.json");
    let store_a = FileStore::new(&path);
    let store_b = FileStore::new(&path);

    store_a
        .transact(&mut |inv| {
            inv.add_instance("C", "a", Default::default(), None, 1)
                .map_err(|e| StoreError::Backend(e.to_string()))
        })
        .unwrap();

    // store_b sees the committed instance inside its own transaction.
    let count = store_b
        .transact(&mut |inv| Ok(inv.instances.len()))
        .unwrap();
    assert_eq!(count, 1);

    // and a follow-up add gets a fresh, non-colliding id.
    let id2 = store_b
        .transact(&mut |inv| {
            inv.add_instance("C", "b", Default::default(), None, 2)
                .map_err(|e| StoreError::Backend(e.to_string()))
        })
        .unwrap();
    assert_eq!(id2, 2);
    assert_eq!(store_a.load().unwrap().instances.len(), 2);
}

/// The transaction return value is propagated, and a closure error aborts the
/// commit (no partial write).
#[test]
fn transact_error_aborts_commit() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("inv.json");
    let store = FileStore::new(&path);

    // seed one instance
    store
        .transact(&mut |inv| {
            inv.add_instance("C", "keep", Default::default(), None, 1)
                .map_err(|e| StoreError::Backend(e.to_string()))
        })
        .unwrap();

    // a closure that mutates then errors must NOT persist its mutation.
    let err = store
        .transact(&mut |inv| -> Result<(), StoreError> {
            inv.add_instance("C", "doomed", Default::default(), None, 2)
                .map_err(|e| StoreError::Backend(e.to_string()))?;
            Err(StoreError::Backend("boom".into()))
        })
        .unwrap_err();
    assert_eq!(err, StoreError::Backend("boom".into()));

    let inv = store.load().unwrap();
    assert_eq!(inv.instances.len(), 1, "errored transaction must not commit");
    assert_eq!(inv.get(1).unwrap().name, "keep");
}

/// Photo put/get/delete roundtrip.
#[test]
fn photo_roundtrip() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("inv.json");
    let store = FileStore::new(&path);

    // missing photo -> None
    assert_eq!(store.get_photo("1-0").unwrap(), None);

    let bytes = b"\xff\xd8\xff\x00binary photo bytes\x00\x01\x02";
    store.put_photo("1-0", bytes).unwrap();
    assert_eq!(store.get_photo("1-0").unwrap().as_deref(), Some(&bytes[..]));

    // overwrite
    store.put_photo("1-0", b"new").unwrap();
    assert_eq!(store.get_photo("1-0").unwrap().as_deref(), Some(&b"new"[..]));

    // delete
    store.delete_photo("1-0").unwrap();
    assert_eq!(store.get_photo("1-0").unwrap(), None);
    // deleting a missing key is a no-op
    store.delete_photo("1-0").unwrap();
}

/// Photos are addressed by key and integrate with the model's Photo records.
#[test]
fn photo_keys_pair_with_model() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("inv.json");
    let store = FileStore::new(&path);

    let id = store
        .transact(&mut |inv| {
            let id = inv
                .add_instance("Bin", "b", Default::default(), None, 1)
                .map_err(|e| StoreError::Backend(e.to_string()))?;
            let key = format!("{id}-0");
            inv.attach_photo(
                id,
                Photo {
                    key: key.clone(),
                    mime: "image/jpeg".into(),
                    name: "b.jpg".into(),
                },
                1,
            )
            .map_err(|e| StoreError::Backend(e.to_string()))?;
            Ok(id)
        })
        .unwrap();

    let key = format!("{id}-0");
    store.put_photo(&key, b"jpegdata").unwrap();

    let inv = store.load().unwrap();
    let photo = &inv.get(id).unwrap().photos[0];
    assert_eq!(photo.key, key);
    assert_eq!(store.get_photo(&photo.key).unwrap().as_deref(), Some(&b"jpegdata"[..]));
}

/// CONCURRENCY PROOF: N threads x M transactions; every add must survive.
///
/// Each thread shares one `Arc<FileStore>` and performs M transactions, each
/// adding one instance. Because `transact` is a serialized read-modify-write
/// with conflict-retry, the final on-disk inventory must contain exactly N*M
/// instances with unique ids — zero lost updates.
#[test]
fn concurrency_proof_no_lost_updates() {
    const N: usize = 16; // OS threads
    const M: usize = 25; // transactions per thread

    let dir = tempdir().unwrap();
    let path = dir.path().join("inv.json");
    let store = Arc::new(FileStore::new(&path));

    let mut handles = Vec::with_capacity(N);
    for t in 0..N {
        let store = Arc::clone(&store);
        handles.push(thread::spawn(move || {
            for m in 0..M {
                store
                    .transact(&mut |inv| {
                        inv.add_instance(
                            "Item",
                            &format!("t{t}-m{m}"),
                            Default::default(),
                            None,
                            (t * 1000 + m) as i64,
                        )
                        .map_err(|e| StoreError::Backend(e.to_string()))
                    })
                    .expect("transaction must succeed");
            }
        }));
    }
    for h in handles {
        h.join().expect("thread panicked");
    }

    let inv = store.load().unwrap();
    // Exactly N*M instances persisted.
    assert_eq!(
        inv.instances.len(),
        N * M,
        "expected {} instances (zero lost updates), got {}",
        N * M,
        inv.instances.len()
    );
    // All ids unique (BTreeMap keys are inherently unique, but verify the stored
    // Instance.id matches its key and ids are exactly 1..=N*M).
    let mut ids: BTreeSet<i64> = BTreeSet::new();
    for (k, inst) in &inv.instances {
        assert_eq!(*k, inst.id, "map key must equal instance id");
        assert!(ids.insert(inst.id), "duplicate id {}", inst.id);
    }
    let expected: BTreeSet<i64> = (1..=(N * M) as i64).collect();
    assert_eq!(ids, expected, "ids must be the contiguous set 1..=N*M");
    assert_eq!(inv.next_id, (N * M) as i64 + 1);
}

/// Concurrent transactors with separate `FileStore` instances over the SAME
/// path also lose no updates (independent handles, shared file lock).
#[test]
fn concurrency_separate_handles_same_path() {
    const N: usize = 8;
    const M: usize = 20;

    let dir = tempdir().unwrap();
    let path = dir.path().join("inv.json");

    let mut handles = Vec::with_capacity(N);
    for t in 0..N {
        let path = path.clone();
        handles.push(thread::spawn(move || {
            // Each thread builds its OWN store handle over the same path.
            let store = FileStore::new(&path);
            for m in 0..M {
                store
                    .transact(&mut |inv| {
                        inv.add_instance(
                            "Item",
                            &format!("t{t}-m{m}"),
                            Default::default(),
                            None,
                            0,
                        )
                        .map_err(|e| StoreError::Backend(e.to_string()))
                    })
                    .expect("transaction must succeed");
            }
        }));
    }
    for h in handles {
        h.join().expect("thread panicked");
    }

    let store = FileStore::new(&path);
    let inv = store.load().unwrap();
    assert_eq!(inv.instances.len(), N * M);
    let ids: BTreeSet<i64> = inv.instances.keys().copied().collect();
    let expected: BTreeSet<i64> = (1..=(N * M) as i64).collect();
    assert_eq!(ids, expected);
}

/// CONCURRENCY PROOF (cross-process): spawn N separate OS *processes*, each
/// running M transactions against the same backing file. Advisory `flock` locks
/// are held per-process, so this proves the lock serializes read-modify-write
/// across process boundaries — the strongest form of the no-lost-updates claim.
#[test]
fn concurrency_proof_cross_process() {
    const N: usize = 8; // child processes
    const M: usize = 20; // transactions per process

    let exe = env!("CARGO_BIN_EXE_concurrent_writer");
    let dir = tempdir().unwrap();
    let path = dir.path().join("inv.json");

    let mut children = Vec::with_capacity(N);
    for t in 0..N {
        let child = std::process::Command::new(exe)
            .arg(&path)
            .arg(format!("p{t}"))
            .arg(M.to_string())
            .spawn()
            .expect("spawn writer process");
        children.push(child);
    }
    for mut c in children {
        let status = c.wait().expect("wait writer process");
        assert!(status.success(), "writer process failed: {status:?}");
    }

    let store = FileStore::new(&path);
    let inv = store.load().unwrap();
    assert_eq!(
        inv.instances.len(),
        N * M,
        "cross-process lost updates: expected {} got {}",
        N * M,
        inv.instances.len()
    );
    let ids: BTreeSet<i64> = inv.instances.keys().copied().collect();
    let expected: BTreeSet<i64> = (1..=(N * M) as i64).collect();
    assert_eq!(ids, expected, "ids must be the contiguous set 1..=N*M");
    assert_eq!(inv.next_id, (N * M) as i64 + 1);
}

/// A retrying transaction observes a fresh reload each attempt: the closure that
/// reads then writes must see committed concurrent work, never stale state.
#[test]
fn transact_reloads_latest_each_attempt() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("inv.json");
    let store = FileStore::new(&path);

    // Seed 3 instances across 3 transactions.
    for i in 0..3 {
        store
            .transact(&mut |inv| {
                inv.add_instance("C", &format!("n{i}"), Default::default(), None, i)
                    .map_err(|e| StoreError::Backend(e.to_string()))
            })
            .unwrap();
    }

    // Inside the transaction, the running count reflects all prior commits.
    let seen = store.transact(&mut |inv| Ok(inv.instances.len())).unwrap();
    assert_eq!(seen, 3);
}

mod props {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        // These properties do real fsync'd file I/O per case, so keep the case
        // count modest; the headline concurrency proof carries the heavy load.
        #![proptest_config(ProptestConfig::with_cases(16))]

        // Property: a sequence of `adds` independent transactions, each adding
        // one instance, persists exactly `adds` instances with the contiguous
        // unique id set 1..=adds, surviving every commit/reload roundtrip.
        #[test]
        fn prop_sequential_adds_persist_all(adds in 0usize..40) {
            let dir = tempdir().unwrap();
            let path = dir.path().join("inv.json");
            let store = FileStore::new(&path);

            for i in 0..adds {
                store
                    .transact(&mut |inv| {
                        inv.add_instance("C", &format!("n{i}"), Default::default(), None, i as i64)
                            .map_err(|e| StoreError::Backend(e.to_string()))
                    })
                    .unwrap();
            }

            // Reload through a fresh handle to prove durability across handles.
            let inv = FileStore::new(&path).load().unwrap();
            prop_assert_eq!(inv.instances.len(), adds);
            let ids: BTreeSet<i64> = inv.instances.keys().copied().collect();
            let expected: BTreeSet<i64> = (1..=adds as i64).collect();
            prop_assert_eq!(ids, expected);
            prop_assert_eq!(inv.next_id, adds as i64 + 1);
        }

        // Property: concurrent threads each running a few transactions never lose
        // updates (smaller scale than the headline test, randomized shape).
        #[test]
        fn prop_concurrent_adds_no_loss(
            threads in 2usize..6,
            per in 1usize..8,
        ) {
            let dir = tempdir().unwrap();
            let path = dir.path().join("inv.json");
            let store = Arc::new(FileStore::new(&path));

            let mut handles = Vec::new();
            for t in 0..threads {
                let store = Arc::clone(&store);
                handles.push(thread::spawn(move || {
                    for m in 0..per {
                        store
                            .transact(&mut |inv| {
                                inv.add_instance("C", &format!("t{t}m{m}"), Default::default(), None, 0)
                                    .map_err(|e| StoreError::Backend(e.to_string()))
                            })
                            .unwrap();
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }

            let inv = store.load().unwrap();
            let total = threads * per;
            prop_assert_eq!(inv.instances.len(), total);
            let ids: BTreeSet<i64> = inv.instances.keys().copied().collect();
            let expected: BTreeSet<i64> = (1..=total as i64).collect();
            prop_assert_eq!(ids, expected);
        }
    }
}
