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
use file::InnerWriter;
use file::OpenedFile;

const TTL: Duration = Duration::from_secs(1); // 1 second

/// `Filesystem` implements [`PathFilesystem`] on top of an OpenDAL [`Operator`].
///
/// This code is vendored from the `fuse3_opendal` crate (Apache-2.0), because that
/// crate does not yet support the OpenDAL and fuse3 versions used by kubedal.
/// Changes: adapted to opendal 0.59 and fuse3 0.9; `read` only fetches the
/// requested byte range and stops at the end of the file; the writer of a file
/// is replaced by a local spool file that is uploaded on `flush`, `fsync` and
/// `release`, so upload errors reach the program and writes through duplicated
/// file descriptors (like shell redirects) work.
pub struct Filesystem {
    op: Operator,
    gid: u32,
    uid: u32,
    /// Folder for the spool files of files opened for writing.
    spool_dir: PathBuf,

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
                .inner_writer
                .clone();
            if let Some(writer) = writer {
                // An open file is a local spool file, so any size works.
                let mut inner = writer.lock().await;
                inner.spool.set_len(size).map_err(io_errno)?;
                inner.written = size;
                inner.dirty = true;
                return Ok(());
            }
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
            spool_dir,
            opened_files: Slab::new(),
        }
    }

    /// A new empty spool file, removed from the folder right away.
    fn new_writer(&self) -> Result<Arc<Mutex<InnerWriter>>> {
        std::fs::create_dir_all(&self.spool_dir).map_err(io_errno)?;
        let spool = tempfile::tempfile_in(&self.spool_dir).map_err(io_errno)?;
        Ok(Arc::new(Mutex::new(InnerWriter {
            spool,
            written: 0,
            dirty: true,
        })))
    }

    /// Upload the spool file of the open file `fh`, if it has one.
    async fn upload_open_file(&self, path: Option<&OsStr>, fh: u64) -> Result<()> {
        let (file_path, writer) = {
            let file = self.get_opened_file(FileKey::try_from(fh)?, path)?;
            (file.path.clone(), file.inner_writer.clone())
        };
        match writer {
            Some(writer) => self.upload_writer(&file_path, &writer).await,
            None => Ok(()),
        }
    }

    /// Upload the spool file of an open file if it changed since the last upload.
    async fn upload_writer(&self, path: &OsStr, writer: &Mutex<InnerWriter>) -> Result<()> {
        let mut inner = writer.lock().await;
        if !inner.dirty {
            return Ok(());
        }
        let mut file = inner.spool.try_clone().map_err(io_errno)?;
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0)).map_err(io_errno)?;
        let key = path.to_string_lossy();
        let key = key.trim_start_matches('/');
        crate::node::upload::upload_file(&self.op, tokio::fs::File::from_std(file), key, key)
            .await
            .map_err(|err| {
                tracing::error!("upload of {key} failed: {}", err.message());
                Errno::from(libc::EIO)
            })?;
        inner.dirty = false;
        Ok(())
    }

    fn check_flags(&self, flags: u32) -> Result<(bool, bool, bool)> {
        let is_trunc = flags & libc::O_TRUNC as u32 != 0 || flags & libc::O_CREAT as u32 != 0;
        let is_append = flags & libc::O_APPEND as u32 != 0;

        let mode = flags & libc::O_ACCMODE as u32;
        let is_read = mode == libc::O_RDONLY as u32 || mode == libc::O_RDWR as u32;
        let is_write = mode == libc::O_WRONLY as u32 || mode == libc::O_RDWR as u32 || is_append;
        if !is_read && !is_write {
            Err(Errno::from(libc::EINVAL))?;
        }
        // OpenDAL only supports truncate write and append write,
        // so O_TRUNC or O_APPEND needs to be specified explicitly
        if (is_write && !is_trunc && !is_append) || is_trunc && !is_write {
            Err(Errno::from(libc::EINVAL))?;
        }

        let capability = self.op.info().capability();
        if is_read && !capability.read {
            Err(Errno::from(libc::EACCES))?;
        }
        if (is_trunc || is_append) && !capability.write {
            Err(Errno::from(libc::EACCES))?;
        }

        tracing::trace!(
            "check_flags: is_read={is_read}, is_write={is_write}, is_trunc={is_trunc}, is_append={is_append}"
        );
        Ok((is_read, is_trunc, is_append))
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
