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
pub mod spool;
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
use tokio::sync::Mutex;

use file::FileKey;
use file::OpenedFile;
use spool::{Spool, WriteState};

const TTL: Duration = Duration::from_secs(1); // 1 second

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
    /// Shared spool of files that are open for writing.
    spool: Arc<Spool>,

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
                writer.lock().await.set_len(size).map_err(io_errno)?;
                return Ok(());
            }
        }
        if let Some(state) = path.and_then(|p| self.spool.get(&key_of(p))) {
            // Another handle writes this file: truncate the shared content.
            state.lock().await.set_len(size).map_err(io_errno)?;
            return Ok(());
        }
        let path = match (path, fh) {
            (Some(path), _) => path.to_string_lossy().to_string(),
            (None, Some(fh)) => self
                .get_opened_file(FileKey::try_from(fh)?, None)?
                .path
                .to_string_lossy()
                .to_string(),
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
        Self {
            op,
            uid,
            gid,
            spool: Arc::new(Spool::new(spool_dir)),
            opened_files: Slab::new(),
        }
    }

    /// Size of the spool content of `path`, if the file is being written.
    async fn spool_size(&self, path: &OsStr) -> Option<u64> {
        let state = self.spool.get(&key_of(path))?;
        let state = state.lock().await;
        (!state.deleted).then_some(state.len)
    }

    /// Get the shared write state of `path` for a new handle. With `keep`, a new
    /// state starts with the current object content (streamed in chunks);
    /// without it, the content starts empty (O_TRUNC or a new file).
    async fn open_write(&self, path: &OsStr, keep: bool) -> Result<Arc<Mutex<WriteState>>> {
        let key = key_of(path);
        let opened = self.spool.open(&key).map_err(io_errno)?;
        if let Some(mut state) = opened.new_guard {
            if keep {
                match state.fill_from(&self.op).await {
                    Ok(()) => {}
                    // A missing object is a new, empty file.
                    Err(err) if err.code() == tonic::Code::NotFound => state.dirty = true,
                    Err(err) => {
                        tracing::warn!("cannot read {key} for writing: {}", err.message());
                        state.remove_files();
                        drop(state);
                        self.spool.forget(&key, &opened.state);
                        return Err(Errno::from(libc::EIO));
                    }
                }
            }
            state.handles += 1;
        } else {
            let mut state = opened.state.lock().await;
            if !keep {
                state.set_len(0).map_err(io_errno)?;
            }
            state.handles += 1;
        }
        Ok(opened.state)
    }

    /// Upload the content of the open file `fh`, if it is open for writing.
    async fn upload_open_file(&self, path: Option<&OsStr>, fh: u64) -> Result<()> {
        let state = self
            .get_opened_file(FileKey::try_from(fh)?, path)?
            .write
            .clone();
        match state {
            Some(state) => upload_errno(state.lock().await.upload(&self.op).await),
            None => Ok(()),
        }
    }

    /// A handle closes. The last handle uploads the content. If that fails, the
    /// spool files stay on the node, a background task retries, and unpublish
    /// uploads whatever is still left.
    async fn release_state(&self, state: Arc<Mutex<WriteState>>) -> Result<()> {
        let mut guard = state.lock().await;
        guard.handles = guard.handles.saturating_sub(1);
        let result = guard.upload(&self.op).await;
        if guard.handles == 0 {
            if !guard.dirty || guard.deleted {
                guard.remove_files();
                let key = guard.key.clone();
                drop(guard);
                self.spool.forget(&key, &state);
            } else {
                drop(guard);
                self.retry_upload(state);
            }
        }
        upload_errno(result)
    }

    /// Retry the upload of a closed file a few times in the background.
    fn retry_upload(&self, state: Arc<Mutex<WriteState>>) {
        let op = self.op.clone();
        let spool = self.spool.clone();
        tokio::spawn(async move {
            for attempt in 0..5u32 {
                tokio::time::sleep(Duration::from_secs(2u64 << attempt)).await;
                let mut guard = state.lock().await;
                if guard.handles > 0 {
                    // A new handle took over; it uploads on close.
                    return;
                }
                if guard.deleted || guard.upload(&op).await.is_ok() {
                    guard.remove_files();
                    let key = guard.key.clone();
                    drop(guard);
                    spool.forget(&key, &state);
                    return;
                }
            }
            tracing::error!("upload keeps failing; the content stays on the node until unpublish");
        });
    }

    /// Mark the write state of `path` as deleted. Returns true if there was one.
    async fn forget_state(&self, path: &OsStr) -> bool {
        let key = key_of(path);
        let Some(state) = self.spool.get(&key) else {
            return false;
        };
        state.lock().await.mark_deleted();
        self.spool.forget(&key, &state);
        true
    }

    /// Move the write state of `from` to `to`, if `from` has one. The old object
    /// is removed; the content is uploaded under the new key on the next flush.
    async fn rename_state(&self, from: &OsStr, to: &OsStr) -> Result<bool> {
        let (from_key, to_key) = (key_of(from), key_of(to));
        let (moved, replaced) = self.spool.rename(&from_key, &to_key);
        let Some(moved) = moved else {
            return Ok(false);
        };
        if let Some(replaced) = replaced {
            replaced.lock().await.mark_deleted();
        }
        moved.lock().await.set_key(&to_key).map_err(io_errno)?;
        match self.op.delete(&from_key).await {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(opendal_error2errno(err)),
        }
        Ok(true)
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
        let file = self
            .opened_files
            .get(key.0)
            .ok_or(Errno::from(libc::ENOENT))?;

        if matches!(path, Some(path) if path != file.path) {
            tracing::trace!(
                "get_opened_file: path not match: path={:?}, file={:?}",
                path,
                file.path
            );
            Err(Errno::from(libc::EBADF))?;
        }

        Ok(file)
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

/// Object key of a path in the file system.
fn key_of(path: &OsStr) -> String {
    path.to_string_lossy().trim_start_matches('/').to_string()
}

fn upload_errno(result: std::result::Result<(), tonic::Status>) -> Result<()> {
    result.map_err(|err| {
        tracing::error!("upload failed: {}", err.message());
        Errno::from(libc::EIO)
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
