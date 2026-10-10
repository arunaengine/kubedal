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
    Filesystem::new(op, 0, 0)
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
