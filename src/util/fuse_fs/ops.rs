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

use super::*;

impl PathFilesystem for Filesystem {
    // Init a fuse filesystem
    async fn init(&self, _req: Request) -> Result<ReplyInit> {
        Ok(ReplyInit {
            max_write: NonZeroU32::new(16 * 1024).unwrap(),
        })
    }

    // Callback when fs is being destroyed
    async fn destroy(&self, _req: Request) {}

    async fn lookup(&self, _req: Request, parent: &OsStr, name: &OsStr) -> Result<ReplyEntry> {
        tracing::debug!("lookup(parent={parent:?}, name={name:?})");

        let path = PathBuf::from(parent).join(name);
        let metadata = self
            .stat_entry(&path.to_string_lossy())
            .await
            .map_err(opendal_error2errno)?;

        let now = SystemTime::now();
        let attr = metadata2file_attr(&metadata, now, self.uid, self.gid);

        Ok(ReplyEntry { ttl: TTL, attr })
    }

    async fn getattr(
        &self,
        _req: Request,
        path: Option<&OsStr>,
        fh: Option<u64>,
        flags: u32,
    ) -> Result<ReplyAttr> {
        tracing::debug!("getattr(path={path:?}, fh={fh:?}, flags={flags:?})");

        let fh_path = fh.and_then(|fh| {
            self.opened_files
                .get(FileKey::try_from(fh).ok()?.0)
                .map(|f| f.path.clone())
        });

        let file_path = match (path.map(Into::into), fh_path) {
            (Some(a), Some(b)) => {
                if a != b {
                    Err(Errno::from(libc::EBADF))?;
                }
                Some(a)
            }
            (a, b) => a.or(b),
        };

        let metadata = self
            .stat_entry(&file_path.unwrap_or_default().to_string_lossy())
            .await
            .map_err(opendal_error2errno)?;

        let now = SystemTime::now();
        let attr = metadata2file_attr(&metadata, now, self.uid, self.gid);

        Ok(ReplyAttr { ttl: TTL, attr })
    }

    async fn setattr(
        &self,
        _req: Request,
        path: Option<&OsStr>,
        fh: Option<u64>,
        set_attr: SetAttr,
    ) -> Result<ReplyAttr> {
        tracing::debug!("setattr(path={path:?}, fh={fh:?}, set_attr={set_attr:?})");

        self.getattr(_req, path, fh, 0).await
    }

    async fn mkdir(
        &self,
        _req: Request,
        parent: &OsStr,
        name: &OsStr,
        mode: u32,
        _umask: u32,
    ) -> Result<ReplyEntry> {
        tracing::debug!("mkdir(parent={parent:?}, name={name:?}, mode=0o{mode:o})");

        let mut path = PathBuf::from(parent).join(name);
        path.push(""); // ref https://users.rust-lang.org/t/trailing-in-paths/43166
        self.op
            .create_dir(&path.to_string_lossy())
            .await
            .map_err(opendal_error2errno)?;

        let now = SystemTime::now();
        let attr = dummy_file_attr(FileType::Directory, now, self.uid, self.gid);

        Ok(ReplyEntry { ttl: TTL, attr })
    }

    async fn unlink(&self, _req: Request, parent: &OsStr, name: &OsStr) -> Result<()> {
        tracing::debug!("unlink(parent={parent:?}, name={name:?})");

        let path = PathBuf::from(parent).join(name);
        self.op
            .delete(&path.to_string_lossy())
            .await
            .map_err(opendal_error2errno)?;

        Ok(())
    }

    async fn rmdir(&self, _req: Request, parent: &OsStr, name: &OsStr) -> Result<()> {
        tracing::debug!("rmdir(parent={parent:?}, name={name:?})");

        let path = PathBuf::from(parent).join(name);
        self.op
            .delete(&path.to_string_lossy())
            .await
            .map_err(opendal_error2errno)?;

        Ok(())
    }

    async fn rename(
        &self,
        _req: Request,
        origin_parent: &OsStr,
        origin_name: &OsStr,
        parent: &OsStr,
        name: &OsStr,
    ) -> Result<()> {
        tracing::debug!(
            "rename(p={origin_parent:?}, name={origin_name:?}, newp={parent:?}, newname={name:?})"
        );

        if !self.op.info().capability().rename {
            Err(Errno::from(libc::ENOTSUP))?;
        }

        let origin_path = PathBuf::from(origin_parent).join(origin_name);
        let path = PathBuf::from(parent).join(name);

        self.op
            .rename(&origin_path.to_string_lossy(), &path.to_string_lossy())
            .await
            .map_err(opendal_error2errno)?;

        Ok(())
    }

    async fn opendir(&self, _req: Request, path: &OsStr, flags: u32) -> Result<ReplyOpen> {
        tracing::debug!("opendir(path={path:?}, flags=0x{flags:x})");
        Ok(ReplyOpen { fh: 0, flags })
    }

    async fn open(&self, _req: Request, path: &OsStr, flags: u32) -> Result<ReplyOpen> {
        tracing::debug!("open(path={path:?}, flags=0x{flags:x})");

        let (is_read, is_trunc, is_append) = self.check_flags(flags)?;
        if flags & libc::O_CREAT as u32 != 0 {
            self.op
                .write(&path.to_string_lossy(), Bytes::new())
                .await
                .map_err(opendal_error2errno)?;
        }

        let inner_writer = if is_trunc || is_append {
            let writer = self
                .op
                .writer_with(&path.to_string_lossy())
                .append(is_append)
                .await
                .map_err(opendal_error2errno)?;
            let written = if is_append {
                self.op
                    .stat(&path.to_string_lossy())
                    .await
                    .map_err(opendal_error2errno)?
                    .content_length()
            } else {
                0
            };
            Some(Arc::new(Mutex::new(InnerWriter { writer, written })))
        } else {
            None
        };

        let size = if is_read && inner_writer.is_none() {
            let meta = self
                .op
                .stat(&path.to_string_lossy())
                .await
                .map_err(opendal_error2errno)?;
            Some(meta.content_length())
        } else {
            None
        };

        let key = self
            .opened_files
            .insert(OpenedFile {
                path: path.into(),
                is_read,
                inner_writer,
                size,
            })
            .ok_or(Errno::from(libc::EBUSY))?;

        Ok(ReplyOpen {
            fh: FileKey(key).to_fh(),
            flags,
        })
    }

    async fn read(
        &self,
        _req: Request,
        path: Option<&OsStr>,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<ReplyData> {
        tracing::debug!("read(path={path:?}, fh={fh}, offset={offset}, size={size})");

        let (file_path, file_size) = {
            let file = self.get_opened_file(FileKey::try_from(fh)?, path)?;
            if !file.is_read {
                Err(Errno::from(libc::EACCES))?;
            }
            (file.path.to_string_lossy().to_string(), file.size)
        };

        let mut end = offset.saturating_add(u64::from(size));
        if let Some(file_size) = file_size {
            if offset >= file_size {
                return Ok(ReplyData { data: Bytes::new() });
            }
            end = end.min(file_size);
        }
        let result = self.op.read_with(&file_path).range(offset..end).await;
        let data = match result {
            Ok(buffer) => buffer.to_bytes(),
            // A read at or after the end of the file returns no data.
            Err(err) if err.kind() == ErrorKind::RangeNotSatisfied => Bytes::new(),
            Err(err) => return Err(opendal_error2errno(err)),
        };

        Ok(ReplyData { data })
    }

    async fn write(
        &self,
        _req: Request,
        path: Option<&OsStr>,
        fh: u64,
        offset: u64,
        data: &[u8],
        _write_flags: u32,
        flags: u32,
    ) -> Result<ReplyWrite> {
        tracing::debug!(
            "write(path={:?}, fh={}, offset={}, data_len={}, flags=0x{:x})",
            path,
            fh,
            offset,
            data.len(),
            flags
        );

        let Some(inner_writer) = ({
            self.get_opened_file(FileKey::try_from(fh)?, path)?
                .inner_writer
                .clone()
        }) else {
            Err(Errno::from(libc::EACCES))?
        };

        let mut inner = inner_writer.lock().await;
        // OpenDAL doesn't support random write
        if offset != inner.written {
            Err(Errno::from(libc::EINVAL))?;
        }

        inner
            .writer
            .write_from(data)
            .await
            .map_err(opendal_error2errno)?;
        inner.written += data.len() as u64;

        Ok(ReplyWrite {
            written: data.len() as _,
        })
    }

    async fn release(
        &self,
        _req: Request,
        path: Option<&OsStr>,
        fh: u64,
        flags: u32,
        lock_owner: u64,
        flush: bool,
    ) -> Result<()> {
        tracing::debug!(
            "release(path={path:?}, fh={fh}, flags=0x{flags:x}, lock_owner={lock_owner}, flush={flush})"
        );

        let file = self
            .opened_files
            .take(FileKey::try_from(fh)?.0)
            .ok_or(Errno::from(libc::EBADF))?;

        // Release comes once, after the last file descriptor is closed. Only then
        // no more writes can follow, so the object is finished here.
        if let Some(inner_writer) = file.inner_writer {
            let mut lock = inner_writer.lock().await;
            if let Err(err) = lock.writer.close().await {
                tracing::error!("finishing {:?} failed: {err}", file.path);
                return Err(opendal_error2errno(err));
            }
        }
        Ok(())
    }

    /// Flush is called on every close of a file descriptor, also for duplicated
    /// descriptors. More writes can follow, so the writer stays open until release.
    async fn flush(
        &self,
        _req: Request,
        path: Option<&OsStr>,
        fh: u64,
        lock_owner: u64,
    ) -> Result<()> {
        tracing::debug!("flush(path={path:?}, fh={fh}, lock_owner={lock_owner})");
        self.get_opened_file(FileKey::try_from(fh)?, path)?;
        Ok(())
    }

    async fn readdir<'a>(
        &'a self,
        _req: Request,
        path: &'a OsStr,
        fh: u64,
        offset: i64,
    ) -> Result<ReplyDirectory<impl futures::Stream<Item = Result<DirectoryEntry>> + Send + 'a>>
    {
        self.readdir_impl(_req, path, fh, offset).await
    }

    async fn access(&self, _req: Request, path: &OsStr, mask: u32) -> Result<()> {
        tracing::debug!("access(path={path:?}, mask=0x{mask:x})");

        self.stat_entry(&path.to_string_lossy())
            .await
            .map_err(opendal_error2errno)?;

        Ok(())
    }

    async fn create(
        &self,
        _req: Request,
        parent: &OsStr,
        name: &OsStr,
        mode: u32,
        flags: u32,
    ) -> Result<ReplyCreated> {
        tracing::debug!(
            "create(parent={parent:?}, name={name:?}, mode=0o{mode:o}, flags=0x{flags:x})"
        );

        let (is_read, is_trunc, is_append) = self.check_flags(flags | libc::O_CREAT as u32)?;

        let path = PathBuf::from(parent).join(name);

        let inner_writer = if is_trunc || is_append {
            let writer = self
                .op
                .writer_with(&path.to_string_lossy())
                .chunk(4 * 1024 * 1024)
                .append(is_append)
                .await
                .map_err(opendal_error2errno)?;
            Some(Arc::new(Mutex::new(InnerWriter { writer, written: 0 })))
        } else {
            None
        };

        let now = SystemTime::now();
        let attr = dummy_file_attr(FileType::RegularFile, now, self.uid, self.gid);

        let key = self
            .opened_files
            .insert(OpenedFile {
                path: path.into(),
                is_read,
                inner_writer,
                size: None,
            })
            .ok_or(Errno::from(libc::EBUSY))?;

        Ok(ReplyCreated {
            ttl: TTL,
            attr,
            generation: 0,
            fh: FileKey(key).to_fh(),
            flags,
        })
    }

    async fn readdirplus<'a>(
        &'a self,
        _req: Request,
        parent: &'a OsStr,
        fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> Result<
        ReplyDirectoryPlus<impl futures::Stream<Item = Result<DirectoryEntryPlus>> + Send + 'a>,
    > {
        self.readdirplus_impl(_req, parent, fh, offset, _lock_owner)
            .await
    }

    async fn rename2(
        &self,
        req: Request,
        origin_parent: &OsStr,
        origin_name: &OsStr,
        parent: &OsStr,
        name: &OsStr,
        _flags: u32,
    ) -> Result<()> {
        tracing::debug!(
            "rename2(origin_parent={origin_parent:?}, origin_name={origin_name:?}, parent={parent:?}, name={name:?})"
        );
        self.rename(req, origin_parent, origin_name, parent, name)
            .await
    }

    async fn statfs(&self, _req: Request, path: &OsStr) -> Result<ReplyStatFs> {
        tracing::debug!("statfs(path={path:?})");
        Ok(ReplyStatFs {
            blocks: 1,
            bfree: 0,
            bavail: 0,
            files: 1,
            ffree: 0,
            bsize: 4096,
            namelen: u32::MAX,
            frsize: 0,
        })
    }
}
