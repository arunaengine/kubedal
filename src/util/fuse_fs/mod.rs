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
/// is finished in `release` instead of `flush`, so writes through duplicated
/// file descriptors (like shell redirects) work.
pub struct Filesystem {
    op: Operator,
    gid: u32,
    uid: u32,

    opened_files: Slab<OpenedFile>,
}

impl Filesystem {
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

    /// Create a new filesystem with given operator, uid and gid.
    pub fn new(op: Operator, uid: u32, gid: u32) -> Self {
        Self {
            op,
            uid,
            gid,
            opened_files: Slab::new(),
        }
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
        if is_trunc && !capability.write {
            Err(Errno::from(libc::EACCES))?;
        }
        if is_append && !capability.write_can_append {
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
