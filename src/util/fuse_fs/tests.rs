//! Tests that call the file system operations directly, without a kernel mount.

use super::*;
use opendal::services::Memory;

fn req() -> Request {
    Request {
        unique: 1,
        uid: 0,
        gid: 0,
        pid: 0,
    }
}

async fn fs_with(path: &str, content: &str) -> Filesystem {
    let op = Operator::new(Memory::default()).unwrap();
    op.write(path, content.to_string()).await.unwrap();
    Filesystem::new(op, 0, 0, std::env::temp_dir())
}

fn size(size: u64) -> SetAttr {
    SetAttr {
        size: Some(size),
        ..Default::default()
    }
}

#[tokio::test]
async fn truncate_to_zero_empties_the_file() {
    let fs = fs_with("a.txt", "hello").await;
    let attr = fs
        .setattr(req(), Some(OsStr::new("/a.txt")), None, size(0))
        .await
        .unwrap();
    assert_eq!(attr.attr.size, 0);
    assert!(fs.op.read("a.txt").await.unwrap().is_empty());
}

#[tokio::test]
async fn truncate_to_other_sizes_is_refused() {
    let fs = fs_with("a.txt", "hello").await;
    let err = fs
        .setattr(req(), Some(OsStr::new("/a.txt")), None, size(2))
        .await
        .unwrap_err();
    assert_eq!(err, Errno::from(libc::EOPNOTSUPP));
    assert_eq!(fs.op.read("a.txt").await.unwrap().to_vec(), b"hello");
    // The current size is fine.
    fs.setattr(req(), Some(OsStr::new("/a.txt")), None, size(5))
        .await
        .unwrap();
}

const WRITE_FLAGS: u32 = (libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC) as u32;

async fn create(fs: &Filesystem, name: &str) -> u64 {
    fs.create(req(), OsStr::new("/"), OsStr::new(name), 0o644, WRITE_FLAGS)
        .await
        .unwrap()
        .fh
}

#[tokio::test]
async fn flush_uploads_and_later_writes_are_uploaded_too() {
    let fs = fs_with("other", "").await;
    let fh = create(&fs, "f.txt").await;
    let path = Some(OsStr::new("/f.txt"));
    // A shell redirect closes a duplicated descriptor before it writes.
    fs.flush(req(), path, fh, 0).await.unwrap();
    fs.write(req(), path, fh, 0, b"hello", 0, 0).await.unwrap();
    fs.flush(req(), path, fh, 0).await.unwrap();
    assert_eq!(fs.op.read("f.txt").await.unwrap().to_vec(), b"hello");
    fs.release(req(), path, fh, 0, 0, false).await.unwrap();
}

#[tokio::test]
async fn upload_errors_reach_flush_and_fsync() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let op = Operator::new(opendal::services::Fs::default().root(&root.to_string_lossy())).unwrap();
    let fs = Filesystem::new(op, 0, 0, dir.path().join("spool"));
    let fh = create(&fs, "f.txt").await;
    let path = Some(OsStr::new("/f.txt"));
    fs.write(req(), path, fh, 0, b"hello", 0, 0).await.unwrap();

    // The backend breaks: its root folder is now a file.
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::write(&root, "not a folder").unwrap();
    assert_eq!(
        fs.flush(req(), path, fh, 0).await.unwrap_err(),
        Errno::from(libc::EIO)
    );
    assert_eq!(
        fs.fsync(req(), path, fh, false).await.unwrap_err(),
        Errno::from(libc::EIO)
    );
}

#[tokio::test]
async fn open_files_can_be_truncated() {
    let fs = fs_with("other", "").await;
    let fh = create(&fs, "f.txt").await;
    let path = Some(OsStr::new("/f.txt"));
    fs.write(req(), path, fh, 0, b"hello", 0, 0).await.unwrap();
    fs.setattr(req(), path, Some(fh), size(2)).await.unwrap();
    fs.flush(req(), path, fh, 0).await.unwrap();
    assert_eq!(fs.op.read("f.txt").await.unwrap().to_vec(), b"he");
}
