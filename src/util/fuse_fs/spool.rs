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

//! Shared state of files that are open for writing (kubedal addition).
//!
//! All handles that write the same object share one spool file on the node.
//! The spool file is a normal named file next to a small JSON file with the
//! object key, so content that could not be uploaded survives a restart of the
//! driver and is uploaded later by [`upload_pending`].

use futures::StreamExt;
use opendal::Operator;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tonic::Status;

/// Content of the JSON file next to a spool file.
#[derive(Debug, Serialize, Deserialize)]
struct SpoolMeta {
    /// Object key, relative to the root of the FUSE file system.
    key: String,
}

/// The shared state of one object that is open for writing.
pub struct WriteState {
    /// Object key the content belongs to.
    pub key: String,
    file: File,
    data_path: PathBuf,
    meta_path: PathBuf,
    /// Size of the content.
    pub len: u64,
    /// The spool file has content that is not uploaded yet.
    pub dirty: bool,
    /// The name was removed (unlink, or replaced by a rename). The content is
    /// kept for open handles but never uploaded.
    pub deleted: bool,
    /// Number of open handles.
    pub handles: usize,
}

impl WriteState {
    fn create(dir: &Path, key: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let data_path = dir.join(format!("{id}.data"));
        let meta_path = dir.join(format!("{id}.json"));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&data_path)?;
        let mut state = Self {
            key: key.to_string(),
            file,
            data_path,
            meta_path,
            len: 0,
            dirty: true,
            deleted: false,
            handles: 0,
        };
        state.write_meta()?;
        Ok(state)
    }

    fn write_meta(&mut self) -> std::io::Result<()> {
        let data = serde_json::to_vec(&SpoolMeta {
            key: self.key.clone(),
        })
        .map_err(std::io::Error::other)?;
        let tmp = self.meta_path.with_extension("json.tmp");
        let mut file = File::create(&tmp)?;
        file.write_all(&data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &self.meta_path)
    }

    pub fn write_at(&mut self, data: &[u8], offset: u64) -> std::io::Result<()> {
        self.file.write_all_at(data, offset)?;
        self.len = self.len.max(offset + data.len() as u64);
        self.dirty = true;
        Ok(())
    }

    pub fn read_at(&self, offset: u64, size: u32) -> std::io::Result<Vec<u8>> {
        if offset >= self.len {
            return Ok(Vec::new());
        }
        let size = (u64::from(size)).min(self.len - offset) as usize;
        let mut buf = vec![0u8; size];
        self.file.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }

    pub fn set_len(&mut self, len: u64) -> std::io::Result<()> {
        self.file.set_len(len)?;
        self.len = len;
        self.dirty = true;
        Ok(())
    }

    /// Point the content to another object key (after a rename).
    pub fn set_key(&mut self, key: &str) -> std::io::Result<()> {
        self.key = key.to_string();
        self.dirty = true;
        if self.deleted {
            return Ok(());
        }
        self.write_meta()
    }

    /// The name is gone: never upload this content again.
    pub fn mark_deleted(&mut self) {
        self.deleted = true;
        self.dirty = false;
        if let Err(err) = std::fs::remove_file(&self.meta_path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("cannot remove {:?}: {err}", self.meta_path);
        }
    }

    /// Copy the current object content into the spool file, in chunks.
    pub async fn fill_from(&mut self, op: &Operator) -> Result<(), Status> {
        let key = self.key.clone();
        let mut stream = op
            .reader(&key)
            .await
            .map_err(|e| crate::util::error::opendal_status(e, &format!("open {key}")))?
            .into_bytes_stream(..)
            .await
            .map_err(|e| crate::util::error::opendal_status(e, &format!("read {key}")))?;
        let mut offset = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| crate::util::error::io_status(e, &format!("read {key}")))?;
            self.file
                .write_all_at(&chunk, offset)
                .map_err(|e| crate::util::error::io_status(e, "write spool file"))?;
            offset += chunk.len() as u64;
        }
        self.len = offset;
        self.dirty = false;
        Ok(())
    }

    /// Upload the content if it changed since the last upload.
    pub async fn upload(&mut self, op: &Operator) -> Result<(), Status> {
        if !self.dirty || self.deleted {
            return Ok(());
        }
        upload_data_file(op, &self.data_path, &self.key).await?;
        self.dirty = false;
        Ok(())
    }

    /// Remove the spool files from disk.
    pub fn remove_files(&self) {
        for path in [&self.data_path, &self.meta_path] {
            if let Err(err) = std::fs::remove_file(path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!("cannot remove {path:?}: {err}");
            }
        }
    }
}

async fn upload_data_file(op: &Operator, data_path: &Path, key: &str) -> Result<(), Status> {
    let file = tokio::fs::File::open(data_path)
        .await
        .map_err(|e| crate::util::error::io_status(e, "open spool file"))?;
    crate::node::upload::upload_file(op, file, key, key).await?;
    Ok(())
}

/// All write states of one FUSE file system, keyed by object key.
pub struct Spool {
    dir: PathBuf,
    states: std::sync::Mutex<HashMap<String, Arc<Mutex<WriteState>>>>,
}

/// Result of [`Spool::open`].
pub struct Opened {
    pub state: Arc<Mutex<WriteState>>,
    /// Set when the state was just created. The caller holds its lock and may
    /// fill it before anybody else can use it.
    pub new_guard: Option<OwnedMutexGuard<WriteState>>,
}

impl Spool {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            states: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn states(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Mutex<WriteState>>>> {
        self.states.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The shared state of `key`, if a handle writes it or it waits for upload.
    pub fn get(&self, key: &str) -> Option<Arc<Mutex<WriteState>>> {
        self.states().get(key).cloned()
    }

    /// The shared state of `key`, created if needed.
    pub fn open(&self, key: &str) -> std::io::Result<Opened> {
        let mut states = self.states();
        if let Some(state) = states.get(key) {
            return Ok(Opened {
                state: state.clone(),
                new_guard: None,
            });
        }
        let state = Arc::new(Mutex::new(WriteState::create(&self.dir, key)?));
        let guard = state
            .clone()
            .try_lock_owned()
            .map_err(|_| std::io::Error::other("new spool state is locked"))?;
        states.insert(key.to_string(), state.clone());
        Ok(Opened {
            state,
            new_guard: Some(guard),
        })
    }

    /// Drop the state of `key` from the map, if it is still this state.
    pub fn forget(&self, key: &str, state: &Arc<Mutex<WriteState>>) {
        let mut states = self.states();
        if states.get(key).is_some_and(|s| Arc::ptr_eq(s, state)) {
            states.remove(key);
        }
    }

    /// Move the state of `from` to `to`. Returns the moved state and the state
    /// that was replaced at `to`, if any.
    #[allow(clippy::type_complexity)]
    pub fn rename(
        &self,
        from: &str,
        to: &str,
    ) -> (
        Option<Arc<Mutex<WriteState>>>,
        Option<Arc<Mutex<WriteState>>>,
    ) {
        let mut states = self.states();
        let Some(moved) = states.remove(from) else {
            return (None, None);
        };
        let replaced = states.insert(to.to_string(), moved.clone());
        (Some(moved), replaced)
    }
}

/// Upload every spool file left in `dir` to its object key and remove it.
///
/// This runs after the FUSE file system is gone (unpublish), so content that
/// could not be uploaded before, also from before a restart, is not lost.
/// Data files without a JSON file belong to deleted names and are removed.
pub async fn upload_pending(op: &Operator, dir: &Path) -> Result<usize, Status> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(crate::util::error::io_status(err, "list spool folder")),
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    let mut uploaded = 0;
    for data_path in paths
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "data"))
    {
        let meta_path = data_path.with_extension("json");
        let meta: SpoolMeta = match std::fs::read(&meta_path) {
            Ok(data) => serde_json::from_slice(&data)
                .map_err(|e| Status::internal(format!("bad spool metadata {meta_path:?}: {e}")))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let _ = std::fs::remove_file(data_path);
                continue;
            }
            Err(err) => return Err(crate::util::error::io_status(err, "read spool metadata")),
        };
        upload_data_file(op, data_path, &meta.key).await?;
        let _ = std::fs::remove_file(&meta_path);
        let _ = std::fs::remove_file(data_path);
        uploaded += 1;
    }
    Ok(uploaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendal::services::Memory;

    #[tokio::test]
    async fn handles_share_one_state() {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::new(dir.path().to_path_buf());
        let first = spool.open("a").unwrap();
        assert!(first.new_guard.is_some());
        drop(first.new_guard);
        let second = spool.open("a").unwrap();
        assert!(second.new_guard.is_none());
        assert!(Arc::ptr_eq(&first.state, &second.state));
    }

    #[tokio::test]
    async fn pending_content_is_uploaded_later() {
        let dir = tempfile::tempdir().unwrap();
        let spool = Spool::new(dir.path().to_path_buf());
        let opened = spool.open("out/a.txt").unwrap();
        let mut state = opened.new_guard.unwrap();
        state.write_at(b"kept", 0).unwrap();
        drop(state);
        // A deleted name leaves no upload behind.
        let gone = spool.open("gone.txt").unwrap();
        let mut gone_state = gone.new_guard.unwrap();
        gone_state.write_at(b"x", 0).unwrap();
        gone_state.mark_deleted();
        drop(gone_state);
        drop(spool);

        let op = Operator::new(Memory::default()).unwrap();
        assert_eq!(upload_pending(&op, dir.path()).await.unwrap(), 1);
        assert_eq!(op.read("out/a.txt").await.unwrap().to_vec(), b"kept");
        assert!(!op.exists("gone.txt").await.unwrap());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn fill_from_streams_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        op.write("big", vec![5u8; 3 * 1024 * 1024]).await.unwrap();
        let spool = Spool::new(dir.path().to_path_buf());
        let opened = spool.open("big").unwrap();
        let mut state = opened.new_guard.unwrap();
        state.fill_from(&op).await.unwrap();
        assert_eq!(state.len, 3 * 1024 * 1024);
        assert!(!state.dirty);
        assert_eq!(state.read_at(10, 4).unwrap(), vec![5u8; 4]);
    }
}
