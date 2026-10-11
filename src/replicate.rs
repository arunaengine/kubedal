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
//! Files deleted in the source are not deleted in the replicas.

use std::collections::HashMap;
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
    let meta = std::fs::metadata(source.join(path))?;
    let target = replica.join(path);
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

/// Copy `from` to a temporary file next to `to`, then rename it over `to`.
/// A copy that fails midway never replaces a good file; the temporary file is
/// removed.
fn copy_file(from: &Path, to: &Path) -> std::io::Result<()> {
    let name = to.file_name().unwrap_or_default().to_string_lossy();
    let temp = to.with_file_name(format!(".{name}.kubedal-copy"));
    let result = (|| {
        let mut input = std::fs::File::open(from)?;
        let mut output = std::fs::File::create(&temp)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        drop(output);
        std::fs::rename(&temp, to)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// True if both files have the same bytes.
fn same_content(a: &Path, b: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let (mut a, mut b) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
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
