//! Upload local volume content to object storage.
//!
//! Symlinks and special files are never followed or uploaded, so a pod cannot make
//! kubedal read files outside of its volume.

use crate::node::paths;
use crate::util::error::{io_status, opendal_status};
use bytes::Bytes;
use futures::TryStreamExt;
use opendal::Operator;
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, openat, statat};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tonic::Status;

/// Size of one upload part and of one read from disk.
const CHUNK_SIZE: usize = 8 * 1024 * 1024;

/// Size, times and inode of a local file. Directories have no stamp values.
///
/// A program can set the modification time back, but not the change time
/// (ctime), so a file only counts as unchanged if all values match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStamp {
    pub size: u64,
    pub mtime_ns: i128,
    #[serde(default)]
    pub ctime_ns: i128,
    #[serde(default)]
    pub inode: u64,
    #[serde(default)]
    pub dir: bool,
}

/// The state of a volume right after it was prepared, used to find changed files.
pub type Baseline = BTreeMap<String, FileStamp>;

fn open_dir_at<Fd: AsFd>(dir: Fd, name: &str) -> std::io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    Ok(openat(dir, name, flags, Mode::empty())?)
}

/// Open the folder `folder` inside `volume` without following any symlink in
/// `folder`. `volume` itself is a directory created by kubedal.
/// Returns `None` if the folder does not exist.
pub fn open_upload_root(volume: &Path, folder: &str) -> Result<Option<OwnedFd>, Status> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut dir = match openat(CWD, volume, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(err) => return Err(io_status(err.into(), "open volume folder")),
    };
    if !folder.trim_matches('/').is_empty() {
        let folder = paths::safe_relative(folder).map_err(|e| {
            Status::invalid_argument(format!("invalid upload folder {folder:?}: {e}"))
        })?;
        for part in folder.split('/') {
            dir = match open_dir_at(&dir, part) {
                Ok(fd) => fd,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => {
                    return Err(Status::permission_denied(format!(
                        "upload folder {folder:?} is not a plain folder (a symlink or a file)"
                    )));
                }
            };
        }
    }
    Ok(Some(dir))
}

/// Walk a directory without following symlinks.
/// Returns regular files and directories with their stamps.
pub fn snapshot(root: &Path) -> std::io::Result<Baseline> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let fd = openat(CWD, root, flags, Mode::empty())?;
    snapshot_fd(&fd)
}

/// Like [`snapshot`], on an open directory. Every entry is opened relative to its
/// parent with `O_NOFOLLOW`, so a symlink can never lead out of the tree.
pub fn snapshot_fd(root: &OwnedFd) -> std::io::Result<Baseline> {
    let mut out = Baseline::new();
    let mut stack = vec![(root.try_clone()?, String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for entry in Dir::read_from(&dir)? {
            let entry = entry?;
            let name_bytes = entry.file_name().to_bytes();
            if name_bytes == b"." || name_bytes == b".." {
                continue;
            }
            // Object keys are UTF-8. Fail instead of skipping, so the data is kept.
            let Ok(name) = std::str::from_utf8(name_bytes).map(str::to_string) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "file name {:?} in folder {prefix:?} is not UTF-8 and cannot be uploaded",
                        String::from_utf8_lossy(name_bytes)
                    ),
                ));
            };
            let relative = format!("{prefix}{name}");
            let stat = statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)?;
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => {
                    out.insert(
                        relative.clone(),
                        FileStamp {
                            size: 0,
                            mtime_ns: 0,
                            ctime_ns: 0,
                            inode: 0,
                            dir: true,
                        },
                    );
                    let child = open_dir_at(&dir, &name)?;
                    stack.push((child, format!("{relative}/")));
                }
                FileType::RegularFile => {
                    out.insert(
                        relative,
                        FileStamp {
                            size: stat.st_size as u64,
                            mtime_ns: i128::from(stat.st_mtime) * 1_000_000_000
                                + i128::from(stat.st_mtime_nsec),
                            ctime_ns: i128::from(stat.st_ctime) * 1_000_000_000
                                + i128::from(stat.st_ctime_nsec),
                            inode: stat.st_ino,
                            dir: false,
                        },
                    );
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

/// Open a regular file below `root` without following symlinks.
fn open_file_in(root: &OwnedFd, relative: &str) -> std::io::Result<std::fs::File> {
    let mut parts: Vec<&str> = relative.split('/').collect();
    let name = parts.pop().unwrap_or_default();
    let mut dir = root.try_clone()?;
    for part in parts {
        dir = open_dir_at(&dir, part)?;
    }
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let fd = openat(&dir, name, flags, Mode::empty())?;
    if FileType::from_raw_mode(rustix::fs::fstat(&fd)?.st_mode) != FileType::RegularFile {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(std::fs::File::from(fd))
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

/// Upload everything below `folder` in `volume` to `prefix` in the operator.
///
/// With a baseline, files whose stamp (size, times and inode) did not change are
/// skipped. Empty directories are created as directory objects.
/// Files that were deleted locally are not deleted in storage.
pub async fn upload_tree(
    op: &Operator,
    volume: &Path,
    folder: &str,
    prefix: &str,
    baseline: Option<&Baseline>,
    retry: RetryPolicy,
) -> Result<UploadReport, Status> {
    let mut report = UploadReport::default();
    let Some(root) = open_upload_root(volume, folder)? else {
        return Ok(report);
    };
    let root = std::sync::Arc::new(root);
    let walk_root = root.clone();
    let current = tokio::task::spawn_blocking(move || snapshot_fd(&walk_root))
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
        let bytes = with_retry(retry, &key, || async {
            let file = open_file_in(&root, relative)
                .map_err(|e| io_status(e, &format!("open {relative}")))?;
            upload_file(op, tokio::fs::File::from_std(file), relative, &key).await
        })
        .await?;
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

/// Stream one open local file to object storage. Returns the number of bytes.
/// `name` is only used in error messages.
pub async fn upload_file(
    op: &Operator,
    mut file: tokio::fs::File,
    name: &str,
    key: &str,
) -> Result<u64, Status> {
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
                .map_err(|e| io_status(e, &format!("read {name}")))?;
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
        let report = upload_tree(&op, dir.path(), "", "out/", None, fast())
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
        let report = upload_tree(&op, dir.path(), "", "", Some(&baseline), fast())
            .await
            .unwrap();
        assert_eq!(report.files, 2);
        assert_eq!(report.skipped, 1);
        assert!(op.exists("changed.txt").await.unwrap());
        assert!(op.exists("new.txt").await.unwrap());
        assert!(!op.exists("same.txt").await.unwrap());
    }

    #[tokio::test]
    async fn same_size_and_old_mtime_still_counts_as_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        write(&path, "aaaa");
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let baseline = snapshot(dir.path()).unwrap();
        // Replace the bytes with the same size and set the old mtime again.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&path, "bbbb").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let report = upload_tree(&op, dir.path(), "", "", Some(&baseline), fast())
            .await
            .unwrap();
        assert_eq!(report.files, 1);
        assert_eq!(op.read("f.txt").await.unwrap().to_vec(), b"bbbb");
    }

    #[tokio::test]
    async fn upload_streams_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let big = vec![7u8; CHUNK_SIZE + 123];
        std::fs::write(dir.path().join("big.bin"), &big).unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let file = tokio::fs::File::open(dir.path().join("big.bin"))
            .await
            .unwrap();
        let n = upload_file(&op, file, "big.bin", "big.bin").await.unwrap();
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
    fn upload_root_stays_in_volume() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("out")).unwrap();
        assert!(open_upload_root(dir.path(), "").unwrap().is_some());
        assert!(open_upload_root(dir.path(), "out/").unwrap().is_some());
        assert!(open_upload_root(dir.path(), "missing").unwrap().is_none());
        assert!(open_upload_root(dir.path(), "../x").is_err());
    }

    #[tokio::test]
    async fn non_utf8_names_fail_the_upload() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let name = std::ffi::OsStr::from_bytes(b"bad\xffname");
        std::fs::write(dir.path().join(name), "x").unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        assert!(
            upload_tree(&op, dir.path(), "", "", None, fast())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn symlinked_upload_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let secret = tempfile::tempdir().unwrap();
        write(&secret.path().join("token"), "secret");
        std::os::unix::fs::symlink(secret.path(), dir.path().join("out")).unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let err = upload_tree(&op, dir.path(), "out", "", None, fast())
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(list_keys(&op, "").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn symlinked_parent_of_upload_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let secret = tempfile::tempdir().unwrap();
        write(&secret.path().join("out/token"), "secret");
        std::os::unix::fs::symlink(secret.path(), dir.path().join("work")).unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let err = upload_tree(&op, dir.path(), "work/out", "", None, fast())
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(list_keys(&op, "").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn symlinks_inside_the_tree_are_not_uploaded() {
        let dir = tempfile::tempdir().unwrap();
        let secret = tempfile::tempdir().unwrap();
        write(&secret.path().join("token"), "secret");
        write(&dir.path().join("a.txt"), "a");
        std::os::unix::fs::symlink(secret.path(), dir.path().join("linkdir")).unwrap();
        std::os::unix::fs::symlink(secret.path().join("token"), dir.path().join("link")).unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        upload_tree(&op, dir.path(), "", "", None, fast())
            .await
            .unwrap();
        assert_eq!(list_keys(&op, "").await.unwrap(), vec!["a.txt"]);
    }
}
