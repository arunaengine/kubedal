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
    let uploaded = states::upload_pending(&make_op(), &spool_dir, &SpoolBudget::new(u64::MAX))
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

#[tokio::test]
async fn closed_files_are_renamed_by_copy() {
    // The memory backend has no native rename, like S3, GCS and Azure Blob.
    let fs = fs_with("other", "").await;
    assert!(!fs.op.info().capability().rename);
    let w = create(&fs, "out.tmp").await;
    let p = Some(OsStr::new("/out.tmp"));
    fs.write(req(), p, w, 0, b"result", 0, 0).await.unwrap();
    fs.release(req(), p, w, 0, 0, false).await.unwrap();
    // Write a temp file, close it, then rename it to the final name.
    fs.rename(
        req(),
        OsStr::new("/"),
        OsStr::new("out.tmp"),
        OsStr::new("/"),
        OsStr::new("out"),
    )
    .await
    .unwrap();
    assert!(!fs.op.exists("out.tmp").await.unwrap());
    assert_eq!(fs.op.read("out").await.unwrap().to_vec(), b"result");
}

#[tokio::test]
async fn renaming_a_folder_without_native_rename_asks_for_a_copy() {
    let fs = fs_with("dir/a.txt", "a").await;
    let err = fs
        .rename(
            req(),
            OsStr::new("/"),
            OsStr::new("dir"),
            OsStr::new("/"),
            OsStr::new("dir2"),
        )
        .await
        .unwrap_err();
    assert_eq!(err, Errno::from(libc::EXDEV));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_racing_the_last_release_never_loses_data() {
    let fs = fs_with("other", "").await;
    for i in 0..30 {
        let (a, b) = (format!("a{i}"), format!("b{i}"));
        let w = create(&fs, &a).await;
        let pa = format!("/{a}");
        let p = Some(OsStr::new(&pa));
        fs.write(req(), p, w, 0, b"data", 0, 0).await.unwrap();
        let (released, renamed) = tokio::join!(
            fs.release(req(), p, w, 0, 0, false),
            fs.rename(
                req(),
                OsStr::new("/"),
                OsStr::new(&a),
                OsStr::new("/"),
                OsStr::new(&b)
            )
        );
        released.unwrap();
        renamed.unwrap();
        assert_eq!(fs.op.read(&b).await.unwrap().to_vec(), b"data", "round {i}");
        assert!(!fs.op.exists(&a).await.unwrap(), "round {i}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncate_racing_the_last_release_is_never_lost_silently() {
    let fs = fs_with("other", "").await;
    for i in 0..30 {
        let name = format!("t{i}");
        let path = format!("/{name}");
        let p = Some(OsStr::new(&path));
        let w = create(&fs, &name).await;
        fs.write(req(), p, w, 0, b"12345", 0, 0).await.unwrap();
        let (released, truncated) = tokio::join!(
            fs.release(req(), p, w, 0, 0, false),
            fs.setattr(req(), p, None, size(2))
        );
        released.unwrap();
        let stored = fs.op.read(&name).await.unwrap().to_vec();
        match truncated {
            // The truncate reached the content before the upload.
            Ok(_) => assert_eq!(stored, b"12", "round {i}"),
            // The file was already closed; the truncate says it could not work.
            Err(err) => {
                assert_eq!(err, Errno::from(libc::EOPNOTSUPP), "round {i}");
                assert_eq!(stored, b"12345", "round {i}");
            }
        }
    }
}

#[tokio::test]
async fn readers_keep_the_file_they_opened() {
    let fs = fs_with("other", "").await;
    let w = create(&fs, "f.txt").await;
    let p = Some(OsStr::new("/f.txt"));
    fs.write(req(), p, w, 0, b"x", 0, 0).await.unwrap();
    // A reader opens while the writer is open, then the writer appends.
    let r = open(&fs, "/f.txt", libc::O_RDONLY).await;
    fs.write(req(), p, w, 1, b"A", 0, 0).await.unwrap();
    assert_eq!(read_all(&fs, "/f.txt", r).await, b"xA");
    // After the release the reader still sees the same content.
    fs.release(req(), p, w, 0, 0, false).await.unwrap();
    assert_eq!(read_all(&fs, "/f.txt", r).await, b"xA");
    // The name is removed and a new file gets it; the reader keeps the old file.
    fs.unlink(req(), OsStr::new("/"), OsStr::new("f.txt"))
        .await
        .unwrap();
    let w2 = create(&fs, "f.txt").await;
    fs.write(req(), p, w2, 0, b"replacement", 0, 0)
        .await
        .unwrap();
    fs.release(req(), p, w2, 0, 0, false).await.unwrap();
    assert_eq!(read_all(&fs, "/f.txt", r).await, b"xA");
}

#[tokio::test]
async fn stopped_retry_workers_never_upload_late() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    let make_op =
        || Operator::new(opendal::services::Fs::default().root(&root.to_string_lossy())).unwrap();
    let spool_dir = dir.path().join("spool");
    let fs = Filesystem::new(make_op(), 0, 0, spool_dir.clone());
    let w = create(&fs, "f.txt").await;
    let p = Some(OsStr::new("/f.txt"));
    fs.write(req(), p, w, 0, b"old", 0, 0).await.unwrap();
    // The upload fails, so a retry worker starts.
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::write(&root, "not a folder").unwrap();
    assert!(fs.release(req(), p, w, 0, 0, false).await.is_err());
    let workers = fs.workers();
    assert_eq!(workers.len(), 1);

    // Unpublish stops the workers first, then replays the spool files.
    workers.shutdown().await;
    assert!(workers.is_empty());
    std::fs::remove_file(&root).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    assert_eq!(
        states::upload_pending(&make_op(), &spool_dir, &SpoolBudget::new(u64::MAX))
            .await
            .unwrap(),
        1
    );
    // A newer version written afterwards is never overwritten by the old retry.
    std::fs::write(root.join("f.txt"), "newer").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(std::fs::read(root.join("f.txt")).unwrap(), b"newer");
}

#[tokio::test]
async fn writes_past_the_spool_limit_fail_with_enospc() {
    let op = Operator::new(Memory::default()).unwrap();
    let fs = Filesystem::new(op, 0, 0, tempfile::tempdir().unwrap().keep())
        .with_spool_budget(Arc::new(SpoolBudget::new(4)));
    let w = create(&fs, "f.txt").await;
    let p = Some(OsStr::new("/f.txt"));
    fs.write(req(), p, w, 0, b"1234", 0, 0).await.unwrap();
    let err = fs.write(req(), p, w, 4, b"5", 0, 0).await.unwrap_err();
    assert_eq!(err, Errno::from(libc::ENOSPC));
}

#[tokio::test]
async fn renaming_an_open_file_never_removes_a_later_file_with_the_old_name() {
    let fs = fs_with("other", "").await;
    let pa = Some(OsStr::new("/a"));
    let pb = Some(OsStr::new("/b"));
    let w = create(&fs, "a").await;
    fs.write(req(), pa, w, 0, b"moved", 0, 0).await.unwrap();
    fs.rename(
        req(),
        OsStr::new("/"),
        OsStr::new("a"),
        OsStr::new("/"),
        OsStr::new("b"),
    )
    .await
    .unwrap();
    // A new file gets the old name and is closed first.
    let w2 = create(&fs, "a").await;
    fs.write(req(), pa, w2, 0, b"new a", 0, 0).await.unwrap();
    fs.release(req(), pa, w2, 0, 0, false).await.unwrap();
    // Closing the renamed file must not touch the new file.
    fs.release(req(), pb, w, 0, 0, false).await.unwrap();
    assert_eq!(fs.op.read("a").await.unwrap().to_vec(), b"new a");
    assert_eq!(fs.op.read("b").await.unwrap().to_vec(), b"moved");
}

async fn fs_in(dir: &std::path::Path) -> (Filesystem, Operator) {
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let op = Operator::new(opendal::services::Fs::default().root(&root.to_string_lossy())).unwrap();
    (Filesystem::new(op.clone(), 0, 0, dir.join("spool")), op)
}

#[tokio::test]
async fn rename_onto_an_open_file_retires_it_also_for_replay() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, op) = fs_in(dir.path()).await;
    let (pa, pb) = (Some(OsStr::new("/a")), Some(OsStr::new("/b")));
    let wb = create(&fs, "b").await;
    fs.write(req(), pb, wb, 0, b"old b", 0, 0).await.unwrap();
    let wa = create(&fs, "a").await;
    fs.write(req(), pa, wa, 0, b"new content", 0, 0)
        .await
        .unwrap();
    fs.rename(
        req(),
        OsStr::new("/"),
        OsStr::new("a"),
        OsStr::new("/"),
        OsStr::new("b"),
    )
    .await
    .unwrap();
    fs.release(req(), pb, wa, 0, 0, false).await.unwrap();
    // The driver stops before the old b is closed, then replays its spool.
    drop(fs);
    states::upload_pending(&op, &dir.path().join("spool"), &SpoolBudget::new(u64::MAX))
        .await
        .unwrap();
    assert_eq!(op.read("b").await.unwrap().to_vec(), b"new content");
    let _ = wb;
}

#[tokio::test]
async fn closing_a_retired_file_never_overwrites_a_finished_rename() {
    let fs = fs_with("other", "").await;
    let (pa, pb) = (Some(OsStr::new("/a")), Some(OsStr::new("/b")));
    let wb = create(&fs, "b").await;
    fs.write(req(), pb, wb, 0, b"old b", 0, 0).await.unwrap();
    // a is closed before the rename, so the closed-file path renames it.
    let wa = create(&fs, "a").await;
    fs.write(req(), pa, wa, 0, b"from a", 0, 0).await.unwrap();
    fs.release(req(), pa, wa, 0, 0, false).await.unwrap();
    fs.rename(
        req(),
        OsStr::new("/"),
        OsStr::new("a"),
        OsStr::new("/"),
        OsStr::new("b"),
    )
    .await
    .unwrap();
    fs.release(req(), pb, wb, 0, 0, false).await.unwrap();
    assert_eq!(fs.op.read("b").await.unwrap().to_vec(), b"from a");
}

#[tokio::test]
async fn rename2_honors_its_flags() {
    let fs = fs_with("b", "existing").await;
    fs.op.write("a", "a").await.unwrap();
    let root = OsStr::new("/");
    let err = fs
        .rename2(
            req(),
            root,
            OsStr::new("a"),
            root,
            OsStr::new("b"),
            libc::RENAME_NOREPLACE,
        )
        .await
        .unwrap_err();
    assert_eq!(err, Errno::from(libc::EEXIST));
    assert_eq!(fs.op.read("b").await.unwrap().to_vec(), b"existing");
    let err = fs
        .rename2(
            req(),
            root,
            OsStr::new("a"),
            root,
            OsStr::new("b"),
            libc::RENAME_EXCHANGE,
        )
        .await
        .unwrap_err();
    assert_eq!(err, Errno::from(libc::EINVAL));
    fs.rename2(
        req(),
        root,
        OsStr::new("a"),
        root,
        OsStr::new("c"),
        libc::RENAME_NOREPLACE,
    )
    .await
    .unwrap();
    assert_eq!(fs.op.read("c").await.unwrap().to_vec(), b"a");
}

#[tokio::test]
async fn getattr_with_a_handle_uses_the_handle_state() {
    let fs = fs_with("other", "").await;
    let p = Some(OsStr::new("/f"));
    let w = create(&fs, "f").await;
    fs.write(req(), p, w, 0, b"abc", 0, 0).await.unwrap();
    fs.unlink(req(), OsStr::new("/"), OsStr::new("f"))
        .await
        .unwrap();
    // After unlink, the handle still has attributes.
    let attr = fs.getattr(req(), p, Some(w), 0).await.unwrap();
    assert_eq!(attr.attr.size, 3);
    // A new file with the same name does not change what the old handle sees.
    let w2 = create(&fs, "f").await;
    fs.write(req(), p, w2, 0, b"longer content", 0, 0)
        .await
        .unwrap();
    let attr = fs.getattr(req(), p, Some(w), 0).await.unwrap();
    assert_eq!(attr.attr.size, 3);
}

#[tokio::test]
async fn shutdown_waits_for_running_callbacks_and_refuses_new_workers() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, _op) = fs_in(dir.path()).await;
    let fs = Arc::new(fs);
    let w = create(&fs, "f.txt").await;
    let p = Some(OsStr::new("/f.txt"));
    fs.write(req(), p, w, 0, b"data", 0, 0).await.unwrap();
    // The upload will fail, which normally starts a retry worker.
    let root = dir.path().join("root");
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::write(&root, "not a folder").unwrap();

    let pause = fs.pause_points().arm("release_upload");
    let release = {
        let fs = fs.clone();
        tokio::spawn(async move {
            fs.release(req(), Some(OsStr::new("/f.txt")), w, 0, 0, false)
                .await
        })
    };
    pause.reached().await;
    let workers = fs.workers();
    let shutdown = {
        let workers = workers.clone();
        tokio::spawn(async move { workers.shutdown().await })
    };
    // Shutdown waits while the release callback still runs.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!shutdown.is_finished());
    pause.resume();
    assert!(release.await.unwrap().is_err());
    shutdown.await.unwrap();
    // The failed upload could not start a worker during shutdown; the content
    // stays in the spool for the replay.
    assert!(workers.is_empty());
    assert!(
        std::fs::read_dir(dir.path().join("spool"))
            .unwrap()
            .any(|e| e.unwrap().path().extension().is_some_and(|x| x == "json"))
    );
}

/// Run `first` until it stops at `point`, start `second`, give `second` time to
/// block, then let `first` go on.
async fn forced<A, B>(
    fs: &Filesystem,
    point: &'static str,
    first: A,
    second: B,
) -> (A::Output, B::Output)
where
    A: std::future::Future,
    B: std::future::Future,
{
    let pause = fs.pause_points().arm(point);
    let second = async {
        pause.reached().await;
        let second = second;
        tokio::pin!(second);
        // Let the second operation run until it waits for the state lock.
        let quick = tokio::time::timeout(std::time::Duration::from_millis(30), &mut second).await;
        pause.resume();
        match quick {
            Ok(out) => out,
            Err(_) => second.await,
        }
    };
    tokio::join!(first, second)
}

#[tokio::test]
async fn forced_release_before_rename() {
    let fs = fs_with("other", "").await;
    let w = create(&fs, "a").await;
    let p = Some(OsStr::new("/a"));
    fs.write(req(), p, w, 0, b"data", 0, 0).await.unwrap();
    let root = OsStr::new("/");
    let (released, renamed) = forced(
        &fs,
        "release_upload",
        fs.release(req(), p, w, 0, 0, false),
        fs.rename(req(), root, OsStr::new("a"), root, OsStr::new("b")),
    )
    .await;
    released.unwrap();
    renamed.unwrap();
    assert_eq!(fs.op.read("b").await.unwrap().to_vec(), b"data");
    assert!(!fs.op.exists("a").await.unwrap());
}

#[tokio::test]
async fn forced_rename_before_release() {
    let fs = fs_with("other", "").await;
    let w = create(&fs, "a").await;
    let p = Some(OsStr::new("/a"));
    fs.write(req(), p, w, 0, b"data", 0, 0).await.unwrap();
    let root = OsStr::new("/");
    let (renamed, released) = forced(
        &fs,
        "rename_locked",
        fs.rename(req(), root, OsStr::new("a"), root, OsStr::new("b")),
        fs.release(req(), p, w, 0, 0, false),
    )
    .await;
    renamed.unwrap();
    released.unwrap();
    assert_eq!(fs.op.read("b").await.unwrap().to_vec(), b"data");
    assert!(!fs.op.exists("a").await.unwrap());
}

#[tokio::test]
async fn forced_release_before_truncate() {
    let fs = fs_with("other", "").await;
    let w = create(&fs, "t").await;
    let p = Some(OsStr::new("/t"));
    fs.write(req(), p, w, 0, b"12345", 0, 0).await.unwrap();
    let (released, truncated) = forced(
        &fs,
        "release_upload",
        fs.release(req(), p, w, 0, 0, false),
        fs.setattr(req(), p, None, size(2)),
    )
    .await;
    released.unwrap();
    // The truncate reaches the state after the upload but before the close, so
    // the close step uploads the truncated content. It is never lost.
    truncated.unwrap();
    assert_eq!(fs.op.read("t").await.unwrap().to_vec(), b"12");
}

#[tokio::test]
async fn forced_open_during_rename_never_displaces_a_new_destination() {
    let fs = fs_with("other", "").await;
    let (pa, pb) = (Some(OsStr::new("/a")), Some(OsStr::new("/b")));
    let wa = create(&fs, "a").await;
    fs.write(req(), pa, wa, 0, b"renamed", 0, 0).await.unwrap();
    let root = OsStr::new("/");
    // While the rename waits on storage, another program creates b.
    let (renamed, created) = forced(
        &fs,
        "rename_locked",
        fs.rename(req(), root, OsStr::new("a"), root, OsStr::new("b")),
        fs.create(req(), root, OsStr::new("b"), 0o644, WRITE_FLAGS),
    )
    .await;
    renamed.unwrap();
    // The create had to wait for the rename and then truncated the renamed file
    // (O_TRUNC), like on a local file system. It never writes past the rename.
    let wb = created.unwrap().fh;
    fs.write(req(), pb, wb, 0, b"created later", 0, 0)
        .await
        .unwrap();
    fs.release(req(), pb, wb, 0, 0, false).await.unwrap();
    fs.release(req(), pb, wa, 0, 0, false).await.unwrap();
    assert_eq!(fs.op.read("b").await.unwrap().to_vec(), b"created later");
    assert!(!fs.op.exists("a").await.unwrap());
}

#[tokio::test]
async fn rename_with_a_failing_journal_changes_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (fs, op) = fs_in(dir.path()).await;
    let pa = Some(OsStr::new("/a"));
    let w = create(&fs, "a").await;
    fs.write(req(), pa, w, 0, b"data", 0, 0).await.unwrap();
    fs.flush(req(), pa, w, 0).await.unwrap();
    // The journal cannot be written: the spool folder is read-only.
    let spool = dir.path().join("spool");
    std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o555)).unwrap();
    let root = OsStr::new("/");
    let result = fs
        .rename(req(), root, OsStr::new("a"), root, OsStr::new("b"))
        .await;
    std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.is_err());
    // Nothing moved: the object, the name and the journal are still the old ones.
    assert_eq!(op.read("a").await.unwrap().to_vec(), b"data");
    assert!(!op.exists("b").await.unwrap());
    fs.write(req(), pa, w, 4, b"!", 0, 0).await.unwrap();
    fs.release(req(), pa, w, 0, 0, false).await.unwrap();
    assert_eq!(op.read("a").await.unwrap().to_vec(), b"data!");
}

#[tokio::test]
async fn failed_closed_file_rename_keeps_the_open_destination() {
    let fs = fs_with("other", "").await;
    let pb = Some(OsStr::new("/b"));
    let wb = create(&fs, "b").await;
    fs.write(req(), pb, wb, 0, b"pending edits", 0, 0)
        .await
        .unwrap();
    // The source does not exist, so the rename fails.
    let root = OsStr::new("/");
    let err = fs
        .rename(req(), root, OsStr::new("missing"), root, OsStr::new("b"))
        .await
        .unwrap_err();
    assert_eq!(err, Errno::from(libc::ENOENT));
    // The open destination keeps its pending edits and uploads them.
    fs.release(req(), pb, wb, 0, 0, false).await.unwrap();
    assert_eq!(fs.op.read("b").await.unwrap().to_vec(), b"pending edits");
}

#[tokio::test]
async fn failed_retirement_in_unlink_stops_changes_without_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, op) = fs_in(dir.path()).await;
    let p = Some(OsStr::new("/f"));
    let w = create(&fs, "f").await;
    fs.write(req(), p, w, 0, b"data", 0, 0).await.unwrap();
    // The journal removal cannot be made durable after the object was deleted
    // in storage.
    fs.states.pause.fail_once("dir_fsync");
    let result = fs.unlink(req(), OsStr::new("/"), OsStr::new("f")).await;
    assert_eq!(result.unwrap_err(), Errno::from(libc::EIO));
    // Later opens fail at once instead of waiting on a dead state.
    let open = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fs.open(req(), OsStr::new("/f"), libc::O_WRONLY as u32),
    )
    .await
    .expect("open must not hang");
    assert!(open.is_err());
    assert!(fs.release(req(), p, w, 0, 0, false).await.is_err());
    assert!(!op.exists("f").await.unwrap());
}

#[tokio::test]
async fn callbacks_that_enter_after_shutdown_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, _op) = fs_in(dir.path()).await;
    let fs = Arc::new(fs);
    let w = create(&fs, "f").await;
    // A write callback was started by fuse3 but stops right before it is counted.
    let pause = fs.pause_points().arm("before_enter");
    let write = {
        let fs = fs.clone();
        tokio::spawn(async move {
            fs.write(req(), Some(OsStr::new("/f")), w, 0, b"late", 0, 0)
                .await
        })
    };
    pause.reached().await;
    // Shutdown sees no running callback and finishes.
    fs.workers().shutdown().await;
    pause.resume();
    let err = write.await.unwrap().unwrap_err();
    assert_eq!(err, Errno::from(libc::ENOTCONN));
}

#[tokio::test]
async fn rename_noreplace_does_not_treat_lookup_errors_as_absent() {
    let fs = fs_with("a", "a").await;
    // The lookup of the destination fails with something other than "not found".
    fs.pause_points().fail_once("noreplace_lookup");
    let root = OsStr::new("/");
    let result = fs
        .rename2(
            req(),
            root,
            OsStr::new("a"),
            root,
            OsStr::new("b"),
            libc::RENAME_NOREPLACE,
        )
        .await;
    assert_eq!(result.unwrap_err(), Errno::from(libc::EACCES));
    assert!(fs.op.exists("a").await.unwrap());
    assert!(!fs.op.exists("b").await.unwrap());
}

#[tokio::test]
async fn reader_getattr_survives_a_rename_of_its_file() {
    let fs = fs_with("a", "hello").await;
    let r = open(&fs, "/a", libc::O_RDONLY).await;
    let root = OsStr::new("/");
    fs.rename(req(), root, OsStr::new("a"), root, OsStr::new("b"))
        .await
        .unwrap();
    let attr = fs
        .getattr(req(), Some(OsStr::new("/a")), Some(r), 0)
        .await
        .unwrap();
    assert_eq!(attr.attr.size, 5);
}

#[tokio::test]
async fn a_failed_folder_sync_during_rename_stops_all_changes() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, op) = fs_in(dir.path()).await;
    let pa = Some(OsStr::new("/a"));
    let w = create(&fs, "a").await;
    fs.write(req(), pa, w, 0, b"source", 0, 0).await.unwrap();
    // The journal was replaced, but syncing the folder fails.
    fs.pause_points().fail_once("dir_fsync");
    let root = OsStr::new("/");
    let err = fs
        .rename(req(), root, OsStr::new("a"), root, OsStr::new("b"))
        .await
        .unwrap_err();
    assert_eq!(err, Errno::from(libc::EIO));
    // The file system stops: no more changes and no uploads from memory.
    assert_eq!(
        fs.write(req(), pa, w, 0, b"x", 0, 0).await.unwrap_err(),
        Errno::from(libc::EIO)
    );
    assert_eq!(
        fs.flush(req(), pa, w, 0).await.unwrap_err(),
        Errno::from(libc::EIO)
    );
    assert!(fs.release(req(), pa, w, 0, 0, false).await.is_err());
    assert!(!op.exists("a").await.unwrap() && !op.exists("b").await.unwrap());
    // The spool is left for the replay, which follows the newest journal.
    drop(fs);
    states::upload_pending(&op, &dir.path().join("spool"), &SpoolBudget::new(u64::MAX))
        .await
        .unwrap();
    assert_eq!(op.read("b").await.unwrap().to_vec(), b"source");
}

#[tokio::test]
async fn a_failed_folder_sync_during_unlink_stops_all_changes() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, _op) = fs_in(dir.path()).await;
    let p = Some(OsStr::new("/f"));
    let w = create(&fs, "f").await;
    fs.write(req(), p, w, 0, b"data", 0, 0).await.unwrap();
    fs.pause_points().fail_once("dir_fsync");
    assert!(
        fs.unlink(req(), OsStr::new("/"), OsStr::new("f"))
            .await
            .is_err()
    );
    assert_eq!(
        fs.create(req(), OsStr::new("/"), OsStr::new("g"), 0o644, WRITE_FLAGS)
            .await
            .unwrap_err(),
        Errno::from(libc::EIO)
    );
}

#[tokio::test]
async fn failed_retirement_after_a_closed_file_rename_stops_the_old_destination() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, op) = fs_in(dir.path()).await;
    op.write("a", "SOURCE").await.unwrap();
    let pb = Some(OsStr::new("/b"));
    let wb = create(&fs, "b").await;
    fs.write(req(), pb, wb, 0, b"OLDDEST", 0, 0).await.unwrap();
    // The journal of b cannot be removed (a folder in its place).
    let spool = dir.path().join("spool");
    let meta = std::fs::read_dir(&spool)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "json"))
        .unwrap();
    std::fs::remove_file(&meta).unwrap();
    std::fs::create_dir(&meta).unwrap();
    let root = OsStr::new("/");
    assert!(
        fs.rename(req(), root, OsStr::new("a"), root, OsStr::new("b"))
            .await
            .is_err()
    );
    assert_eq!(op.read("b").await.unwrap().to_vec(), b"SOURCE");
    // The old destination can never overwrite the rename.
    assert_eq!(
        fs.flush(req(), pb, wb, 0).await.unwrap_err(),
        Errno::from(libc::EIO)
    );
    assert!(fs.release(req(), pb, wb, 0, 0, false).await.is_err());
    assert_eq!(op.read("b").await.unwrap().to_vec(), b"SOURCE");
}

#[tokio::test]
async fn a_file_handle_keeps_working_after_its_file_is_renamed() {
    let fs = fs_with("other", "").await;
    let w = create(&fs, "a").await;
    fs.write(req(), Some(OsStr::new("/a")), w, 0, b"one", 0, 0)
        .await
        .unwrap();
    let root = OsStr::new("/");
    fs.rename(req(), root, OsStr::new("a"), root, OsStr::new("b"))
        .await
        .unwrap();
    // The kernel now sends the new path together with the old handle.
    let pb = Some(OsStr::new("/b"));
    fs.write(req(), pb, w, 3, b" two", 0, 0).await.unwrap();
    fs.fsync(req(), pb, w, false).await.unwrap();
    fs.getattr(req(), pb, Some(w), 0).await.unwrap();
    fs.release(req(), pb, w, 0, 0, false).await.unwrap();
    assert_eq!(fs.op.read("b").await.unwrap().to_vec(), b"one two");
}

#[tokio::test]
async fn path_truncate_and_a_writable_open_do_not_interleave() {
    let fs = fs_with("f", "old content").await;
    let p = Some(OsStr::new("/f"));
    // The truncate found no open state and is about to empty the object in
    // storage, while a writable open (without O_TRUNC) wants to load it.
    let (truncated, opened) = forced(
        &fs,
        "truncate_fallback",
        fs.setattr(req(), p, None, size(0)),
        fs.open(req(), OsStr::new("/f"), libc::O_WRONLY as u32),
    )
    .await;
    truncated.unwrap();
    let w = opened.unwrap().fh;
    fs.write(req(), p, w, 0, b"X", 0, 0).await.unwrap();
    fs.release(req(), p, w, 0, 0, false).await.unwrap();
    // The open loaded the truncated (empty) object, so no old tail comes back.
    assert_eq!(fs.op.read("f").await.unwrap().to_vec(), b"X");
}

#[tokio::test]
async fn mkdir_and_rmdir_are_refused_after_shutdown() {
    let fs = fs_with("dir/a", "a").await;
    fs.workers().shutdown().await;
    let root = OsStr::new("/");
    let err = fs
        .mkdir(req(), root, OsStr::new("new"), 0o755, 0)
        .await
        .unwrap_err();
    assert_eq!(err, Errno::from(libc::ENOTCONN));
    let err = fs.rmdir(req(), root, OsStr::new("dir")).await.unwrap_err();
    assert_eq!(err, Errno::from(libc::ENOTCONN));
}

#[tokio::test]
async fn rmdir_sees_files_that_exist_only_in_the_spool() {
    let fs = fs_with("other", "").await;
    let w = fs
        .create(
            req(),
            OsStr::new("/dir"),
            OsStr::new("a"),
            0o644,
            WRITE_FLAGS,
        )
        .await
        .unwrap()
        .fh;
    let err = fs
        .rmdir(req(), OsStr::new("/"), OsStr::new("dir"))
        .await
        .unwrap_err();
    assert_eq!(err, Errno::from(libc::ENOTEMPTY));
    fs.release(req(), Some(OsStr::new("/dir/a")), w, 0, 0, false)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_rename_copy_whose_delete_applied_despite_an_error_finishes() {
    let fs = fs_with("a", "source").await;
    assert!(!fs.op.info().capability().rename);
    let pb = Some(OsStr::new("/b"));
    let wb = create(&fs, "b").await;
    fs.write(req(), pb, wb, 0, b"old b", 0, 0).await.unwrap();
    fs.states.pause.fail_once("delete_fails_after_apply");
    let root = OsStr::new("/");
    fs.rename(req(), root, OsStr::new("a"), root, OsStr::new("b"))
        .await
        .unwrap();
    let _ = fs.release(req(), pb, wb, 0, 0, false).await;
    assert_eq!(fs.op.read("b").await.unwrap().to_vec(), b"source");
    assert!(!fs.op.exists("a").await.unwrap());
}

#[tokio::test]
async fn a_failed_delete_after_a_rename_copy_stops_changes() {
    let fs = fs_with("a", "source").await;
    let pb = Some(OsStr::new("/b"));
    let wb = create(&fs, "b").await;
    fs.write(req(), pb, wb, 0, b"old b", 0, 0).await.unwrap();
    fs.states.pause.fail_once("delete_fails");
    let root = OsStr::new("/");
    let renamed = fs
        .rename(req(), root, OsStr::new("a"), root, OsStr::new("b"))
        .await;
    assert_eq!(renamed.unwrap_err(), Errno::from(libc::EIO));
    assert!(fs.states.is_broken());
    // The old b never uploads over the copied content.
    assert!(fs.release(req(), pb, wb, 0, 0, false).await.is_err());
    assert_eq!(fs.op.read("b").await.unwrap().to_vec(), b"source");
    assert_eq!(fs.op.read("a").await.unwrap().to_vec(), b"source");
}

/// Break the file system with a failed folder sync while `other` flushes, stop
/// it like a crash, and replay its spool.
async fn break_and_replay(fs: Filesystem, op: &Operator, dir: &std::path::Path) {
    let other = create(&fs, "other").await;
    fs.write(req(), Some(OsStr::new("/other")), other, 0, b"OTHER", 0, 0)
        .await
        .unwrap();
    fs.pause_points().fail_once("dir_fsync");
    assert!(fs.flush(req(), None, other, 0).await.is_err());
    assert!(fs.states.is_broken());
    fs.workers().shutdown().await;
    // Like a crash: no state gets dropped, so no spool file is cleaned up.
    std::mem::forget(fs);
    let budget = states::SpoolBudget::new(u64::MAX);
    states::upload_pending(op, &dir.join("spool"), &budget)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_change_to_a_published_file_survives_a_broken_mount() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, op) = fs_in(dir.path()).await;
    op.write("victim", "OLD").await.unwrap();
    let fh = open(&fs, "/victim", libc::O_RDWR).await;
    fs.write(req(), Some(OsStr::new("/victim")), fh, 0, b"NEW", 0, 0)
        .await
        .unwrap();
    break_and_replay(fs, &op, dir.path()).await;
    assert_eq!(op.read("victim").await.unwrap().to_vec(), b"NEW");
}

#[tokio::test]
async fn a_change_after_a_flush_survives_a_broken_mount() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, op) = fs_in(dir.path()).await;
    let p = Some(OsStr::new("/victim"));
    let fh = create(&fs, "victim").await;
    fs.write(req(), p, fh, 0, b"first", 0, 0).await.unwrap();
    fs.flush(req(), p, fh, 0).await.unwrap();
    assert_eq!(op.read("victim").await.unwrap().to_vec(), b"first");
    // The flush removed the journal; the next change needs a new one.
    fs.write(req(), p, fh, 0, b"later", 0, 0).await.unwrap();
    break_and_replay(fs, &op, dir.path()).await;
    assert_eq!(op.read("victim").await.unwrap().to_vec(), b"later");
}

#[tokio::test]
async fn forced_truncate_waiting_while_a_flush_breaks_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, op) = fs_in(dir.path()).await;
    let fs = Arc::new(fs);
    op.write("victim", "KEEP").await.unwrap();
    let other = create(&fs, "other").await;
    fs.write(req(), Some(OsStr::new("/other")), other, 0, b"OTHER", 0, 0)
        .await
        .unwrap();
    let pause = fs.pause_points().arm("truncate_fallback");
    let truncate = {
        let fs = fs.clone();
        tokio::spawn(async move {
            let size = SetAttr {
                size: Some(0),
                ..Default::default()
            };
            fs.setattr(req(), Some(OsStr::new("/victim")), None, size)
                .await
        })
    };
    pause.reached().await;
    // The flush holds only its own state lock and stops the file system while
    // the truncate waits.
    fs.pause_points().fail_once("dir_fsync");
    assert!(fs.flush(req(), None, other, 0).await.is_err());
    pause.resume();
    let result = truncate.await.unwrap();
    assert_eq!(result.unwrap_err(), Errno::from(libc::EIO));
    assert_eq!(op.read("victim").await.unwrap().to_vec(), b"KEEP");
}

#[tokio::test]
async fn a_failed_first_journal_leaves_the_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, op) = fs_in(dir.path()).await;
    op.write("victim", "OLD").await.unwrap();
    let p = Some(OsStr::new("/victim"));
    let fh = open(&fs, "/victim", libc::O_RDWR).await;
    // Block the temp file of the journal with a folder of the same name.
    let spool = dir.path().join("spool");
    let data = std::fs::read_dir(&spool)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "data"))
        .unwrap();
    let blocker = data.with_extension("json.tmp");
    std::fs::create_dir(&blocker).unwrap();
    assert!(fs.write(req(), p, fh, 0, b"BAD", 0, 0).await.is_err());
    std::fs::remove_dir(&blocker).unwrap();
    // Nothing changed: the flush publishes nothing and the handle reads OLD.
    fs.flush(req(), p, fh, 0).await.unwrap();
    assert_eq!(op.read("victim").await.unwrap().to_vec(), b"OLD");
    assert_eq!(read_all(&fs, "/victim", fh).await, b"OLD");
    assert!(!fs.states.is_broken());
}

#[tokio::test]
async fn writes_after_unlink_never_bring_the_file_back() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, op) = fs_in(dir.path()).await;
    let p = Some(OsStr::new("/f"));
    let fh = create(&fs, "f").await;
    fs.write(req(), p, fh, 0, b"first", 0, 0).await.unwrap();
    fs.flush(req(), p, fh, 0).await.unwrap();
    fs.unlink(req(), OsStr::new("/"), OsStr::new("f"))
        .await
        .unwrap();
    // The open handle still writes its local copy.
    fs.write(req(), p, fh, 0, b"later", 0, 0).await.unwrap();
    assert_eq!(read_all(&fs, "/f", fh).await, b"later");
    break_and_replay(fs, &op, dir.path()).await;
    assert!(!op.exists("f").await.unwrap());
}

#[tokio::test]
async fn writes_after_a_rename_replaced_the_file_never_overwrite_it() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, op) = fs_in(dir.path()).await;
    op.write("a", "SRC").await.unwrap();
    let p = Some(OsStr::new("/b"));
    let fh = create(&fs, "b").await;
    fs.write(req(), p, fh, 0, b"old b", 0, 0).await.unwrap();
    fs.flush(req(), p, fh, 0).await.unwrap();
    let root = OsStr::new("/");
    fs.rename(req(), root, OsStr::new("a"), root, OsStr::new("b"))
        .await
        .unwrap();
    fs.write(req(), p, fh, 0, b"stale", 0, 0).await.unwrap();
    break_and_replay(fs, &op, dir.path()).await;
    assert_eq!(op.read("b").await.unwrap().to_vec(), b"SRC");
}

#[tokio::test]
async fn forced_truncating_open_after_a_release_broke_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, op) = fs_in(dir.path()).await;
    let fs = Arc::new(fs);
    let p = Some(OsStr::new("/f"));
    let first = create(&fs, "f").await;
    let second = open(&fs, "/f", libc::O_RDWR).await;
    fs.write(req(), p, first, 0, b"content", 0, 0)
        .await
        .unwrap();
    let pause = fs.pause_points().arm("release_upload");
    let release = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.release(req(), p, first, 0, 0, false).await })
    };
    pause.reached().await;
    // The truncating open waits on the state lock that the release holds.
    let truncating = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.open(req(), OsStr::new("/f"), WRITE_FLAGS).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // The release upload stops the file system at its last folder sync.
    fs.pause_points().fail_once("dir_fsync");
    pause.resume();
    assert!(release.await.unwrap().is_err());
    assert!(fs.states.is_broken());
    assert!(truncating.await.unwrap().is_err());
    assert_eq!(read_all(&fs, "/f", second).await, b"content");
    assert_eq!(op.read("f").await.unwrap().to_vec(), b"content");
}

#[tokio::test]
async fn forced_open_during_the_unlink_of_a_closed_file_waits() {
    let dir = tempfile::tempdir().unwrap();
    let (fs, op) = fs_in(dir.path()).await;
    let fs = Arc::new(fs);
    op.write("f", "OLD").await.unwrap();
    let pause = fs.pause_points().arm("unlink_closed");
    let unlink = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.unlink(req(), OsStr::new("/"), OsStr::new("f")).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), pause.reached())
        .await
        .expect("the unlink of a closed file must hold the namespace lock");
    let opened = {
        let fs = fs.clone();
        tokio::spawn(async move { fs.open(req(), OsStr::new("/f"), libc::O_RDWR as u32).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // The open must not load the old content before the delete.
    assert!(!opened.is_finished());
    pause.resume();
    unlink.await.unwrap().unwrap();
    let _ = opened.await.unwrap();
    assert_ne!(
        op.read("f").await.map(|b| b.to_vec()).ok(),
        Some(b"OLD".to_vec())
    );
}

#[tokio::test]
async fn a_rename_copy_whose_source_fails_early_stops_nothing() {
    let fs = fs_with("a", "source").await;
    assert!(!fs.op.info().capability().rename);
    fs.op.create_dir("d/").await.unwrap();
    let pb = Some(OsStr::new("/b"));
    let wb = create(&fs, "b").await;
    fs.write(req(), pb, wb, 0, b"old b", 0, 0).await.unwrap();
    let root = OsStr::new("/");
    // A folder source and an unreadable source fail before any tombstone.
    let folder = fs
        .rename(req(), root, OsStr::new("d"), root, OsStr::new("b"))
        .await;
    assert_eq!(folder.unwrap_err(), Errno::from(libc::EXDEV));
    fs.states.pause.fail_once("copy_source_fails");
    let unreadable = fs
        .rename(req(), root, OsStr::new("a"), root, OsStr::new("b"))
        .await;
    assert_eq!(unreadable.unwrap_err(), Errno::from(libc::EIO));
    assert!(!fs.states.is_broken());
    // The open b still uploads its own content.
    fs.release(req(), pb, wb, 0, 0, false).await.unwrap();
    assert_eq!(fs.op.read("b").await.unwrap().to_vec(), b"old b");
    assert_eq!(fs.op.read("a").await.unwrap().to_vec(), b"source");
}

#[tokio::test]
async fn names_that_storage_would_change_are_refused() {
    use std::os::unix::ffi::OsStrExt;
    let fs = fs_with("report", "KEEP").await;
    // The lossy form of f\xff and f\xfe.
    fs.op.write("f\u{fffd}", "OTHER").await.unwrap();
    let root = OsStr::new("/");
    let invalid = OsStr::from_bytes(b"f\xff");
    let invalid_path = OsStr::from_bytes(b"/f\xfe");
    let spaced = OsStr::new("report ");
    let eilseq = Errno::from(libc::EILSEQ);
    let einval = Errno::from(libc::EINVAL);
    let create = |name| fs.create(req(), root, name, 0o644, WRITE_FLAGS);
    assert_eq!(create(invalid).await.unwrap_err(), eilseq);
    assert_eq!(create(spaced).await.unwrap_err(), einval);
    let flags = (libc::O_RDWR | libc::O_TRUNC) as u32;
    assert_eq!(
        fs.open(req(), invalid_path, flags).await.unwrap_err(),
        eilseq
    );
    assert_eq!(
        fs.open(req(), OsStr::new("/report "), flags)
            .await
            .unwrap_err(),
        einval
    );
    assert_eq!(fs.unlink(req(), root, invalid).await.unwrap_err(), eilseq);
    assert_eq!(fs.unlink(req(), root, spaced).await.unwrap_err(), einval);
    let rename = |from, to| fs.rename(req(), root, from, root, to);
    assert_eq!(
        rename(OsStr::new("report"), invalid).await.unwrap_err(),
        eilseq
    );
    assert_eq!(rename(spaced, OsStr::new("x")).await.unwrap_err(), einval);
    assert_eq!(
        fs.lookup(req(), root, OsStr::new(" report"))
            .await
            .unwrap_err(),
        einval
    );
    // The objects with the names they would have changed into stay intact.
    assert_eq!(fs.op.read("report").await.unwrap().to_vec(), b"KEEP");
    assert_eq!(fs.op.read("f\u{fffd}").await.unwrap().to_vec(), b"OTHER");
    assert!(!fs.op.exists("x").await.unwrap());
}

#[tokio::test]
async fn replies_set_no_fopen_flags_from_the_request() {
    let fs = fs_with("f", "content").await;
    let exclusive = (libc::O_RDWR | libc::O_CREAT | libc::O_EXCL) as u32;
    let created = fs
        .create(req(), OsStr::new("/"), OsStr::new("new"), 0o600, exclusive)
        .await
        .unwrap();
    assert_eq!(created.flags, 0);
    let flags = (libc::O_RDWR | libc::O_APPEND | libc::O_TRUNC | libc::O_DIRECT) as u32;
    let opened = fs.open(req(), OsStr::new("/f"), flags).await.unwrap();
    assert_eq!(opened.flags, 0);
    let dir = fs
        .opendir(req(), OsStr::new("/"), libc::O_DIRECTORY as u32)
        .await
        .unwrap();
    assert_eq!(dir.flags, 0);
}

#[tokio::test]
async fn link_counts_are_one_for_files_two_for_folders_and_zero_after_unlink() {
    let fs = fs_with("f", "abc").await;
    fs.op.create_dir("d/").await.unwrap();
    let nlink = |path: &'static str| {
        let fs = &fs;
        async move {
            fs.getattr(req(), Some(OsStr::new(path)), None, 0)
                .await
                .unwrap()
                .attr
                .nlink
        }
    };
    assert_eq!(nlink("/f").await, 1);
    assert_eq!(nlink("/d").await, 2);
    let fh = open(&fs, "/f", libc::O_RDWR).await;
    fs.unlink(req(), OsStr::new("/"), OsStr::new("f"))
        .await
        .unwrap();
    let attr = fs.getattr(req(), None, Some(fh), 0).await.unwrap().attr;
    assert_eq!(attr.nlink, 0);
}

#[tokio::test]
async fn rmdir_removes_the_folder_marker_and_refuses_folders_with_children() {
    let fs = fs_with("other", "").await;
    let root = OsStr::new("/");
    fs.mkdir(req(), root, OsStr::new("d"), 0o755, 0)
        .await
        .unwrap();
    assert!(fs.op.exists("d/").await.unwrap());
    fs.op.write("d/f", "child").await.unwrap();
    let full = fs.rmdir(req(), root, OsStr::new("d")).await;
    assert_eq!(full.unwrap_err(), Errno::from(libc::ENOTEMPTY));
    assert_eq!(fs.op.read("d/f").await.unwrap().to_vec(), b"child");
    fs.op.delete("d/f").await.unwrap();
    fs.rmdir(req(), root, OsStr::new("d")).await.unwrap();
    assert!(!fs.op.exists("d/").await.unwrap());
}

#[tokio::test]
async fn readdirplus_reports_the_live_size_like_getattr() {
    use futures::StreamExt;
    let fs = fs_with("f", "abc").await;
    let p = Some(OsStr::new("/f"));
    let fh = open(&fs, "/f", libc::O_RDWR).await;
    fs.write(req(), p, fh, 0, b"123456789", 0, 0).await.unwrap();
    let listed_size = || async {
        let reply = fs
            .readdirplus(req(), OsStr::new("/"), 0, 0, 0)
            .await
            .unwrap();
        let entries: Vec<_> = reply.entries.collect().await;
        entries
            .into_iter()
            .map(|e| e.unwrap())
            .find(|e| e.name == "f")
            .unwrap()
            .attr
            .size
    };
    let getattr_size = fs.getattr(req(), p, None, 0).await.unwrap().attr.size;
    assert_eq!(getattr_size, 9);
    assert_eq!(listed_size().await, 9);
    let zero = SetAttr {
        size: Some(0),
        ..Default::default()
    };
    fs.setattr(req(), p, Some(fh), zero).await.unwrap();
    assert_eq!(listed_size().await, 0);
}
