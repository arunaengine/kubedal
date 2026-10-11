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

        let path = dir_key_of(path)?;
        // Files that are written but not uploaded yet are listed too, once.
        let live = Arc::new(self.states.live_children(live_prefix(&path)).await);
        let live_names = live.clone();
        let children = self
            .op
            .lister(&path)
            .await
            .map_err(opendal_error2errno)?
            .filter_map(move |entry| {
                let dir = normalize_path(path.as_str());
                let live = live_names.clone();
                async move {
                    match entry {
                        Ok(e) if e.path() == dir => None,
                        Ok(e) if has_live_name(&live, &e) => None,
                        _ => Some(entry),
                    }
                }
            })
            .map(|entry| {
                entry
                    .map(|e| {
                        let name = e.name().trim_matches('/').into();
                        (entry_mode2file_type(e.metadata().mode()), name)
                    })
                    .map_err(opendal_error2errno)
            })
            .chain(stream::iter((0..live.len()).map(move |i| {
                Ok((FileType::RegularFile, live[i].0.clone().into()))
            })))
            .enumerate()
            .map(|(i, entry)| {
                entry.map(|(kind, name)| DirectoryEntry {
                    kind,
                    name,
                    offset: (i + 3) as i64,
                })
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
        let uid = self.uid;
        let gid = self.gid;

        let path = dir_key_of(parent)?;
        // Files that are written but not uploaded yet are listed too, once,
        // with their live size.
        let live = Arc::new(self.states.live_children(live_prefix(&path)).await);
        let live_names = live.clone();
        let live_entries = stream::iter((0..live.len()).map(move |i| {
            let (name, size) = &live[i];
            Ok(DirectoryEntryPlus {
                kind: FileType::RegularFile,
                name: name.clone().into(),
                offset: 0,
                attr: FileAttr {
                    size: *size,
                    ..dummy_file_attr(FileType::RegularFile, now, uid, gid)
                },
                entry_ttl: TTL,
                attr_ttl: TTL,
            })
        }));
        let children = self
            .op
            .lister_with(&path)
            .await
            .map_err(opendal_error2errno)?
            .filter_map(move |entry| {
                let dir = normalize_path(path.as_str());
                let live = live_names.clone();
                async move {
                    match entry {
                        Ok(e) if e.path() == dir => None,
                        Ok(e) if has_live_name(&live, &e) => None,
                        _ => Some(entry),
                    }
                }
            })
            .then(move |entry| async move {
                let e = entry.map_err(opendal_error2errno)?;
                let metadata = e.metadata();
                let mut attr = metadata2file_attr(metadata, now, uid, gid);
                // Like getattr: a file that is being written has its local size.
                // The kernel applies these attributes to its cached inode.
                if !metadata.is_dir()
                    && let Some(size) = self.states.size(e.path()).await
                {
                    attr = FileAttr {
                        size,
                        ..dummy_file_attr(FileType::RegularFile, now, uid, gid)
                    };
                }
                Ok(DirectoryEntryPlus {
                    kind: entry_mode2file_type(metadata.mode()),
                    name: e.name().trim_matches('/').into(),
                    offset: 0,
                    attr,
                    entry_ttl: TTL,
                    attr_ttl: TTL,
                })
            })
            .chain(live_entries)
            .enumerate()
            .map(|(i, entry)| {
                entry.map(|e| DirectoryEntryPlus {
                    offset: (i + 3) as i64,
                    ..e
                })
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

/// Prefix of the state keys in the folder with object key `dir`.
fn live_prefix(dir: &str) -> &str {
    if dir == "/" { "" } else { dir }
}

/// True if a listed object (a file or a folder) has the name of a live state.
/// The name is listed once, from the state, like lookup finds the live file.
fn has_live_name(live: &[(String, u64)], entry: &opendal::Entry) -> bool {
    let name = entry.name().trim_end_matches('/');
    live.iter().any(|(live_name, _)| live_name == name)
}
