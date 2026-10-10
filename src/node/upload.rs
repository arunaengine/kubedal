//! Upload local volume content to object storage.
//!
//! Symlinks and special files are never followed or uploaded, so a pod cannot make
//! kubedal read files outside of its volume.

use crate::node::paths;
use crate::util::error::{io_status, opendal_status};
use bytes::Bytes;
use futures::TryStreamExt;
use opendal::Operator;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tonic::Status;

/// Size of one upload part and of one read from disk.
const CHUNK_SIZE: usize = 8 * 1024 * 1024;

/// Size and modification time of a local file. Directories have no stamp values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStamp {
    pub size: u64,
    pub mtime_ns: i128,
    #[serde(default)]
    pub dir: bool,
}

/// The state of a volume right after it was prepared, used to find changed files.
pub type Baseline = BTreeMap<String, FileStamp>;

/// Walk a directory without following symlinks.
/// Returns regular files and directories with their stamps.
pub fn snapshot(root: &Path) -> std::io::Result<Baseline> {
    let mut out = Baseline::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                tracing::warn!("skipping file with a non UTF-8 name in {dir:?}");
                continue;
            };
            let relative = format!("{prefix}{name}");
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                out.insert(
                    relative.clone(),
                    FileStamp {
                        size: 0,
                        mtime_ns: 0,
                        dir: true,
                    },
                );
                stack.push((entry.path(), format!("{relative}/")));
            } else if file_type.is_file() {
                let meta = entry.metadata()?;
                out.insert(
                    relative,
                    FileStamp {
                        size: meta.len(),
                        mtime_ns: i128::from(meta.mtime()) * 1_000_000_000
                            + i128::from(meta.mtime_nsec()),
                        dir: false,
                    },
                );
            }
        }
    }
    Ok(out)
}

pub fn save_baseline(path: &Path, baseline: &Baseline) -> Result<(), Status> {
    let data = serde_json::to_vec(baseline)
        .map_err(|e| Status::internal(format!("encode baseline: {e}")))?;
    std::fs::write(path, data).map_err(|e| io_status(e, "write baseline"))
}

pub fn load_baseline(path: &Path) -> Result<Option<Baseline>, Status> {
    match std::fs::read(path) {
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(|e| Status::internal(format!("decode baseline {path:?}: {e}"))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io_status(err, "read baseline")),
    }
}

/// How often and how patiently to retry a failed file upload.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub attempts: u32,
    pub first_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            first_delay: Duration::from_secs(1),
        }
    }
}

/// What an upload did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadReport {
    pub files: usize,
    pub bytes: u64,
    pub skipped: usize,
}

/// Upload everything below `local` to `prefix` in the operator.
///
/// With a baseline, files with the same size and modification time as in the
/// baseline are skipped. Empty directories are created as directory objects.
/// Files that were deleted locally are not deleted in storage.
pub async fn upload_tree(
    op: &Operator,
    local: &Path,
    prefix: &str,
    baseline: Option<&Baseline>,
    retry: RetryPolicy,
) -> Result<UploadReport, Status> {
    let mut report = UploadReport::default();
    if !local.is_dir() {
        return Ok(report);
    }
    let root = local.to_path_buf();
    let current = tokio::task::spawn_blocking(move || snapshot(&root))
        .await
        .map_err(|e| Status::internal(format!("walk task failed: {e}")))?
        .map_err(|e| io_status(e, "walk volume content"))?;

    let mut non_empty_dirs = std::collections::HashSet::new();
    for path in current.keys() {
        let mut parent = path.as_str();
        while let Some((p, _)) = parent.rsplit_once('/') {
            non_empty_dirs.insert(p.to_string());
            parent = p;
        }
    }

    for (relative, stamp) in &current {
        if baseline.is_some_and(|b| b.get(relative) == Some(stamp)) {
            report.skipped += 1;
            continue;
        }
        let key = format!("{prefix}{relative}");
        if stamp.dir {
            if !non_empty_dirs.contains(relative) && op.info().capability().create_dir {
                with_retry(retry, &key, || async {
                    op.create_dir(&format!("{key}/"))
                        .await
                        .map_err(|e| opendal_status(e, &format!("create {key}/")))
                })
                .await?;
            }
            continue;
        }
        let path: PathBuf = local.join(relative);
        let bytes = with_retry(retry, &key, || upload_file(op, &path, &key)).await?;
        report.files += 1;
        report.bytes += bytes;
    }
    Ok(report)
}

async fn with_retry<T, F, Fut>(retry: RetryPolicy, key: &str, mut f: F) -> Result<T, Status>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, Status>>,
{
    let mut delay = retry.first_delay;
    let mut attempt = 1;
    loop {
        match f().await {
            Ok(value) => return Ok(value),
            Err(err) if attempt < retry.attempts.max(1) => {
                tracing::warn!(
                    "upload of {key} failed (attempt {attempt}): {}",
                    err.message()
                );
                tokio::time::sleep(delay).await;
                delay *= 2;
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Stream one local file to object storage. Returns the number of bytes.
pub async fn upload_file(op: &Operator, path: &Path, key: &str) -> Result<u64, Status> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| io_status(e, &format!("open {}", path.display())))?;
    let mut writer = op
        .writer_with(key)
        .chunk(CHUNK_SIZE)
        .await
        .map_err(|e| opendal_status(e, &format!("write {key}")))?;

    let mut total = 0u64;
    let result: Result<(), Status> = async {
        loop {
            let mut buf = Vec::with_capacity(CHUNK_SIZE);
            let n = (&mut file)
                .take(CHUNK_SIZE as u64)
                .read_to_end(&mut buf)
                .await
                .map_err(|e| io_status(e, &format!("read {}", path.display())))?;
            if n == 0 {
                return Ok(());
            }
            total += n as u64;
            writer
                .write(Bytes::from(buf))
                .await
                .map_err(|e| opendal_status(e, &format!("write {key}")))?;
        }
    }
    .await;

    if let Err(err) = result {
        let _ = writer.abort().await;
        return Err(err);
    }
    writer
        .close()
        .await
        .map_err(|e| opendal_status(e, &format!("finish {key}")))?;
    Ok(total)
}

/// List the keys below a prefix, for tests and checks.
pub async fn list_keys(op: &Operator, prefix: &str) -> Result<Vec<String>, Status> {
    let mut keys: Vec<String> = op
        .lister_with(prefix)
        .recursive(true)
        .await
        .map_err(|e| opendal_status(e, "list"))?
        .map_ok(|e| e.path().to_string())
        .try_collect()
        .await
        .map_err(|e| opendal_status(e, "list"))?;
    keys.sort();
    Ok(keys)
}

/// Check that a folder inside a volume is a safe relative path ("" is the whole volume).
pub fn local_folder(volume: &Path, folder: &str) -> Result<PathBuf, Status> {
    if folder.trim_matches('/').is_empty() {
        return Ok(volume.to_path_buf());
    }
    paths::join_safe(volume, folder)
        .map_err(|e| Status::invalid_argument(format!("invalid upload folder {folder:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendal::services::Memory;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn fast() -> RetryPolicy {
        RetryPolicy {
            attempts: 2,
            first_delay: Duration::from_millis(1),
        }
    }

    #[test]
    fn snapshot_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.txt"), "a");
        write(&dir.path().join("sub/b.txt"), "bb");
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("link")).unwrap();
        let snap = snapshot(dir.path()).unwrap();
        let keys: Vec<_> = snap.keys().cloned().collect();
        assert_eq!(keys, vec!["a.txt", "sub", "sub/b.txt"]);
        assert_eq!(snap["sub/b.txt"].size, 2);
        assert!(snap["sub"].dir);
    }

    #[tokio::test]
    async fn uploads_all_files_without_baseline() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.txt"), "a");
        write(&dir.path().join("sub/b.txt"), "bb");
        std::fs::create_dir_all(dir.path().join("empty")).unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let report = upload_tree(&op, dir.path(), "out/", None, fast())
            .await
            .unwrap();
        assert_eq!(report.files, 2);
        assert_eq!(report.bytes, 3);
        assert_eq!(
            list_keys(&op, "out/").await.unwrap(),
            vec!["out/a.txt", "out/empty/", "out/sub/b.txt"]
        );
        assert_eq!(op.read("out/sub/b.txt").await.unwrap().to_vec(), b"bb");
    }

    #[tokio::test]
    async fn baseline_skips_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("same.txt"), "same");
        write(&dir.path().join("changed.txt"), "old");
        let baseline = snapshot(dir.path()).unwrap();
        // Change content and size, and add a new file.
        write(&dir.path().join("changed.txt"), "newer");
        write(&dir.path().join("new.txt"), "n");
        let op = Operator::new(Memory::default()).unwrap();
        let report = upload_tree(&op, dir.path(), "", Some(&baseline), fast())
            .await
            .unwrap();
        assert_eq!(report.files, 2);
        assert_eq!(report.skipped, 1);
        assert!(op.exists("changed.txt").await.unwrap());
        assert!(op.exists("new.txt").await.unwrap());
        assert!(!op.exists("same.txt").await.unwrap());
    }

    #[tokio::test]
    async fn upload_streams_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let big = vec![7u8; CHUNK_SIZE + 123];
        std::fs::write(dir.path().join("big.bin"), &big).unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let n = upload_file(&op, &dir.path().join("big.bin"), "big.bin")
            .await
            .unwrap();
        assert_eq!(n, big.len() as u64);
        assert_eq!(op.read("big.bin").await.unwrap().to_vec(), big);
    }

    #[test]
    fn baseline_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("data/a"), "a");
        let snap = snapshot(&dir.path().join("data")).unwrap();
        let file = dir.path().join("baseline.json");
        save_baseline(&file, &snap).unwrap();
        assert_eq!(load_baseline(&file).unwrap(), Some(snap));
        assert_eq!(load_baseline(&dir.path().join("none")).unwrap(), None);
    }

    #[test]
    fn local_folder_stays_in_volume() {
        let vol = Path::new("/v");
        assert_eq!(local_folder(vol, "").unwrap(), PathBuf::from("/v"));
        assert_eq!(local_folder(vol, "out/").unwrap(), PathBuf::from("/v/out"));
        assert!(local_folder(vol, "../x").is_err());
    }
}
