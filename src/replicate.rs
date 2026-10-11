//! The `kubedal replicate` command, run in the sync pod of a DataReplicaSet.
//!
//! The controller mounts the source PVC and the replica PVCs into the sync pod
//! and sets these environment variables:
//!
//! - `source`: folder of the source mount
//! - `replicas`: folders of the replica mounts, separated by `;`
//! - `sleep_duration`: seconds to wait between two passes
//!
//! Each pass copies new and changed files from the source to every replica.
//! Files deleted in the source are not deleted in the replicas. Symlinks are
//! never followed, so nothing outside the source is copied.

use std::collections::HashMap;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// What the source file looked like when it was last copied to a replica.
type Copied = HashMap<PathBuf, (u64, Option<SystemTime>)>;

/// Read the environment and copy the source to the replicas until the process
/// is stopped.
pub fn run_from_env() -> Result<(), String> {
    let var = |name: &str| std::env::var(name).map_err(|e| format!("variable {name}: {e}"));
    let source = PathBuf::from(var("source")?);
    let replicas: Vec<PathBuf> = var("replicas")?
        .split(';')
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .collect();
    let sleep = var("sleep_duration")?
        .parse::<u64>()
        .map_err(|e| format!("variable sleep_duration: {e}"))?;
    tracing::info!("replicating {} to {replicas:?}", source.display());
    // A sync pod that was killed in the middle of a copy can leave a temporary
    // file behind. A running pod removes its own on failure.
    for replica in &replicas {
        remove_stale_temps(replica);
    }
    let mut copied: Vec<Copied> = vec![Copied::new(); replicas.len()];
    loop {
        for (replica, copied) in replicas.iter().zip(copied.iter_mut()) {
            sync_once(&source, replica, copied);
        }
        std::thread::sleep(Duration::from_secs(sleep));
    }
}

/// Copy new and changed files from `source` to `replica` once. Errors are
/// logged, and the file is tried again in the next pass.
///
/// A file is copied when it is missing in the replica, has another size there,
/// or changed in the source since the last copy of this process. Without a
/// record of an earlier copy (the first pass), the content is compared.
pub fn sync_once(source: &Path, replica: &Path, copied: &mut Copied) {
    let mut folders = vec![PathBuf::new()];
    while let Some(relative) = folders.pop() {
        let entries = match std::fs::read_dir(source.join(&relative)) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::warn!("cannot list {}: {err}", source.join(&relative).display());
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    tracing::warn!("cannot list {}: {err}", source.join(&relative).display());
                    continue;
                }
            };
            let path = relative.join(entry.file_name());
            if let Err(err) = sync_entry(source, replica, &path, copied, &mut folders) {
                tracing::warn!("cannot copy {}: {err}", path.display());
            }
        }
    }
}

fn sync_entry(
    source: &Path,
    replica: &Path,
    path: &Path,
    copied: &mut Copied,
    folders: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(source.join(path))?;
    let target = replica.join(path);
    if meta.file_type().is_symlink() {
        tracing::warn!("skip symlink {}", path.display());
        return Ok(());
    }
    if meta.is_file() && is_temp_name(path) {
        tracing::warn!(
            "skip {}: it has the name of a temporary copy",
            path.display()
        );
        return Ok(());
    }
    if meta.is_dir() {
        std::fs::create_dir_all(&target)?;
        folders.push(path.to_path_buf());
        return Ok(());
    }
    if !meta.is_file() {
        return Ok(());
    }
    let seen = (meta.len(), meta.modified().ok());
    let same_size = std::fs::metadata(&target).is_ok_and(|t| t.len() == meta.len());
    let changed = match copied.get(path) {
        Some(last) => *last != seen,
        // The source may have changed while no sync pod ran.
        None => same_size && !same_content(&source.join(path), &target)?,
    };
    if !same_size || changed {
        tracing::info!("copy {}", path.display());
        copy_file(&source.join(path), &target)?;
    }
    copied.insert(path.to_path_buf(), seen);
    Ok(())
}

/// Start of the names of temporary copy files, followed by 32 hex digits.
const TEMP_PREFIX: &str = ".kdc-";

/// True if the file name of `path` is exactly that of a temporary copy file.
fn is_temp_name(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.strip_prefix(TEMP_PREFIX).is_some_and(|hex| {
        hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Remove the temporary copy files below `replica`. Symlinks are not followed.
fn remove_stale_temps(replica: &Path) {
    let mut folders = vec![replica.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let entries = match std::fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::warn!("cannot list {}: {err}", folder.display());
                continue;
            }
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => folders.push(path),
                Ok(kind) if kind.is_file() && is_temp_name(&path) => {
                    tracing::info!("remove stale temporary file {}", path.display());
                    if let Err(err) = std::fs::remove_file(&path) {
                        tracing::warn!("cannot remove {}: {err}", path.display());
                    }
                }
                _ => {}
            }
        }
    }
}

/// A new, short and unique name for a temporary copy file.
fn temp_name() -> String {
    format!("{TEMP_PREFIX}{}", uuid::Uuid::new_v4().simple())
}

/// Copy `from` to a temporary file next to `to`, then rename it over `to`.
/// A copy that fails midway never replaces a good file; the temporary file is
/// removed.
fn copy_file(from: &Path, to: &Path) -> std::io::Result<()> {
    copy_file_via(from, to, &to.with_file_name(temp_name()))
}

/// Like [`copy_file`] with the temporary file `temp`. It must not exist yet:
/// only a file that this copy created is ever removed.
fn copy_file_via(from: &Path, to: &Path, temp: &Path) -> std::io::Result<()> {
    let mut input = open_no_follow(from)?;
    let mut output = std::fs::File::options()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(temp)?;
    let result = (|| {
        std::io::copy(&mut input, &mut output)?;
        // On kubedal FUSE volumes, fsync and close upload the file, so their
        // errors must count.
        output.sync_all()?;
        close_file(output)?;
        std::fs::rename(temp, to)?;
        // On a kubedal FUSE volume, the kernel may send the release of the
        // temporary file after the rename, so the renamed content may not be
        // stored under its new name yet. An fsync of the new name uploads it.
        let file = std::fs::File::options()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(to)?;
        file.sync_all()?;
        close_file(file)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

/// Open a file for reading, but fail if it is a symlink.
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Close a file and report the error of `close`, which dropping a file hides.
fn close_file(file: std::fs::File) -> std::io::Result<()> {
    use std::os::fd::IntoRawFd;
    // SAFETY: the descriptor comes from `into_raw_fd`, so nothing else owns or
    // closes it.
    if unsafe { libc::close(file.into_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// True if both files have the same bytes.
fn same_content(a: &Path, b: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let (mut a, mut b) = (open_no_follow(a)?, open_no_follow(b)?);
    let (mut buf_a, mut buf_b) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let n = a.read(&mut buf_a)?;
        if n == 0 {
            return Ok(b.read(&mut buf_b[..1])? == 0);
        }
        match b.read_exact(&mut buf_b[..n]) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(err) => return Err(err),
        }
        if buf_a[..n] != buf_b[..n] {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_changed_files_are_copied() {
        let dir = tempfile::tempdir().unwrap();
        let (source, replica) = (dir.path().join("source"), dir.path().join("replica"));
        std::fs::create_dir_all(source.join("a/b")).unwrap();
        std::fs::create_dir_all(&replica).unwrap();
        std::fs::write(source.join("top.txt"), "one").unwrap();
        std::fs::write(source.join("a/b/deep.txt"), "deep").unwrap();
        let mut copied = Copied::new();

        sync_once(&source, &replica, &mut copied);
        assert_eq!(std::fs::read(replica.join("top.txt")).unwrap(), b"one");
        assert_eq!(
            std::fs::read(replica.join("a/b/deep.txt")).unwrap(),
            b"deep"
        );

        // Same size, but a new modification time: copied again.
        std::fs::write(source.join("top.txt"), "two").unwrap();
        let later = SystemTime::now() + Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(source.join("top.txt"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        sync_once(&source, &replica, &mut copied);
        assert_eq!(std::fs::read(replica.join("top.txt")).unwrap(), b"two");

        // Files deleted in the source stay in the replica.
        std::fs::remove_file(source.join("a/b/deep.txt")).unwrap();
        sync_once(&source, &replica, &mut copied);
        assert!(replica.join("a/b/deep.txt").exists());
    }

    #[test]
    fn a_file_of_the_same_size_but_other_content_is_replaced_on_the_first_pass() {
        let dir = tempfile::tempdir().unwrap();
        let (source, replica) = (dir.path().join("source"), dir.path().join("replica"));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&replica).unwrap();
        // The source changed from "one" to "two" while no sync pod ran.
        std::fs::write(source.join("f"), "two").unwrap();
        std::fs::write(replica.join("f"), "one").unwrap();
        std::fs::write(source.join("same"), "abc").unwrap();
        std::fs::write(replica.join("same"), "abc").unwrap();
        let mut copied = Copied::new();
        sync_once(&source, &replica, &mut copied);
        assert_eq!(std::fs::read(replica.join("f")).unwrap(), b"two");
        assert!(copied.contains_key(Path::new("same")));
    }

    #[test]
    fn a_copy_that_fails_midway_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("f");
        std::fs::write(&target, "good").unwrap();
        // Opening a folder works, but reading it fails.
        assert!(copy_file(dir.path(), &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"good");
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn a_failed_copy_is_not_recorded_and_tried_again() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (source, replica) = (dir.path().join("source"), dir.path().join("replica"));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&replica).unwrap();
        std::fs::write(source.join("f"), "data").unwrap();
        let mode = |m| std::fs::Permissions::from_mode(m);
        std::fs::set_permissions(&replica, mode(0o555)).unwrap();
        let mut copied = Copied::new();
        sync_once(&source, &replica, &mut copied);
        assert!(copied.is_empty());
        std::fs::set_permissions(&replica, mode(0o755)).unwrap();
        sync_once(&source, &replica, &mut copied);
        assert_eq!(std::fs::read(replica.join("f")).unwrap(), b"data");
        assert!(copied.contains_key(Path::new("f")));
    }

    #[test]
    fn symlinks_are_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let (source, replica) = (dir.path().join("source"), dir.path().join("replica"));
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&replica).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "secret").unwrap();
        std::fs::write(source.join("f"), "data").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), source.join("file-link")).unwrap();
        std::os::unix::fs::symlink(&outside, source.join("dir-link")).unwrap();
        sync_once(&source, &replica, &mut Copied::new());
        assert_eq!(std::fs::read(replica.join("f")).unwrap(), b"data");
        assert!(!replica.join("file-link").exists());
        assert!(!replica.join("dir-link").exists());
        // A symlink given as a file to copy is refused too.
        assert!(copy_file(&source.join("file-link"), &replica.join("x")).is_err());
        assert!(!replica.join("x").exists());
    }

    #[test]
    fn a_copy_never_touches_a_file_with_the_temporary_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("src"), "new").unwrap();
        std::fs::write(dir.path().join("f"), "old").unwrap();
        let temp = dir.path().join(temp_name());
        std::fs::write(&temp, "keep").unwrap();
        assert!(copy_file_via(&dir.path().join("src"), &dir.path().join("f"), &temp).is_err());
        assert_eq!(std::fs::read(&temp).unwrap(), b"keep");
        assert_eq!(std::fs::read(dir.path().join("f")).unwrap(), b"old");
    }

    #[test]
    fn files_named_like_old_temporary_files_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (source, replica) = (dir.path().join("source"), dir.path().join("replica"));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&replica).unwrap();
        std::fs::write(source.join("f"), "one").unwrap();
        std::fs::write(source.join(".f.kubedal-copy"), "real data").unwrap();
        let mut copied = Copied::new();
        sync_once(&source, &replica, &mut copied);
        std::fs::remove_file(source.join(".f.kubedal-copy")).unwrap();
        std::fs::write(source.join("f"), "changed").unwrap();
        sync_once(&source, &replica, &mut copied);
        assert_eq!(std::fs::read(replica.join("f")).unwrap(), b"changed");
        assert_eq!(
            std::fs::read(replica.join(".f.kubedal-copy")).unwrap(),
            b"real data"
        );
    }

    #[test]
    fn a_file_with_a_long_name_is_copied() {
        let dir = tempfile::tempdir().unwrap();
        let (source, replica) = (dir.path().join("source"), dir.path().join("replica"));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&replica).unwrap();
        let name = "n".repeat(250);
        std::fs::write(source.join(&name), "data").unwrap();
        sync_once(&source, &replica, &mut Copied::new());
        assert_eq!(std::fs::read(replica.join(&name)).unwrap(), b"data");
    }

    #[test]
    fn stale_temporary_files_are_removed_and_never_replicated() {
        let dir = tempfile::tempdir().unwrap();
        let (source, replica) = (dir.path().join("source"), dir.path().join("replica"));
        std::fs::create_dir_all(source.join("sub")).unwrap();
        std::fs::create_dir_all(replica.join("sub")).unwrap();
        let temp = temp_name();
        std::fs::write(replica.join("sub").join(&temp), "half").unwrap();
        // Names that only look similar stay.
        std::fs::write(replica.join("sub/.kdc-1234"), "keep").unwrap();
        std::fs::write(replica.join(format!("{temp}x")), "keep").unwrap();
        remove_stale_temps(&replica);
        assert!(!replica.join("sub").join(&temp).exists());
        assert!(replica.join("sub/.kdc-1234").exists());
        assert!(replica.join(format!("{temp}x")).exists());

        std::fs::write(source.join(&temp), "source").unwrap();
        sync_once(&source, &replica, &mut Copied::new());
        assert!(!replica.join(&temp).exists());
    }

    #[test]
    fn a_file_of_another_size_is_replaced_on_the_first_pass() {
        let dir = tempfile::tempdir().unwrap();
        let (source, replica) = (dir.path().join("source"), dir.path().join("replica"));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&replica).unwrap();
        std::fs::write(source.join("f"), "new content").unwrap();
        std::fs::write(replica.join("f"), "old").unwrap();
        sync_once(&source, &replica, &mut Copied::new());
        assert_eq!(std::fs::read(replica.join("f")).unwrap(), b"new content");
    }
}
