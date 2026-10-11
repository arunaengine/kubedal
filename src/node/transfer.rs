//! Move files between object storage and local directories.
//!
//! Downloads stream to disk in chunks, so large files never sit in memory.

use crate::node::paths::{self, PathError};
use crate::util::error::{io_status, opendal_status};
use futures::{StreamExt, TryStreamExt};
use opendal::{ErrorKind, Operator};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt;
use tonic::Status;

/// How many files are transferred at the same time.
pub const CONCURRENCY: usize = 8;

/// Counts transferred bytes and stops when a size limit is reached.
#[derive(Debug, Default)]
pub struct ByteBudget {
    limit: Option<u64>,
    used: AtomicU64,
}

impl ByteBudget {
    pub fn new(limit: Option<u64>) -> Self {
        Self {
            limit,
            used: AtomicU64::new(0),
        }
    }

    pub fn limit(&self) -> Option<u64> {
        self.limit
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Fail early if a known total size does not fit the limit.
    pub fn check_total(&self, total: u64) -> Result<(), Status> {
        match self.limit {
            Some(limit) if total > limit => Err(Status::resource_exhausted(format!(
                "volume needs {total} bytes, but the DataNode limit is {limit} bytes"
            ))),
            _ => Ok(()),
        }
    }

    /// Count bytes that were written. Fails once the limit is passed.
    pub fn add(&self, bytes: u64) -> Result<(), Status> {
        let used = self.used.fetch_add(bytes, Ordering::Relaxed) + bytes;
        match self.limit {
            Some(limit) if used > limit => Err(Status::resource_exhausted(format!(
                "volume content passed the DataNode limit of {limit} bytes"
            ))),
            _ => Ok(()),
        }
    }
}

/// A file found below a prefix in object storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    /// Full key in the DataNode.
    pub key: String,
    /// Path relative to the volume root.
    pub relative: String,
    /// Size reported by the listing (0 if unknown).
    pub size: u64,
}

/// Everything found below a prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteTree {
    /// Prefix in the DataNode that relative paths start from, like `data/` or "".
    /// For a single file this is the folder of the file.
    pub base: String,
    /// Directories relative to the volume root, including empty ones.
    pub dirs: Vec<String>,
    pub files: Vec<RemoteFile>,
}

impl RemoteTree {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

fn path_status(err: PathError, key: &str) -> Status {
    Status::invalid_argument(format!(
        "object {key:?} cannot be placed in a volume: {err}"
    ))
}

/// List everything below `prefix` (a path inside the DataNode).
///
/// If `prefix` points to a single file, the tree holds only that file, named after it.
/// A prefix that does not exist gives an empty tree.
pub async fn list_tree(op: &Operator, prefix: &str) -> Result<RemoteTree, Status> {
    // The path is checked as written: only one leading "/" may go ("." and
    // "/" still mean the root). It is also read as a single object key, so it
    // must reach exactly that object.
    if prefix != "." && crate::util::opendal::exact_user_key(prefix).is_none() {
        return Err(Status::invalid_argument(format!(
            "invalid path {prefix:?}: storage would read another object"
        )));
    }
    let prefix = paths::normalize_prefix(prefix)
        .map_err(|e| Status::invalid_argument(format!("invalid path {prefix:?}: {e}")))?;

    if let Some(key) = prefix.strip_suffix('/') {
        match op.stat(key).await {
            Ok(meta) if meta.is_file() => {
                let (base, name) = match key.rsplit_once('/') {
                    Some((parent, name)) => (format!("{parent}/"), name.to_string()),
                    None => (String::new(), key.to_string()),
                };
                return Ok(RemoteTree {
                    base,
                    dirs: vec![],
                    files: vec![RemoteFile {
                        key: key.to_string(),
                        relative: name,
                        size: meta.content_length(),
                    }],
                });
            }
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(opendal_status(err, &format!("stat {key}"))),
        }
    }

    let mut lister = op
        .lister_with(&prefix)
        .recursive(true)
        .await
        .map_err(|e| opendal_status(e, &format!("list {prefix}")))?;

    let mut tree = RemoteTree {
        base: prefix.clone(),
        ..Default::default()
    };
    while let Some(entry) = lister
        .try_next()
        .await
        .map_err(|e| opendal_status(e, &format!("list {prefix}")))?
    {
        let key = entry.path();
        let Some(relative) = key.strip_prefix(prefix.as_str()) else {
            continue;
        };
        if relative.is_empty() || relative == "/" {
            continue;
        }
        let relative = paths::safe_relative(relative).map_err(|e| path_status(e, key))?;
        if entry.metadata().is_dir() {
            tree.dirs.push(relative);
        } else {
            tree.files.push(RemoteFile {
                key: key.to_string(),
                relative,
                size: entry.metadata().content_length(),
            });
        }
    }
    Ok(tree)
}

/// Download a whole tree into `dest`. `dest` must exist.
///
/// On error the caller must remove `dest`, because files may be incomplete.
pub async fn download_tree(
    op: &Operator,
    tree: &RemoteTree,
    dest: &Path,
    budget: &ByteBudget,
) -> Result<(), Status> {
    budget.check_total(tree.total_size())?;

    for dir in &tree.dirs {
        create_dir_all(&dest.join(dir)).await?;
    }

    // Wait for every started download, also after an error. A dropped download
    // may still have blocking file work running, which could finish after the
    // caller removed `dest` or during a retry. After the first error no new
    // file starts.
    let first_error: std::sync::Mutex<Option<Status>> = std::sync::Mutex::new(None);
    futures::stream::iter(tree.files.iter())
        .for_each_concurrent(CONCURRENCY, |file| async {
            let failed = || first_error.lock().is_ok_and(|e| e.is_some());
            if failed() {
                return;
            }
            let target = dest.join(&file.relative);
            let result = async {
                if let Some(parent) = target.parent() {
                    create_dir_all(parent).await?;
                }
                download_file(op, &file.key, &target, budget, false).await
            }
            .await;
            if let Err(err) = result
                && let Ok(mut slot) = first_error.lock()
            {
                slot.get_or_insert(err);
            }
        })
        .await;
    match first_error.into_inner().unwrap_or_else(|p| p.into_inner()) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

pub async fn create_dir_all(path: &Path) -> Result<(), Status> {
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(path)
        .await
        .map_err(|e| io_status(e, &format!("create directory {}", path.display())))
}

/// Result of a single file download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    pub size: u64,
    /// Lowercase hex SHA-256, if it was asked for.
    pub sha256: Option<String>,
}

/// Stream one object into a new local file with mode 0644.
///
/// The file must not exist yet. This also refuses to follow a symlink at `dest`.
pub async fn download_file(
    op: &Operator,
    key: &str,
    dest: &Path,
    budget: &ByteBudget,
    hash: bool,
) -> Result<Downloaded, Status> {
    // A key that storage changes (like "x " read as "x") would give the content
    // of another object.
    if !crate::util::opendal::is_exact_key(key) {
        return Err(Status::invalid_argument(format!(
            "object {key:?} cannot be read under its own name"
        )));
    }
    let mut stream = op
        .reader(key)
        .await
        .map_err(|e| opendal_status(e, &format!("open {key}")))?
        .into_bytes_stream(..)
        .await
        .map_err(|e| opendal_status(e, &format!("read {key}")))?;

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(dest)
        .await
        .map_err(|e| io_status(e, &format!("create {}", dest.display())))?;

    let mut hasher = hash.then(Sha256::new);
    let mut size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| io_status(e, &format!("download {key}")))?;
        budget.add(chunk.len() as u64)?;
        size += chunk.len() as u64;
        if let Some(hasher) = hasher.as_mut() {
            hasher.update(&chunk);
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| io_status(e, &format!("write {}", dest.display())))?;
    }
    file.flush()
        .await
        .map_err(|e| io_status(e, &format!("write {}", dest.display())))?;

    Ok(Downloaded {
        size,
        sha256: hasher.map(|h| paths::to_hex(&h.finalize())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendal::services::Memory;

    async fn memory_with(files: &[(&str, &str)]) -> Operator {
        let op = Operator::new(Memory::default()).unwrap();
        for (key, content) in files {
            if key.ends_with('/') {
                op.create_dir(key).await.unwrap();
            } else {
                op.write(key, content.to_string()).await.unwrap();
            }
        }
        op
    }

    #[test]
    fn budget_enforces_limit() {
        let budget = ByteBudget::new(Some(10));
        assert!(budget.check_total(10).is_ok());
        assert_eq!(
            budget.check_total(11).unwrap_err().code(),
            tonic::Code::ResourceExhausted
        );
        assert!(budget.add(6).is_ok());
        assert!(budget.add(4).is_ok());
        assert_eq!(
            budget.add(1).unwrap_err().code(),
            tonic::Code::ResourceExhausted
        );
        assert!(ByteBudget::new(None).add(u32::MAX as u64).is_ok());
    }

    #[tokio::test]
    async fn lists_recursively_below_prefix() {
        let op = memory_with(&[
            ("data/a.txt", "a"),
            ("data/sub/b.txt", "bb"),
            ("data/empty/", ""),
            ("other/c.txt", "c"),
        ])
        .await;
        let mut tree = list_tree(&op, "/data").await.unwrap();
        tree.files.sort_by(|a, b| a.relative.cmp(&b.relative));
        let names: Vec<_> = tree.files.iter().map(|f| f.relative.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "sub/b.txt"]);
        assert_eq!(tree.total_size(), 3);
        assert!(tree.dirs.contains(&"empty".to_string()));
    }

    #[tokio::test]
    async fn single_file_prefix_gives_one_file() {
        let op = memory_with(&[("data/a.txt", "abc")]).await;
        let tree = list_tree(&op, "data/a.txt").await.unwrap();
        assert_eq!(tree.files.len(), 1);
        assert_eq!(tree.files[0].relative, "a.txt");
        assert_eq!(tree.files[0].key, "data/a.txt");
        assert_eq!(tree.base, "data/");
    }

    #[tokio::test]
    async fn missing_prefix_is_empty() {
        let op = memory_with(&[]).await;
        let tree = list_tree(&op, "nothing/here").await.unwrap();
        assert!(tree.files.is_empty());
    }

    #[tokio::test]
    async fn download_tree_writes_files_and_dirs() {
        let op = memory_with(&[
            ("p/a.txt", "hello"),
            ("p/x/y/b.txt", "world"),
            ("p/empty/", ""),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let tree = list_tree(&op, "p").await.unwrap();
        let budget = ByteBudget::new(Some(100));
        download_tree(&op, &tree, dir.path(), &budget)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x/y/b.txt")).unwrap(),
            "world"
        );
        assert!(dir.path().join("empty").is_dir());
        assert_eq!(budget.used(), 10);
    }

    #[tokio::test]
    async fn download_tree_respects_limit() {
        let op = memory_with(&[("p/a.txt", "0123456789")]).await;
        let dir = tempfile::tempdir().unwrap();
        let tree = list_tree(&op, "p").await.unwrap();
        let err = download_tree(&op, &tree, dir.path(), &ByteBudget::new(Some(5)))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
    }

    #[tokio::test]
    async fn a_failed_tree_download_returns_only_after_started_files_finished() {
        let big = "x".repeat(32 << 20);
        let op = memory_with(&[("p/big", &big), ("p/fails", "y")]).await;
        let dir = tempfile::tempdir().unwrap();
        let mut tree = list_tree(&op, "p").await.unwrap();
        // The big file starts first.
        tree.files.sort_by(|a, b| a.relative.cmp(&b.relative));
        // A folder in the way makes the small file fail at once.
        std::fs::create_dir(dir.path().join("fails")).unwrap();
        let err = download_tree(&op, &tree, dir.path(), &ByteBudget::new(None)).await;
        assert!(err.is_err());
        // The big download started together with the failing one and was not
        // cut off: nothing is still writing into the folder.
        let size = std::fs::metadata(dir.path().join("big")).map(|m| m.len());
        assert_eq!(size.unwrap(), big.len() as u64);
    }

    #[tokio::test]
    async fn keys_that_storage_would_change_are_not_read() {
        let op = memory_with(&[("x", "content of x")]).await;
        let dir = tempfile::tempdir().unwrap();
        for path in ["x ", "a//x", "./x", "x /"] {
            let err = list_tree(&op, path).await.unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{path:?}");
        }
        let err = download_file(
            &op,
            "x ",
            &dir.path().join("x "),
            &ByteBudget::default(),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(!dir.path().join("x ").exists());
    }

    #[tokio::test]
    async fn download_file_hashes_content() {
        let op = memory_with(&[("f", "abc")]).await;
        let dir = tempfile::tempdir().unwrap();
        let out = download_file(
            &op,
            "f",
            &dir.path().join("f"),
            &ByteBudget::default(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(out.size, 3);
        assert_eq!(
            out.sha256.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }
}
