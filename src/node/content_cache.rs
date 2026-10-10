//! A per-node cache of file contents, keyed by SHA-256.
//!
//! When several pods on a node use the same file, it is downloaded only once.
//! Each volume gets its own copy of a cached file. The copy uses `copy_file_range`,
//! which file systems like XFS and Btrfs turn into a cheap shared copy. A pod that
//! writes to its copy therefore never changes the cache. Cache files are read-only
//! and are checked against their digest before they enter the cache.

use crate::node::paths;
use crate::node::transfer::{self, ByteBudget};
use crate::util::error::io_status;
use opendal::Operator;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tonic::Status;

pub struct ContentCache {
    objects: PathBuf,
    tmp: PathBuf,
    max_bytes: u64,
    /// One lock per digest, so concurrent volumes download a file only once.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// What a prune run removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub removed_files: usize,
    pub removed_bytes: u64,
    pub remaining_bytes: u64,
}

impl ContentCache {
    /// Open a cache in `dir`. Leftovers of interrupted downloads are removed.
    pub fn open(dir: &Path, max_bytes: u64) -> std::io::Result<Self> {
        let objects = dir.join("sha256");
        let tmp = dir.join("tmp");
        std::fs::create_dir_all(&objects)?;
        match std::fs::remove_dir_all(&tmp) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        std::fs::create_dir_all(&tmp)?;
        Ok(Self {
            objects,
            tmp,
            max_bytes,
            locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn set_max_bytes(&mut self, max_bytes: u64) {
        self.max_bytes = max_bytes;
    }

    /// Path of a cached object. `digest` must be lowercase hex (checked by the manifest).
    ///
    /// `scope` names the DataNode the content came from. A cache hit needs the same
    /// scope, so a pod can never get content of a DataNode it may not read just by
    /// knowing its digest.
    pub fn path(&self, scope: &str, digest: &str) -> PathBuf {
        self.objects.join(object_name(scope, digest))
    }

    fn lock_for(&self, digest: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|p| p.into_inner());
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        locks.entry(digest.to_string()).or_default().clone()
    }

    /// Make sure the object with `digest` is in the cache, downloading `key` on a miss.
    ///
    /// The bytes count against `budget` once, for a hit or a miss, because the file
    /// will be copied into the volume.
    pub async fn fetch(
        &self,
        op: &Operator,
        scope: &str,
        key: &str,
        digest: &str,
        budget: &ByteBudget,
    ) -> Result<PathBuf, Status> {
        let lock = self.lock_for(&object_name(scope, digest));
        let _guard = lock.lock().await;
        self.fetch_locked(op, scope, key, digest, budget).await
    }

    /// Like [`ContentCache::fetch`], then copy the object to `dest`.
    ///
    /// The object stays locked until the copy is done, so [`ContentCache::prune`]
    /// cannot remove it between the lookup and the copy.
    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_into(
        &self,
        op: &Operator,
        scope: &str,
        key: &str,
        digest: &str,
        budget: &ByteBudget,
        dest: &Path,
        executable: bool,
    ) -> Result<(), Status> {
        let lock = self.lock_for(&object_name(scope, digest));
        let _guard = lock.lock().await;
        let cached = self.fetch_locked(op, scope, key, digest, budget).await?;
        Self::place(&cached, dest, executable).await
    }

    /// The work of `fetch`. The caller must hold the lock of the object.
    async fn fetch_locked(
        &self,
        op: &Operator,
        scope: &str,
        key: &str,
        digest: &str,
        budget: &ByteBudget,
    ) -> Result<PathBuf, Status> {
        let path = self.path(scope, digest);

        if let Ok(meta) = tokio::fs::metadata(&path).await {
            budget.add(meta.len())?;
            // Mark the object as recently used, for pruning.
            let touch = path.clone();
            let _ = tokio::task::spawn_blocking(move || {
                std::fs::File::open(&touch).and_then(|f| f.set_modified(SystemTime::now()))
            })
            .await;
            return Ok(path);
        }

        let tmp = self.tmp.join(uuid::Uuid::new_v4().simple().to_string());
        let result = transfer::download_file(op, key, &tmp, budget, true).await;
        let downloaded = match result {
            Ok(downloaded) => downloaded,
            Err(err) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(err);
            }
        };
        if downloaded.sha256.as_deref() != Some(digest) {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(Status::data_loss(format!(
                "content of {key} has sha256 {}, but the manifest expects {digest}",
                downloaded.sha256.unwrap_or_default()
            )));
        }

        let finish = {
            let tmp = tmp.clone();
            let path = path.clone();
            move || -> std::io::Result<()> {
                let file = std::fs::File::open(&tmp)?;
                file.sync_all()?;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444))?;
                std::fs::rename(&tmp, &path)
            }
        };
        let done = tokio::task::spawn_blocking(finish)
            .await
            .map_err(|e| Status::internal(format!("cache task failed: {e}")))?;
        if let Err(err) = done {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(io_status(err, "store file in content cache"));
        }
        Ok(path)
    }

    /// Copy a cached object into a volume and set its mode.
    pub async fn place(cached: &Path, dest: &Path, executable: bool) -> Result<(), Status> {
        let cached = cached.to_path_buf();
        let dest = dest.to_path_buf();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::copy(&cached, &dest)?;
            let mode = if executable { 0o755 } else { 0o644 };
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode))
        })
        .await
        .map_err(|e| Status::internal(format!("copy task failed: {e}")))?
        .map_err(|e| io_status(e, "copy file from content cache"))
    }

    /// Remove the least recently used objects until the cache fits its size limit.
    pub fn prune(&self) -> std::io::Result<PruneReport> {
        let mut entries = Vec::new();
        let mut total = 0u64;
        for entry in std::fs::read_dir(&self.objects)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if !meta.is_file() {
                continue;
            }
            total += meta.len();
            let used = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            entries.push((used, meta.len(), entry.path(), entry.file_name()));
        }
        entries.sort_by_key(|(used, ..)| *used);

        let mut report = PruneReport::default();
        for (_, size, path, name) in entries {
            if total <= self.max_bytes {
                break;
            }
            // Skip objects that a running publish holds right now. The lock map stays
            // locked until the file is gone, so no publish can start using it in
            // between: it either holds the lock already (busy) or sees a miss.
            let object = name.to_string_lossy().to_string();
            let locks = self.locks.lock().unwrap_or_else(|p| p.into_inner());
            let busy = locks
                .get(&object)
                .is_some_and(|lock| Arc::strong_count(lock) > 1);
            if busy {
                continue;
            }
            let removed = std::fs::remove_file(&path);
            drop(locks);
            match removed {
                Ok(()) => {
                    total -= size;
                    report.removed_files += 1;
                    report.removed_bytes += size;
                }
                Err(err) => tracing::warn!("cannot remove cached object {path:?}: {err}"),
            }
        }
        report.remaining_bytes = total;
        Ok(report)
    }
}

/// File name of a cached object: a short hash of the scope, then the digest.
fn object_name(scope: &str, digest: &str) -> String {
    let scope_hash = paths::to_hex(&Sha256::digest(scope.as_bytes()));
    format!("{}-{digest}", &scope_hash[..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendal::services::Memory;
    use std::os::unix::fs::MetadataExt;

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    async fn op_with(key: &str, content: &str) -> Operator {
        let op = Operator::new(Memory::default()).unwrap();
        op.write(key, content.to_string()).await.unwrap();
        op
    }

    #[tokio::test]
    async fn downloads_once_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentCache::open(dir.path(), u64::MAX).unwrap();
        let op = op_with("a", "abc").await;
        let budget = ByteBudget::default();

        let path = cache.fetch(&op, "ns/dn", "a", ABC, &budget).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "abc");
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o444);

        // A hit does not read the object again, even if storage changed.
        op.write("a", "changed").await.unwrap();
        let again = cache.fetch(&op, "ns/dn", "a", ABC, &budget).await.unwrap();
        assert_eq!(again, path);
        assert_eq!(std::fs::read_to_string(&again).unwrap(), "abc");
        // Both uses count against the volume budget.
        assert_eq!(budget.used(), 6);
    }

    #[tokio::test]
    async fn wrong_digest_is_rejected_and_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentCache::open(dir.path(), u64::MAX).unwrap();
        let op = op_with("a", "not abc").await;
        let err = cache
            .fetch(&op, "ns/dn", "a", ABC, &ByteBudget::default())
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::DataLoss);
        assert!(!cache.path("ns/dn", ABC).exists());
        assert_eq!(
            std::fs::read_dir(dir.path().join("tmp")).unwrap().count(),
            0
        );
    }

    #[tokio::test]
    async fn placed_copies_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentCache::open(&dir.path().join("c"), u64::MAX).unwrap();
        let op = op_with("a", "abc").await;
        let cached = cache
            .fetch(&op, "ns/dn", "a", ABC, &ByteBudget::default())
            .await
            .unwrap();
        let dest = dir.path().join("copy");
        ContentCache::place(&cached, &dest, true).await.unwrap();
        assert_eq!(std::fs::metadata(&dest).unwrap().mode() & 0o777, 0o755);

        // The pod writes to its copy; the cache stays intact.
        std::fs::write(&dest, "overwritten").unwrap();
        assert_eq!(std::fs::read_to_string(&cached).unwrap(), "abc");
    }

    #[tokio::test]
    async fn prune_skips_objects_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentCache::open(dir.path(), 0).unwrap();
        let op = op_with("a", "abc").await;
        let path = cache
            .fetch(&op, "ns/dn", "a", ABC, &ByteBudget::default())
            .await
            .unwrap();
        // While a publish holds the object, prune must leave it alone.
        let lock = cache.lock_for(&object_name("ns/dn", ABC));
        let guard = lock.lock().await;
        assert_eq!(cache.prune().unwrap().removed_files, 0);
        assert!(path.exists());
        drop(guard);
        drop(lock);
        assert_eq!(cache.prune().unwrap().removed_files, 1);
    }

    #[tokio::test]
    async fn fetch_into_copies_the_object() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentCache::open(&dir.path().join("c"), 0).unwrap();
        let op = op_with("a", "abc").await;
        let dest = dir.path().join("copy");
        cache
            .fetch_into(&op, "ns/dn", "a", ABC, &ByteBudget::default(), &dest, false)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "abc");
    }

    #[tokio::test]
    async fn prune_removes_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentCache::open(dir.path(), 4).unwrap();
        let old = cache.path("ns/dn", &"1".repeat(64));
        let new = cache.path("ns/dn", &"2".repeat(64));
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        let past = SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();

        let report = cache.prune().unwrap();
        assert_eq!(report.removed_files, 1);
        assert_eq!(report.remaining_bytes, 3);
        assert!(!old.exists());
        assert!(new.exists());
    }

    #[test]
    fn open_clears_partial_downloads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tmp")).unwrap();
        std::fs::write(dir.path().join("tmp/partial"), "x").unwrap();
        ContentCache::open(dir.path(), 1).unwrap();
        assert!(!dir.path().join("tmp/partial").exists());
    }
}
