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
        let entries: Vec<DirectoryEntry> = match self.dir_snapshot(path, fh, offset as u64).await? {
            Listing::Fresh(entries) => entries.into_iter().map(name_only).collect(),
            Listing::Kept(entries) => entries.as_ref().clone(),
        };
        let start = (offset as usize).min(entries.len());
        Ok(ReplyDirectory {
            entries: stream::iter(entries.into_iter().skip(start).map(Ok)).boxed(),
        })
    }

    pub(super) async fn readdirplus_impl<'a>(
        &'a self,
        req: Request,
        parent: &'a OsStr,
        fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> Result<
        ReplyDirectoryPlus<impl futures::Stream<Item = Result<DirectoryEntryPlus>> + Send + 'a>,
    > {
        tracing::debug!("readdirplus(parent={parent:?}, fh={fh}, offset={offset})");
        let kept = match self.dir_snapshot(parent, fh, offset).await? {
            Listing::Fresh(entries) => {
                let start = (offset as usize).min(entries.len());
                return Ok(ReplyDirectoryPlus {
                    entries: stream::iter(entries.into_iter().skip(start).map(Ok)).boxed(),
                });
            }
            Listing::Kept(entries) => entries,
        };
        // A kept listing holds only names. Attributes and type come fresh from
        // lookup, so a name that is now another kind of entry never gets the
        // old type; names that are gone are left out.
        let now = SystemTime::now();
        let dot_attr = dummy_file_attr(FileType::Directory, now, self.uid, self.gid);
        let start = (offset as usize).min(kept.len());
        let entries = stream::iter((start..kept.len()).map(move |i| kept[i].clone()))
            .then(move |e| async move {
                let (attr, ttl) = if e.name == "." || e.name == ".." {
                    (dot_attr, TTL)
                } else {
                    match self.lookup(req, parent, &e.name).await {
                        Ok(reply) => (reply.attr, reply.ttl),
                        Err(err) if err == Errno::from(libc::ENOENT) => return None,
                        Err(err) => return Some(Err(err)),
                    }
                };
                Some(Ok(DirectoryEntryPlus {
                    kind: attr.kind,
                    name: e.name,
                    offset: e.offset,
                    attr,
                    entry_ttl: ttl,
                    attr_ttl: ttl,
                }))
            })
            .filter_map(futures::future::ready);
        Ok(ReplyDirectoryPlus {
            entries: entries.boxed(),
        })
    }

    /// The listing of an open folder handle. A new listing is made when a
    /// listing starts (offset 0); its names are kept until `releasedir`, so all
    /// pages of one handle use the same names and offsets: names added or
    /// removed in between never shift them. Without a handle (`fh` 0), every
    /// call makes a new listing.
    async fn dir_snapshot(&self, path: &OsStr, fh: u64, offset: u64) -> Result<Listing> {
        if fh != 0
            && offset != 0
            && let Some(entries) = self.dir_snapshots().get(&fh)
        {
            return Ok(Listing::Kept(entries.clone()));
        }
        let entries = self.list_dir(path).await?;
        if fh != 0 {
            let names = entries.iter().cloned().map(name_only).collect();
            self.dir_snapshots().insert(fh, Arc::new(names));
        }
        Ok(Listing::Fresh(entries))
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

/// Most names that the snapshots of all open folder handles keep together.
pub(super) const MAX_SNAPSHOT_NAMES: usize = 1_000_000;

/// Names of open folder handles, see [`Filesystem::dir_snapshot`]. A handle
/// whose folder was removed may never see its `releasedir`, so the total
/// number of names is limited: the oldest snapshots go first. A handle that
/// lost its snapshot makes a new listing at its next call, and its paging may
/// then skip or repeat names.
pub(super) struct DirSnapshots {
    /// Snapshot and its age of each handle.
    map: HashMap<u64, (u64, Arc<Vec<DirectoryEntry>>)>,
    next_age: u64,
    names: usize,
    max_names: usize,
}

impl DirSnapshots {
    pub(super) fn new(max_names: usize) -> Self {
        Self {
            map: HashMap::new(),
            next_age: 0,
            names: 0,
            max_names,
        }
    }

    pub(super) fn get(&self, fh: &u64) -> Option<Arc<Vec<DirectoryEntry>>> {
        self.map.get(fh).map(|(_, entries)| entries.clone())
    }

    /// Keep the snapshot of `fh`. A snapshot larger than the limit is not kept.
    pub(super) fn insert(&mut self, fh: u64, entries: Arc<Vec<DirectoryEntry>>) {
        self.remove(&fh);
        if entries.len() > self.max_names {
            return;
        }
        while self.names + entries.len() > self.max_names {
            let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (age, _))| *age)
                .map(|(fh, _)| *fh)
            else {
                break;
            };
            self.remove(&oldest);
        }
        self.names += entries.len();
        self.map.insert(fh, (self.next_age, entries));
        self.next_age += 1;
    }

    pub(super) fn remove(&mut self, fh: &u64) {
        if let Some((_, entries)) = self.map.remove(fh) {
            self.names -= entries.len();
        }
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// A folder listing: made just now with attributes, or kept from the start of
/// the listing with names only.
enum Listing {
    Fresh(Vec<DirectoryEntryPlus>),
    Kept(Arc<Vec<DirectoryEntry>>),
}

fn name_only(e: DirectoryEntryPlus) -> DirectoryEntry {
    DirectoryEntry {
        kind: e.kind,
        name: e.name,
        offset: e.offset,
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

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    fn names(count: usize) -> Arc<Vec<DirectoryEntry>> {
        Arc::new(
            (0..count)
                .map(|i| DirectoryEntry {
                    kind: FileType::RegularFile,
                    name: i.to_string().into(),
                    offset: i as i64 + 1,
                })
                .collect(),
        )
    }

    #[test]
    fn the_oldest_snapshots_go_when_the_limit_is_reached() {
        let mut snapshots = DirSnapshots::new(10);
        snapshots.insert(1, names(4));
        snapshots.insert(2, names(4));
        snapshots.insert(3, names(4));
        assert!(snapshots.get(&1).is_none());
        assert!(snapshots.get(&2).is_some() && snapshots.get(&3).is_some());
        assert_eq!(snapshots.names, 8);
        // Too large to keep at all.
        snapshots.insert(4, names(11));
        assert!(snapshots.get(&4).is_none());
        snapshots.remove(&2);
        snapshots.remove(&3);
        assert!(snapshots.is_empty());
        assert_eq!(snapshots.names, 0);
    }
}
