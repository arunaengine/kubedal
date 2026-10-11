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
        let live = self.states.live_children(live_prefix(&path)).await;
        let dir = normalize_path(path.as_str());
        let mut entries = Vec::new();
        let mut lister = self.op.lister(&path).await.map_err(opendal_error2errno)?;
        while let Some(entry) = lister.next().await {
            let e = entry.map_err(opendal_error2errno)?;
            if e.path() == dir || has_live_name(&live, &e) {
                continue;
            }
            let name = e.name().trim_matches('/').to_string();
            entries.push((name, entry_mode2file_type(e.metadata().mode())));
        }
        entries.extend(
            live.into_iter()
                .map(|(name, _)| (name, FileType::RegularFile)),
        );
        // Sorted by name, the offset of an entry does not depend on whether it
        // is live or stored, so a listing can go on after a file is closed.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let children = stream::iter(entries.into_iter().enumerate().map(|(i, (name, kind))| {
            Ok(DirectoryEntry {
                kind,
                name: name.into(),
                offset: (i + 3) as i64,
            })
        }));

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
        let live = self.states.live_children(live_prefix(&path)).await;
        let dir = normalize_path(path.as_str());
        let mut entries = Vec::new();
        let mut lister = self
            .op
            .lister_with(&path)
            .await
            .map_err(opendal_error2errno)?;
        while let Some(entry) = lister.next().await {
            let e = entry.map_err(opendal_error2errno)?;
            if e.path() == dir || has_live_name(&live, &e) {
                continue;
            }
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
            entries.push(DirectoryEntryPlus {
                kind: entry_mode2file_type(metadata.mode()),
                name: e.name().trim_matches('/').into(),
                offset: 0,
                attr,
                entry_ttl: TTL,
                attr_ttl: TTL,
            });
        }
        entries.extend(live.into_iter().map(|(name, size)| DirectoryEntryPlus {
            kind: FileType::RegularFile,
            name: name.into(),
            offset: 0,
            attr: FileAttr {
                size,
                ..dummy_file_attr(FileType::RegularFile, now, uid, gid)
            },
            entry_ttl: TTL,
            attr_ttl: TTL,
        }));
        // Sorted by name, like readdir, so offsets depend only on the names.
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let children = stream::iter(entries.into_iter().enumerate().map(|(i, e)| {
            Ok(DirectoryEntryPlus {
                offset: (i + 3) as i64,
                ..e
            })
        }));

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
