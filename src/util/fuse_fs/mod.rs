// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

mod dir;
mod file;
mod ops;
pub mod pause;
pub mod states;
#[cfg(test)]
mod tests;

use std::ffi::OsStr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use bytes::Bytes;
use fuse3::Errno;
use fuse3::Result;
use fuse3::path::prelude::*;
use futures::StreamExt;
use futures::stream;
use opendal::EntryMode;
use opendal::ErrorKind;
use opendal::Metadata;
use opendal::Operator;
use opendal::raw::normalize_path;
use sharded_slab::Slab;

use file::FileKey;
use file::OpenedFile;
pub use states::SpoolBudget;
use states::{FileState, States};

const TTL: Duration = Duration::from_secs(1); // 1 second

/// Background tasks of one FUSE file system.
pub struct Workers {
    tasks: std::sync::Mutex<tokio::task::JoinSet<()>>,
    fence: Arc<states::Fence>,
}

impl Workers {
    fn spawn(&self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        if self.fence.is_closing() {
            // The spool files stay; the replay after shutdown uploads them.
            return;
        }
        let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
        // Forget tasks that are done.
        while tasks.try_join_next().is_some() {}
        tasks.spawn(task);
    }

    /// Stop the file system: refuse new uploads and workers, wait for running
    /// operations (FUSE callbacks), then stop all tasks and wait until they are
    /// gone. Spool files stay on disk, so the next replay uploads them.
    pub async fn shutdown(&self) {
        self.fence.close_and_wait().await;
        let mut tasks = {
            let mut guard = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
            std::mem::take(&mut *guard)
        };
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }

    /// Number of tasks that still run.
    pub fn len(&self) -> usize {
        let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
        while tasks.try_join_next().is_some() {}
        tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `Filesystem` implements [`PathFilesystem`] on top of an OpenDAL [`Operator`].
///
/// This code is vendored from the `fuse3_opendal` crate (Apache-2.0), because that
/// crate does not yet support the OpenDAL and fuse3 versions used by kubedal.
/// Changes: adapted to opendal 0.59 and fuse3 0.9; `read` only fetches the
/// requested byte range and stops at the end of the file; the writer of a file
/// is replaced by a shared spool file per object (see [`spool`]) that is uploaded
/// on `flush`, `fsync` and when the last handle closes, so upload errors reach
/// the program, several handles see the same content, and content that cannot
/// be uploaded is kept on the node.
pub struct Filesystem {
    op: Operator,
    gid: u32,
    uid: u32,
    /// Shared states of files that are open for writing.
    states: Arc<States>,
    spool_dir: PathBuf,
    /// Background upload retries, so they can be stopped with the mount.
    workers: Arc<Workers>,

    opened_files: Slab<OpenedFile>,
}

impl Filesystem {
    /// Change the size of a file. Object storage cannot change part of an object,
    /// so only truncating to zero or to the current size is supported. Other sizes
    /// fail with EOPNOTSUPP instead of claiming success.
    async fn truncate(&self, path: Option<&OsStr>, fh: Option<u64>, size: u64) -> Result<()> {
        if let Some(fh) = fh {
            let writer = self
                .get_opened_file(FileKey::try_from(fh)?, path)?
                .write
                .clone();
            if let Some(writer) = writer {
                // An open file is a local spool file, so any size works.
                let mut data = writer.lock().await;
                // Another operation may have stopped the file system meanwhile.
                self.check_writable()?;
                if !data.closed {
                    self.states
                        .set_len(&writer, &mut data, size)
                        .map_err(io_errno)?;
                    return Ok(());
                }
            }
        }
        // The namespace lock covers the state lookup and the storage fallback,
        // so no writable open can load the old content in between.
        let _ns = self.states.lock_namespace().await;
        self.check_writable()?;
        // Another handle writes this file: truncate the shared content. A state
        // that closed meanwhile is skipped; its content is in storage then.
        if let Some(path) = path
            && self
                .states
                .truncate(&key_of(path)?, size)
                .await
                .map_err(io_errno)?
        {
            return Ok(());
        }
        self.states.pause.hit("truncate_fallback").await;
        let path = match (path, fh) {
            (Some(path), _) => key_of(path)?,
            (None, Some(fh)) => key_of(
                self.get_opened_file(FileKey::try_from(fh)?, None)?
                    .path
                    .as_os_str(),
            )?,
            (None, None) => return Err(Errno::from(libc::EINVAL)),
        };
        let current = self
            .stat_entry(&path)
            .await
            .map_err(opendal_error2errno)?
            .content_length();
        if size == current {
            return Ok(());
        }
        if size != 0 {
            return Err(Errno::from(libc::EOPNOTSUPP));
        }
        // A flush may have stopped the file system while this call waited.
        self.check_writable()?;
        self.op
            .write(&path, Bytes::new())
            .await
            .map_err(opendal_error2errno)?;
        Ok(())
    }

    /// Stat a path. Object stores often have no object for a folder, so a missing
    /// path is tried again as a folder (with a trailing slash).
    async fn stat_entry(&self, path: &str) -> opendal::Result<Metadata> {
        match self.op.stat(path).await {
            Err(err) if err.kind() == ErrorKind::NotFound && !path.ends_with('/') => {
                let dir = format!("{}/", path.trim_end_matches('/'));
                let meta = self.op.stat(&dir).await?;
                if meta.is_dir() { Ok(meta) } else { Err(err) }
            }
            other => other,
        }
    }

    /// Create a new filesystem with given operator, uid and gid. Files opened for
    /// writing are kept in `spool_dir` until they are uploaded.
    pub fn new(op: Operator, uid: u32, gid: u32, spool_dir: PathBuf) -> Self {
        let states = Arc::new(States::new(
            spool_dir.clone(),
            Arc::new(SpoolBudget::new(u64::MAX)),
        ));
        Self {
            op,
            uid,
            gid,
            workers: Arc::new(Workers {
                tasks: Default::default(),
                fence: states.fence.clone(),
            }),
            spool_dir: spool_dir.clone(),
            states,
            opened_files: Slab::new(),
        }
    }

    /// Size of the spool content of `path`, if the file is being written.
    async fn spool_size(&self, path: &OsStr) -> Result<Option<u64>> {
        Ok(self.states.size(&key_of(path)?).await)
    }

    /// The live state of `path`, for reads by handles without their own state.
    async fn live_state(&self, path: &OsStr) -> Result<Option<Arc<FileState>>> {
        Ok(self.states.open_read(&key_of(path)?).await)
    }

    /// Join or create the shared write state of `path` for a new handle. With
    /// `keep`, a new state starts with the current object content.
    async fn open_write(&self, path: &OsStr, keep: bool) -> Result<Arc<FileState>> {
        self.states
            .open_write(&self.op, &key_of(path)?, keep)
            .await
            .map_err(status_errno)
    }

    /// Upload the content of the open file `fh`, if it is open for writing.
    async fn upload_open_file(&self, path: Option<&OsStr>, fh: u64) -> Result<()> {
        let state = self
            .get_opened_file(FileKey::try_from(fh)?, path)?
            .write
            .clone();
        match state {
            Some(state) => upload_errno(self.states.upload(&self.op, &state).await),
            None => Ok(()),
        }
    }

    /// A handle closes. The last handle uploads the content. If that fails, the
    /// spool files stay on the node, a background task retries, and unpublish
    /// uploads whatever is still left.
    async fn release_state(&self, state: Arc<FileState>) -> Result<()> {
        let (result, pending) = self.states.release(&self.op, &state).await;
        if pending {
            self.retry_upload(state);
        }
        upload_errno(result)
    }

    /// Limit the space of the spool files (shared by all file systems of a node).
    /// Writes past the limit fail with ENOSPC.
    pub fn with_spool_budget(mut self, budget: Arc<SpoolBudget>) -> Self {
        let mut states = States::new(self.spool_dir.clone(), budget);
        states.fence = self.workers.fence.clone();
        states.pause = self.states.pause.clone();
        self.states = Arc::new(states);
        self
    }

    /// Count a running FUSE callback, so shutdown can wait for it.
    /// After a local disk error in the middle of a change, changes fail with EIO.
    fn check_writable(&self) -> Result<()> {
        if self.states.is_broken() {
            return Err(Errno::from(libc::EIO));
        }
        Ok(())
    }

    /// After shutdown started, callbacks fail with ENOTCONN.
    fn enter(&self) -> Result<states::FenceGuard> {
        self.states
            .fence
            .enter()
            .map_err(|_| Errno::from(libc::ENOTCONN))
    }

    /// Pause points for tests that force an order of operations.
    pub fn pause_points(&self) -> Arc<pause::PausePoints> {
        self.states.pause.clone()
    }

    /// The background workers of this file system. Stop them with
    /// [`Workers::shutdown`] before anything else uploads its spool files.
    pub fn workers(&self) -> Arc<Workers> {
        self.workers.clone()
    }

    /// Retry the upload of a closed file a few times in the background.
    fn retry_upload(&self, state: Arc<FileState>) {
        let op = self.op.clone();
        let states = self.states.clone();
        self.workers.spawn(async move {
            for attempt in 0..5u32 {
                tokio::time::sleep(Duration::from_secs(2u64 << attempt)).await;
                if states.retry(&op, &state).await {
                    return;
                }
            }
            tracing::error!(
                "upload of FUSE file {} keeps failing; the content stays on the node and is \
                 uploaded before the next mount or on unpublish",
                state.id
            );
        });
    }

    /// Remove the name `path` from storage and from a live state. Returns true if
    /// a live state existed. The state only loses its name if the delete worked.
    async fn unlink_state(&self, path: &OsStr) -> Result<bool> {
        self.states
            .unlink(&self.op, &key_of(path)?)
            .await
            .map_err(status_errno)
    }

    /// Copy a closed file for a rename on a backend without native rename (S3,
    /// GCS, Azure Blob); the caller then removes the old object.
    ///
    /// `source` comes from [`Filesystem::open_copy_source`]: a stream of the
    /// content, or None for a copy inside the storage.
    async fn copy_for_rename(
        &self,
        from: &str,
        to: &str,
        source: Option<opendal::FuturesBytesStream>,
    ) -> Result<()> {
        if let Some(mut reader) = source {
            let mut writer = self
                .op
                .writer_with(to)
                .chunk(8 * 1024 * 1024)
                .await
                .map_err(opendal_error2errno)?;
            while let Some(chunk) = reader.next().await {
                let chunk =
                    chunk.map_err(|e| Errno::from(e.raw_os_error().unwrap_or(libc::EIO)))?;
                if let Err(err) = writer.write(chunk).await {
                    let _ = writer.abort().await;
                    return Err(opendal_error2errno(err));
                }
            }
            writer.close().await.map_err(opendal_error2errno)?;
        } else {
            self.op.copy(from, to).await.map_err(opendal_error2errno)?;
        }
        Ok(())
    }

    /// Check the source of a rename by copy and open it, before anything is
    /// written: a missing, unreadable or folder source fails here, while the
    /// destination is untouched. Folders fail with EXDEV, which tells programs
    /// like `mv` to copy the files themselves. Returns a stream of the content, or None if
    /// the storage copies by itself. Test failure point: "copy_source_fails".
    async fn open_copy_source(&self, from: &str) -> Result<Option<opendal::FuturesBytesStream>> {
        if self.states.pause.should_fail("copy_source_fails") {
            return Err(Errno::from(libc::EIO));
        }
        let meta = self.stat_entry(from).await.map_err(opendal_error2errno)?;
        if meta.is_dir() {
            return Err(Errno::from(libc::EXDEV));
        }
        if self.op.info().capability().copy {
            return Ok(None);
        }
        let reader = self
            .op
            .reader(from)
            .await
            .map_err(opendal_error2errno)?
            .into_bytes_stream(..)
            .await
            .map_err(opendal_error2errno)?;
        Ok(Some(reader))
    }

    /// Returns (read, write, append) for open flags.
    fn check_flags(&self, flags: u32) -> Result<(bool, bool, bool)> {
        let is_append = flags & libc::O_APPEND as u32 != 0;
        let mode = flags & libc::O_ACCMODE as u32;
        let is_read = mode == libc::O_RDONLY as u32 || mode == libc::O_RDWR as u32;
        let is_write = mode == libc::O_WRONLY as u32 || mode == libc::O_RDWR as u32 || is_append;
        if !is_read && !is_write {
            Err(Errno::from(libc::EINVAL))?;
        }
        let capability = self.op.info().capability();
        if is_read && !capability.read {
            Err(Errno::from(libc::EACCES))?;
        }
        if is_write && !capability.write {
            Err(Errno::from(libc::EACCES))?;
        }
        tracing::trace!(
            "check_flags: is_read={is_read}, is_write={is_write}, is_append={is_append}"
        );
        Ok((is_read, is_write, is_append))
    }

    // Get opened file and check given path
    fn get_opened_file(
        &self,
        key: FileKey,
        path: Option<&OsStr>,
    ) -> Result<sharded_slab::Entry<'_, OpenedFile>> {
        // The file handle is authoritative. After a rename the kernel sends the
        // new path with the old handle, which is fine.
        let _ = path;
        self.opened_files.get(key.0).ok_or(Errno::from(libc::EBADF))
    }
}

const fn entry_mode2file_type(mode: EntryMode) -> FileType {
    match mode {
        EntryMode::DIR => FileType::Directory,
        _ => FileType::RegularFile,
    }
}

fn metadata2file_attr(metadata: &Metadata, atime: SystemTime, uid: u32, gid: u32) -> FileAttr {
    let last_modified = metadata.last_modified().map(|t| t.into()).unwrap_or(atime);
    let kind = entry_mode2file_type(metadata.mode());
    FileAttr {
        size: metadata.content_length(),
        mtime: last_modified,
        ctime: last_modified,
        ..dummy_file_attr(kind, atime, uid, gid)
    }
}

const fn dummy_file_attr(kind: FileType, now: SystemTime, uid: u32, gid: u32) -> FileAttr {
    FileAttr {
        size: 0,
        blocks: 0,
        atime: now,
        mtime: now,
        ctime: now,
        kind,
        perm: fuse3::perm_from_mode_and_kind(kind, 0o775),
        nlink: 0,
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        #[cfg(target_os = "macos")]
        crtime: now,
        #[cfg(target_os = "macos")]
        flags: 0,
    }
}

/// Object key of a path in the file system. The root is "".
///
/// Names that storage would change refuse: two different names must never
/// reach the same object. Invalid UTF-8 fails with EILSEQ. A path that OpenDAL
/// normalizes into another one (leading or trailing white space, empty parts)
/// or that has "." or ".." parts fails with EINVAL.
fn key_of(path: &OsStr) -> Result<String> {
    let raw = path.to_str().ok_or_else(|| Errno::from(libc::EILSEQ))?;
    let key = raw.trim_start_matches('/');
    if key.is_empty() {
        return Ok(String::new());
    }
    let dots = key.split('/').any(|part| part == "." || part == "..");
    if dots || normalize_path(key) != key {
        return Err(Errno::from(libc::EINVAL));
    }
    Ok(key.to_string())
}

/// Object key of a folder: the key with a trailing "/", or "/" for the root.
fn dir_key_of(path: &OsStr) -> Result<String> {
    let key = key_of(path)?;
    Ok(if key.is_empty() {
        "/".into()
    } else {
        format!("{key}/")
    })
}

fn upload_errno(result: std::result::Result<(), tonic::Status>) -> Result<()> {
    result.map_err(|err| {
        tracing::error!("upload failed: {}", err.message());
        Errno::from(libc::EIO)
    })
}

fn status_errno(err: tonic::Status) -> Errno {
    tracing::warn!("file operation failed: {}", err.message());
    Errno::from(match err.code() {
        tonic::Code::NotFound => libc::ENOENT,
        tonic::Code::PermissionDenied => libc::EACCES,
        tonic::Code::ResourceExhausted => libc::ENOSPC,
        _ => libc::EIO,
    })
}

fn io_errno(err: std::io::Error) -> Errno {
    tracing::warn!("spool file error: {err}");
    Errno::from(err.raw_os_error().unwrap_or(libc::EIO))
}

fn opendal_error2errno(err: opendal::Error) -> fuse3::Errno {
    tracing::trace!("opendal_error2errno: {err:?}");
    match err.kind() {
        ErrorKind::Unsupported => Errno::from(libc::EOPNOTSUPP),
        ErrorKind::IsADirectory => Errno::from(libc::EISDIR),
        ErrorKind::NotFound => Errno::from(libc::ENOENT),
        ErrorKind::PermissionDenied => Errno::from(libc::EACCES),
        ErrorKind::AlreadyExists => Errno::from(libc::EEXIST),
        ErrorKind::NotADirectory => Errno::from(libc::ENOTDIR),
        ErrorKind::RangeNotSatisfied => Errno::from(libc::EINVAL),
        ErrorKind::RateLimited => Errno::from(libc::EBUSY),
        _ => Errno::from(libc::ENOENT),
    }
}
