//! Test helper binary: perform `M` transactions against a `FileStore`, each
//! adding one instance. Used by the multi-process concurrency proof to show that
//! the exclusive advisory lock serializes read-modify-write across *processes*
//! (not just threads). Args: `<json-path> <writer-tag> <M>`.

use std::process::ExitCode;

use inv_core::InventoryExt;
use inv_store::{FileStore, StoreError, StoreExt};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: concurrent_writer <json-path> <tag> <count>");
        return ExitCode::from(2);
    }
    let path = &args[1];
    let tag = &args[2];
    let count: usize = args[3].parse().expect("count must be a usize");

    let store = FileStore::new(path);
    for m in 0..count {
        let r = store.transact(&mut |inv| {
            inv.add_instance("Item", &format!("{tag}-{m}"), Default::default(), None, 0)
                .map_err(|e| StoreError::Backend(e.to_string()))
        });
        if let Err(e) = r {
            eprintln!("transaction failed: {e}");
            return ExitCode::from(1);
        }
    }
    ExitCode::SUCCESS
}
