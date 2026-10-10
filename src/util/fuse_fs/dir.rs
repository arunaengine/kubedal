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

impl Filesystem {
    pub(super) async fn readdir_impl<'a>(
        &'a self,
        _req: Request,
        path: &'a OsStr,
        fh: u64,
        offset: i64,
    ) -> Result<ReplyDirectory<impl futures::Stream<Item = Result<DirectoryEntry>> + Send + 'a>>
    {
        tracing::debug!("readdir(path={path:?}, fh={fh}, offset={offset})");

        let mut current_dir = PathBuf::from(path);
        current_dir.push(""); // ref https://users.rust-lang.org/t/trailing-in-paths/43166
        let path = current_dir.to_string_lossy().to_string();
        let children = self
            .op
            .lister(&current_dir.to_string_lossy())
            .await
            .map_err(opendal_error2errno)?
            .filter_map(move |entry| {
                let dir = normalize_path(path.as_str());
                async move {
                    match entry {
                        Ok(e) if e.path() == dir => None,
                        _ => Some(entry),
                    }
                }
            })
            .enumerate()
            .map(|(i, entry)| {
                entry
                    .map(|e| DirectoryEntry {
                        kind: entry_mode2file_type(e.metadata().mode()),
                        name: e.name().trim_matches('/').into(),
                        offset: (i + 3) as i64,
                    })
                    .map_err(opendal_error2errno)
            });

        let relative_paths = stream::iter([
            Result::Ok(DirectoryEntry {
                kind: FileType::Directory,
                name: ".".into(),
                offset: 1,
            }),
            Result::Ok(DirectoryEntry {
                kind: FileType::Directory,
                name: "..".into(),
                offset: 2,
            }),
        ]);

        Ok(ReplyDirectory {
            entries: relative_paths.chain(children).skip(offset as usize).boxed(),
        })
    }

    pub(super) async fn readdirplus_impl<'a>(
        &'a self,
        _req: Request,
        parent: &'a OsStr,
        fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> Result<
        ReplyDirectoryPlus<impl futures::Stream<Item = Result<DirectoryEntryPlus>> + Send + 'a>,
    > {
        tracing::debug!("readdirplus(parent={parent:?}, fh={fh}, offset={offset})");

        let now = SystemTime::now();
        let mut current_dir = PathBuf::from(parent);
        current_dir.push(""); // ref https://users.rust-lang.org/t/trailing-in-paths/43166
        let uid = self.uid;
        let gid = self.gid;

        let path = current_dir.to_string_lossy().to_string();
        let children = self
            .op
            .lister_with(&path)
            .await
            .map_err(opendal_error2errno)?
            .filter_map(move |entry| {
                let dir = normalize_path(path.as_str());
                async move {
                    match entry {
                        Ok(e) if e.path() == dir => None,
                        _ => Some(entry),
                    }
                }
            })
            .enumerate()
            .map(move |(i, entry)| {
                entry
                    .map(|e| {
                        let metadata = e.metadata();
                        DirectoryEntryPlus {
                            kind: entry_mode2file_type(metadata.mode()),
                            name: e.name().trim_matches('/').into(),
                            offset: (i + 3) as i64,
                            attr: metadata2file_attr(metadata, now, uid, gid),
                            entry_ttl: TTL,
                            attr_ttl: TTL,
                        }
                    })
                    .map_err(opendal_error2errno)
            });

        let relative_path_attr = dummy_file_attr(FileType::Directory, now, uid, gid);
        let relative_paths = stream::iter([
            Result::Ok(DirectoryEntryPlus {
                kind: FileType::Directory,
                name: ".".into(),
                offset: 1,
                attr: relative_path_attr,
                entry_ttl: TTL,
                attr_ttl: TTL,
            }),
            Result::Ok(DirectoryEntryPlus {
                kind: FileType::Directory,
                name: "..".into(),
                offset: 2,
                attr: relative_path_attr,
                entry_ttl: TTL,
                attr_ttl: TTL,
            }),
        ]);

        Ok(ReplyDirectoryPlus {
            entries: relative_paths.chain(children).skip(offset as usize).boxed(),
        })
    }
}
