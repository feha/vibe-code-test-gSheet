//! [`FileStore`]: a [`Store`](crate::Store) backed by a single JSON document.
//!
//! ## Race-freedom
//!
//! Every transaction acquires an **exclusive OS advisory lock** on a dedicated
//! lock file (`<file>.lock`) and holds it for the *entire* read-modify-write:
//!
//! ```text
//! lock(EX) -> read+parse -> run closure -> write temp -> atomic rename -> unlock
//! ```
//!
//! Because the lock blocks until acquired, all transactors (threads *and*
//! processes, distinct `FileStore` handles included) are serialized over the
//! same backing file. No two read-modify-write critical sections ever overlap,
//! so there are no lost updates by construction. The commit itself is a
//! write-to-sibling-temp + `rename` (atomic on POSIX), so a reader never sees a
//! torn file even though it does not take the lock.
//!
//! The [`Store`](crate::Store) contract also mandates conflict-retry semantics.
//! With a held exclusive lock a single attempt already observes the latest
//! committed state, but we still wrap the body in a bounded retry loop: a
//! transient lock acquisition error is retried, and the design generalizes to
//! optimistic backends.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;
use inv_model::Inventory;

use crate::{Store, StoreError};

/// Maximum number of times a transaction body is (re-)run before giving up with
/// [`StoreError::Conflict`].
const MAX_ATTEMPTS: usize = 64;

/// A [`Store`](crate::Store) backed by one pretty-JSON file plus a sidecar
/// directory of photo bytes.
pub struct FileStore {
    /// Path to the JSON inventory document.
    path: PathBuf,
    /// Path to the advisory lock file (`<file>.lock`).
    lock_path: PathBuf,
    /// Path to the photo sidecar directory (`<file>.photos`).
    photos_dir: PathBuf,
}

impl FileStore {
    /// Create a store over the JSON document at `path`. The file, its lock file,
    /// and the photo sidecar directory are created lazily on first write.
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let lock_path = sibling_suffix(&path, ".lock");
        let photos_dir = sibling_suffix(&path, ".photos");
        FileStore {
            path,
            lock_path,
            photos_dir,
        }
    }

    /// Read and parse the inventory file, returning an empty inventory when the
    /// file is missing or empty. Assumes any needed locking is held by caller.
    fn read_inventory(&self) -> Result<Inventory, StoreError> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                if bytes.is_empty() {
                    return Ok(Inventory::new());
                }
                Inventory::from_json_bytes(&bytes)
                    .map_err(|e| StoreError::Backend(format!("parse {}: {e}", self.path.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Inventory::new()),
            Err(e) => Err(StoreError::Io(format!("read {}: {e}", self.path.display()))),
        }
    }

    /// Open (creating if needed) the dedicated lock file. Holding an exclusive
    /// lock on this handle guards the whole read-modify-write.
    fn open_lock_file(&self) -> Result<File, StoreError> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|e| StoreError::Io(format!("open lock {}: {e}", self.lock_path.display())))
    }

    /// Atomically write `inv` to the backing file: serialize, write to a sibling
    /// temp file, fsync, then `rename` over the target.
    fn commit_inventory(&self, inv: &Inventory) -> Result<(), StoreError> {
        let bytes = inv
            .to_json_bytes()
            .map_err(|e| StoreError::Backend(format!("serialize: {e}")))?;

        // Temp file lives in the same directory so the rename is on one volume.
        let tmp_path = sibling_suffix(&self.path, ".tmp");
        {
            let mut tmp = File::create(&tmp_path)
                .map_err(|e| StoreError::Io(format!("create temp {}: {e}", tmp_path.display())))?;
            tmp.write_all(&bytes)
                .map_err(|e| StoreError::Io(format!("write temp {}: {e}", tmp_path.display())))?;
            tmp.sync_all()
                .map_err(|e| StoreError::Io(format!("fsync temp {}: {e}", tmp_path.display())))?;
        }
        fs::rename(&tmp_path, &self.path).map_err(|e| {
            // Best-effort cleanup of the temp file on rename failure.
            let _ = fs::remove_file(&tmp_path);
            StoreError::Io(format!(
                "rename {} -> {}: {e}",
                tmp_path.display(),
                self.path.display()
            ))
        })?;
        Ok(())
    }

    /// Resolve the on-disk path for a photo `key`.
    fn photo_path(&self, key: &str) -> PathBuf {
        self.photos_dir.join(key)
    }
}

impl Store for FileStore {
    fn load(&self) -> Result<Inventory, StoreError> {
        // A shared lock is sufficient for a consistent read against in-flight
        // commits; combined with atomic rename this never observes a torn file.
        // (No file is created by load.)
        match File::open(&self.lock_path) {
            Ok(lock) => {
                lock.lock_shared().map_err(|e| {
                    StoreError::Io(format!("shared-lock {}: {e}", self.lock_path.display()))
                })?;
                let res = self.read_inventory();
                let _ = FileExt::unlock(&lock);
                res
            }
            // No lock file yet => store has never been written => empty.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.read_inventory(),
            Err(e) => Err(StoreError::Io(format!(
                "open lock {}: {e}",
                self.lock_path.display()
            ))),
        }
    }

    fn transact<T>(
        &self,
        f: &mut dyn FnMut(&mut Inventory) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut last_err: Option<StoreError> = None;
        for _ in 0..MAX_ATTEMPTS {
            // Acquire the exclusive lock for the whole read-modify-write. This
            // blocks until no other transactor holds it, serializing critical
            // sections across threads and processes.
            let lock = self.open_lock_file()?;
            if let Err(e) = lock.lock_exclusive() {
                last_err = Some(StoreError::Io(format!(
                    "exclusive-lock {}: {e}",
                    self.lock_path.display()
                )));
                continue;
            }

            // Read the latest committed state under the lock, run the closure,
            // and commit atomically — all before releasing.
            let result = (|| {
                let mut inv = self.read_inventory()?;
                let value = f(&mut inv)?;
                self.commit_inventory(&inv)?;
                Ok(value)
            })();

            let _ = FileExt::unlock(&lock);
            return result;
        }
        Err(last_err.unwrap_or(StoreError::Conflict))
    }

    fn get_photo(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.photo_path(key);
        match File::open(&path) {
            Ok(mut file) => {
                let mut buf = Vec::new();
                file.read_to_end(&mut buf)
                    .map_err(|e| StoreError::Io(format!("read photo {}: {e}", path.display())))?;
                Ok(Some(buf))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Io(format!("open photo {}: {e}", path.display()))),
        }
    }

    fn put_photo(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError> {
        fs::create_dir_all(&self.photos_dir).map_err(|e| {
            StoreError::Io(format!(
                "create photos dir {}: {e}",
                self.photos_dir.display()
            ))
        })?;
        let path = self.photo_path(key);
        // Write-to-temp + atomic rename so a concurrent reader never sees a torn
        // photo file.
        let tmp = self.photo_path(&format!("{key}.tmp"));
        {
            let mut file = File::create(&tmp).map_err(|e| {
                StoreError::Io(format!("create temp photo {}: {e}", tmp.display()))
            })?;
            file.write_all(bytes).map_err(|e| {
                StoreError::Io(format!("write temp photo {}: {e}", tmp.display()))
            })?;
            file.sync_all().map_err(|e| {
                StoreError::Io(format!("fsync temp photo {}: {e}", tmp.display()))
            })?;
        }
        fs::rename(&tmp, &path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            StoreError::Io(format!(
                "rename photo {} -> {}: {e}",
                tmp.display(),
                path.display()
            ))
        })
    }

    fn delete_photo(&self, key: &str) -> Result<(), StoreError> {
        let path = self.photo_path(key);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            // Deleting a missing key is a no-op.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Io(format!(
                "delete photo {}: {e}",
                path.display()
            ))),
        }
    }
}

/// Build a sibling path by appending `suffix` to the file name of `path`.
/// E.g. `/d/inv.json` + `.lock` -> `/d/inv.json.lock`.
fn sibling_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}
