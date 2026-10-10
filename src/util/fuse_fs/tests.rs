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
    Filesystem::new(op, 0, 0, tempfile::tempdir().unwrap().keep())
}

async fn open(fs: &Filesystem, path: &str, flags: i32) -> u64 {
    fs.open(req(), OsStr::new(path), flags as u32)
        .await
        .unwrap()
        .fh
}

async fn read_all(fs: &Filesystem, path: &str, fh: u64) -> Vec<u8> {
    fs.read(req(), Some(OsStr::new(path)), fh, 0, 1 << 20)
        .await
        .unwrap()
        .data
        .to_vec()
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

const WRITE_FLAGS: u32 = (libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC) as u32;

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

#[tokio::test]
async fn two_append_handles_keep_both_writes() {
    let fs = fs_with("f.txt", "x").await;
    let p = Some(OsStr::new("/f.txt"));
    let a = open(&fs, "/f.txt", libc::O_WRONLY | libc::O_APPEND).await;
    let b = open(&fs, "/f.txt", libc::O_WRONLY | libc::O_APPEND).await;
    // Both handles think the file has 1 byte, like a kernel with a stale size.
    fs.write(req(), p, a, 1, b"A", 0, 0).await.unwrap();
    fs.write(req(), p, b, 1, b"B", 0, 0).await.unwrap();
    fs.flush(req(), p, a, 0).await.unwrap();
    fs.release(req(), p, a, 0, 0, false).await.unwrap();
    fs.flush(req(), p, b, 0).await.unwrap();
    fs.release(req(), p, b, 0, 0, false).await.unwrap();
    assert_eq!(fs.op.read("f.txt").await.unwrap().to_vec(), b"xAB");
}

#[tokio::test]
async fn reads_see_content_that_is_not_uploaded_yet() {
    let fs = fs_with("other", "").await;
    let w = create(&fs, "f.txt").await;
    let p = Some(OsStr::new("/f.txt"));
    fs.write(req(), p, w, 0, b"hello", 0, 0).await.unwrap();
    // The object does not exist yet, but lookup, getattr and reads see it.
    let entry = fs
        .lookup(req(), OsStr::new("/"), OsStr::new("f.txt"))
        .await
        .unwrap();
    assert_eq!(entry.attr.size, 5);
    let attr = fs.getattr(req(), p, None, 0).await.unwrap();
    assert_eq!(attr.attr.size, 5);
    let r = open(&fs, "/f.txt", libc::O_RDONLY).await;
    assert_eq!(read_all(&fs, "/f.txt", r).await, b"hello");
    assert_eq!(read_all(&fs, "/f.txt", w).await, b"hello");
}

#[tokio::test]
async fn path_truncate_changes_the_open_content() {
    let fs = fs_with("other", "").await;
    let w = create(&fs, "f.txt").await;
    let p = Some(OsStr::new("/f.txt"));
    fs.write(req(), p, w, 0, b"12345", 0, 0).await.unwrap();
    fs.setattr(req(), p, None, size(2)).await.unwrap();
    fs.flush(req(), p, w, 0).await.unwrap();
    assert_eq!(fs.op.read("f.txt").await.unwrap().to_vec(), b"12");
}

#[tokio::test]
async fn unlinked_files_are_not_created_again_by_open_handles() {
    let fs = fs_with("other", "").await;
    let w = create(&fs, "f.txt").await;
    let p = Some(OsStr::new("/f.txt"));
    fs.write(req(), p, w, 0, b"data", 0, 0).await.unwrap();
    fs.flush(req(), p, w, 0).await.unwrap();
    fs.unlink(req(), OsStr::new("/"), OsStr::new("f.txt"))
        .await
        .unwrap();
    // The handle keeps working, but nothing comes back under the old name.
    fs.write(req(), p, w, 4, b"more", 0, 0).await.unwrap();
    assert_eq!(read_all(&fs, "/f.txt", w).await, b"datamore");
    fs.flush(req(), p, w, 0).await.unwrap();
    fs.release(req(), p, w, 0, 0, false).await.unwrap();
    assert!(!fs.op.exists("f.txt").await.unwrap());
}

#[tokio::test]
async fn rename_moves_the_open_content() {
    let fs = fs_with("other", "").await;
    let w = create(&fs, "a.txt").await;
    let p = Some(OsStr::new("/a.txt"));
    fs.write(req(), p, w, 0, b"moved", 0, 0).await.unwrap();
    fs.flush(req(), p, w, 0).await.unwrap();
    fs.rename(
        req(),
        OsStr::new("/"),
        OsStr::new("a.txt"),
        OsStr::new("/"),
        OsStr::new("b.txt"),
    )
    .await
    .unwrap();
    fs.release(req(), p, w, 0, 0, false).await.unwrap();
    assert!(!fs.op.exists("a.txt").await.unwrap());
    assert_eq!(fs.op.read("b.txt").await.unwrap().to_vec(), b"moved");
}

#[tokio::test]
async fn content_that_cannot_be_uploaded_is_kept_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let make_op =
        || Operator::new(opendal::services::Fs::default().root(&root.to_string_lossy())).unwrap();
    let spool_dir = dir.path().join("spool");
    let fs = Filesystem::new(make_op(), 0, 0, spool_dir.clone());
    let w = create(&fs, "f.txt").await;
    let p = Some(OsStr::new("/f.txt"));
    fs.write(req(), p, w, 0, b"precious", 0, 0).await.unwrap();

    std::fs::remove_dir_all(&root).unwrap();
    std::fs::write(&root, "not a folder").unwrap();
    assert!(fs.flush(req(), p, w, 0).await.is_err());
    assert!(fs.release(req(), p, w, 0, 0, false).await.is_err());
    drop(fs);

    // The storage works again, for example after a restart of the driver.
    std::fs::remove_file(&root).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let uploaded = states::upload_pending(&make_op(), &spool_dir)
        .await
        .unwrap();
    assert_eq!(uploaded, 1);
    assert_eq!(std::fs::read(root.join("f.txt")).unwrap(), b"precious");
}

#[tokio::test]
async fn opening_for_write_without_truncate_keeps_the_content() {
    let fs = fs_with("f.txt", "hello").await;
    let p = Some(OsStr::new("/f.txt"));
    let w = open(&fs, "/f.txt", libc::O_WRONLY).await;
    fs.write(req(), p, w, 0, b"J", 0, 0).await.unwrap();
    fs.release(req(), p, w, 0, 0, false).await.unwrap();
    assert_eq!(fs.op.read("f.txt").await.unwrap().to_vec(), b"Jello");
}
