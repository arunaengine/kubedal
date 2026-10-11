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
        let entries = self.dir_snapshot(path, fh, offset as u64).await?;
        let start = (offset as usize).min(entries.len());
        Ok(ReplyDirectory {
            entries: stream::iter((start..entries.len()).map(move |i| {
                let e = &entries[i];
                Ok(DirectoryEntry {
                    kind: e.kind,
                    name: e.name.clone(),
                    offset: e.offset,
                })
            }))
            .boxed(),
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
        let entries = self.dir_snapshot(parent, fh, offset).await?;
        let start = (offset as usize).min(entries.len());
        Ok(ReplyDirectoryPlus {
            entries: stream::iter((start..entries.len()).map(move |i| Ok(entries[i].clone())))
                .boxed(),
        })
    }

    /// The listing of an open folder handle. It is made when a listing starts
    /// (offset 0) and kept until `releasedir`, so all pages of one handle come
    /// from the same listing: names added or removed in between never shift
    /// the offsets. Without a handle (`fh` 0), every call makes a new listing.
    async fn dir_snapshot(
        &self,
        path: &OsStr,
        fh: u64,
        offset: u64,
    ) -> Result<Arc<Vec<DirectoryEntryPlus>>> {
        if fh != 0
            && offset != 0
            && let Some(entries) = self.dir_snapshots().get(&fh)
        {
            return Ok(entries.clone());
        }
        let entries = Arc::new(self.list_dir(path).await?);
        if fh != 0 {
            self.dir_snapshots().insert(fh, entries.clone());
        }
        Ok(entries)
    }

    /// List a folder with ".", ".." and offsets that start at 1.
    async fn list_dir(&self, parent: &OsStr) -> Result<Vec<DirectoryEntryPlus>> {
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
        // Sorted by name, the offset of an entry does not depend on whether it
        // is live or stored. A name is listed once; like lookup, a file wins
        // over a folder.
        entries.sort_by(|a, b| {
            let is_dir = |e: &DirectoryEntryPlus| e.kind == FileType::Directory;
            a.name.cmp(&b.name).then(is_dir(a).cmp(&is_dir(b)))
        });
        entries.dedup_by(|a, b| a.name == b.name);

        let relative = |name: &str| DirectoryEntryPlus {
            kind: FileType::Directory,
            name: name.into(),
            offset: 0,
            attr: dummy_file_attr(FileType::Directory, now, uid, gid),
            entry_ttl: TTL,
            attr_ttl: TTL,
        };
        Ok([relative("."), relative("..")]
            .into_iter()
            .chain(entries)
            .enumerate()
            .map(|(i, e)| DirectoryEntryPlus {
                offset: (i + 1) as i64,
                ..e
            })
            .collect())
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
