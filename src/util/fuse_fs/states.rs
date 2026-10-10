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
//! Rules that keep the states consistent:
//! - Every state has a fixed identity. The path map only points to it.
//! - The map entry of a state is only added, moved or removed while the state is
//!   locked (a new state is locked before it enters the map).
//! - After an operation locks a state, it checks `closed` and `unlinked` first.
//!   A closed or unlinked state is never joined, truncated, renamed or uploaded.
//! - A state is only closed while it is locked, and closing removes its map entry.
//!
//! Each state has a spool file on the node. A small JSON file next to it holds the
//! object key. It is only written once the spool file holds the complete content,
//! so [`upload_pending`] never replays an unfinished spool file.

use crate::util::error::{io_status, opendal_status};
use futures::StreamExt;
use opendal::Operator;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::{Mutex, MutexGuard};
use tonic::Status;

/// Content of the JSON file next to a spool file.
#[derive(Debug, Serialize, Deserialize)]
struct SpoolMeta {
    /// Object key, relative to the root of the FUSE file system.
    key: String,
}

/// Limits the space all spool files of a node may use.
#[derive(Debug)]
pub struct SpoolBudget {
    limit: u64,
    used: AtomicU64,
}

impl SpoolBudget {
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            used: AtomicU64::new(0),
        }
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    fn reserve(&self, bytes: u64) -> std::io::Result<()> {
        let mut current = self.used.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(bytes);
            if next > self.limit {
                return Err(std::io::Error::from_raw_os_error(libc::ENOSPC));
            }
            match self
                .used
                .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    fn release(&self, bytes: u64) {
        let mut current = self.used.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_sub(bytes);
            match self
                .used
                .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
}

/// One file that is open for writing, or whose upload is still pending.
pub struct FileState {
    /// Fixed identity, for logs and tests.
    pub id: u64,
    /// The name was removed (unlink, or replaced by a rename). Set while the state
    /// is locked, or by a rename that replaces the name. Never cleared.
    unlinked: AtomicBool,
    data: Mutex<StateData>,
}

/// The part of a state that needs its lock.
pub struct StateData {
    /// Object key the content belongs to.
    pub key: String,
    file: File,
    data_path: PathBuf,
    meta_path: PathBuf,
    /// Size of the content.
    pub len: u64,
    /// The spool file has content that is not uploaded yet.
    pub dirty: bool,
    /// Number of open handles that may write.
    pub handles: usize,
    /// The state is done. Its spool files are gone and it left the map.
    pub closed: bool,
    /// The JSON file exists, so the spool file may be replayed after a crash.
    complete: bool,
    /// Bytes charged to the budget for this spool file.
    charged: u64,
    budget: Arc<SpoolBudget>,
}

impl FileState {
    pub async fn lock(&self) -> MutexGuard<'_, StateData> {
        self.data.lock().await
    }

    pub fn is_unlinked(&self) -> bool {
        self.unlinked.load(Ordering::SeqCst)
    }

    /// Lock the state if it is still alive (not closed, not unlinked).
    pub async fn lock_alive(&self) -> Option<MutexGuard<'_, StateData>> {
        let guard = self.data.lock().await;
        (!guard.closed && !self.is_unlinked()).then_some(guard)
    }
}

impl StateData {
    fn write_meta(&mut self) -> std::io::Result<()> {
        self.file.sync_all()?;
        let data = serde_json::to_vec(&SpoolMeta {
            key: self.key.clone(),
        })
        .map_err(std::io::Error::other)?;
        let tmp = self.meta_path.with_extension("json.tmp");
        let mut file = File::create(&tmp)?;
        file.write_all(&data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &self.meta_path)?;
        self.complete = true;
        Ok(())
    }

    /// Change the space charged for this spool file to `bytes`.
    fn charge_to(&mut self, bytes: u64) -> std::io::Result<()> {
        if bytes > self.charged {
            self.budget.reserve(bytes - self.charged)?;
        } else {
            self.budget.release(self.charged - bytes);
        }
        self.charged = bytes;
        Ok(())
    }

    pub fn write_at(&mut self, data: &[u8], offset: u64) -> std::io::Result<()> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EFBIG))?;
        let before = self.charged;
        self.charge_to(self.len.max(end))?;
        if let Err(err) = self.file.write_all_at(data, offset) {
            // Give the space back; the write did not happen.
            let _ = self.charge_to(before);
            return Err(err);
        }
        self.len = self.len.max(end);
        self.dirty = true;
        Ok(())
    }

    pub fn read_at(&self, offset: u64, size: u32) -> std::io::Result<Vec<u8>> {
        if offset >= self.len {
            return Ok(Vec::new());
        }
        let size = u64::from(size).min(self.len - offset) as usize;
        let mut buf = vec![0u8; size];
        self.file.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }

    pub fn set_len(&mut self, len: u64) -> std::io::Result<()> {
        let before = self.charged;
        self.charge_to(len)?;
        if let Err(err) = self.file.set_len(len) {
            let _ = self.charge_to(before);
            return Err(err);
        }
        self.len = len;
        self.dirty = true;
        Ok(())
    }

    /// Copy the current object content into the spool file, in chunks.
    async fn fill_from(&mut self, op: &Operator) -> Result<(), Status> {
        let key = self.key.clone();
        let mut stream = op
            .reader(&key)
            .await
            .map_err(|e| opendal_status(e, &format!("open {key}")))?
            .into_bytes_stream(..)
            .await
            .map_err(|e| opendal_status(e, &format!("read {key}")))?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| io_status(e, &format!("read {key}")))?;
            let offset = self.len;
            self.write_at(&chunk, offset)
                .map_err(|e| io_status(e, "write spool file"))?;
        }
        self.dirty = false;
        Ok(())
    }

    /// Upload the content to its key.
    async fn upload(&mut self, op: &Operator) -> Result<(), Status> {
        if !self.dirty {
            return Ok(());
        }
        if !self.complete {
            self.write_meta()
                .map_err(|e| io_status(e, "write spool metadata"))?;
        }
        upload_data_file(op, &self.data_path, &self.key).await?;
        self.dirty = false;
        Ok(())
    }

    /// Remove the spool files and give back their space. The open file stays
    /// readable for handles that still hold the state.
    fn remove_files(&mut self) {
        for path in [&self.data_path, &self.meta_path] {
            if let Err(err) = std::fs::remove_file(path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!("cannot remove {path:?}: {err}");
            }
        }
        self.complete = false;
        let _ = self.charge_to(0);
    }
}

/// Mark a state as unlinked and remove its journal durably, so neither the state
/// nor a replay after a crash uploads it again.
fn retire(state: &FileState, data: &mut StateData) -> std::io::Result<()> {
    state.unlinked.store(true, Ordering::SeqCst);
    data.dirty = false;
    match std::fs::remove_file(&data.meta_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    data.complete = false;
    if let Some(dir) = data.meta_path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

async fn upload_data_file(op: &Operator, data_path: &Path, key: &str) -> Result<(), Status> {
    let file = tokio::fs::File::open(data_path)
        .await
        .map_err(|e| io_status(e, "open spool file"))?;
    crate::node::upload::upload_file(op, file, key, key).await?;
    Ok(())
}

/// All file states of one FUSE file system.
pub struct States {
    /// Pause points for tests that force an order of operations.
    pub pause: Arc<super::pause::PausePoints>,
    dir: PathBuf,
    budget: Arc<SpoolBudget>,
    next_id: AtomicU64,
    map: std::sync::Mutex<HashMap<String, Arc<FileState>>>,
}

impl States {
    pub fn new(dir: PathBuf, budget: Arc<SpoolBudget>) -> Self {
        Self {
            pause: Arc::default(),
            dir,
            budget,
            next_id: AtomicU64::new(1),
            map: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<FileState>>> {
        self.map.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The state that the map holds for `key`, if any. It may be closed by the
    /// time it is locked, so callers use [`FileState::lock_alive`].
    pub fn get(&self, key: &str) -> Option<Arc<FileState>> {
        self.map().get(key).cloned()
    }

    /// Remove the map entry of `key` if it points to `state`. The caller holds
    /// the lock of `state`.
    fn unmap(&self, key: &str, state: &Arc<FileState>) {
        let mut map = self.map();
        if map.get(key).is_some_and(|s| Arc::ptr_eq(s, state)) {
            map.remove(key);
        }
    }

    /// Join the state of `key` as a writing handle, or create it. With `keep`, a
    /// new state starts with the current object content; without it, the content
    /// is empty (O_TRUNC or a new file).
    pub async fn open_write(
        &self,
        op: &Operator,
        key: &str,
        keep: bool,
    ) -> Result<Arc<FileState>, Status> {
        loop {
            let existing = self.get(key);
            if let Some(state) = existing {
                let Some(mut data) = state.lock_alive().await else {
                    // It closed or lost its name while we waited. Its map entry is
                    // gone (or replaced), so look again.
                    continue;
                };
                if !keep {
                    data.set_len(0).map_err(|e| io_status(e, "truncate"))?;
                }
                data.handles += 1;
                drop(data);
                return Ok(state);
            }
            return self.create(op, key, keep).await;
        }
    }

    async fn create(&self, op: &Operator, key: &str, keep: bool) -> Result<Arc<FileState>, Status> {
        std::fs::create_dir_all(&self.dir).map_err(|e| io_status(e, "create spool folder"))?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let name = uuid::Uuid::new_v4().simple().to_string();
        let data_path = self.dir.join(format!("{name}.data"));
        let meta_path = self.dir.join(format!("{name}.json"));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&data_path)
            .map_err(|e| io_status(e, "create spool file"))?;
        let state = Arc::new(FileState {
            id,
            unlinked: AtomicBool::new(false),
            data: Mutex::new(StateData {
                key: key.to_string(),
                file,
                data_path,
                meta_path,
                len: 0,
                dirty: true,
                handles: 0,
                closed: false,
                complete: false,
                charged: 0,
                budget: self.budget.clone(),
            }),
        });
        let mut data = state.data.lock().await;
        let inserted = {
            let mut map = self.map();
            let free = !map.contains_key(key);
            if free {
                map.insert(key.to_string(), state.clone());
            }
            free
        };
        if !inserted {
            // Another handle created a state first; join that one instead.
            data.remove_files();
            data.closed = true;
            drop(data);
            return Box::pin(self.open_write(op, key, keep)).await;
        }
        let loaded = if keep {
            match data.fill_from(op).await {
                Ok(()) => Ok(()),
                // A missing object is a new, empty file.
                Err(err) if err.code() == tonic::Code::NotFound => {
                    data.dirty = true;
                    Ok(())
                }
                Err(err) => Err(err),
            }
        } else {
            Ok(())
        };
        // Only now the spool file holds complete content and may be replayed.
        let loaded = loaded.and_then(|()| {
            data.write_meta()
                .map_err(|e| io_status(e, "write spool metadata"))
        });
        if let Err(err) = loaded {
            data.remove_files();
            data.closed = true;
            self.unmap(key, &state);
            return Err(err);
        }
        data.handles = 1;
        drop(data);
        Ok(state)
    }

    /// Join the state of `key` as a reading handle, if a live one exists.
    pub async fn open_read(&self, key: &str) -> Option<Arc<FileState>> {
        let state = self.get(key)?;
        let alive = state.lock_alive().await.is_some();
        alive.then_some(state)
    }

    /// Size of the live state of `key`, if any.
    pub async fn size(&self, key: &str) -> Option<u64> {
        let state = self.get(key)?;
        let data = state.lock_alive().await?;
        Some(data.len)
    }

    /// Upload the content of a state now (flush and fsync).
    pub async fn upload(&self, op: &Operator, state: &FileState) -> Result<(), Status> {
        let mut data = state.lock().await;
        if data.closed || state.is_unlinked() {
            return Ok(());
        }
        data.upload(op).await
    }

    /// A writing handle closes. The last one uploads and closes the state. If the
    /// upload fails, the state stays (with its spool files) for a retry.
    /// Returns the upload result and whether the state still waits for upload.
    pub async fn release(
        &self,
        op: &Operator,
        state: &Arc<FileState>,
    ) -> (Result<(), Status>, bool) {
        let mut data = state.lock().await;
        data.handles = data.handles.saturating_sub(1);
        if data.closed {
            return (Ok(()), false);
        }
        let result = if state.is_unlinked() {
            Ok(())
        } else {
            data.upload(op).await
        };
        if data.handles > 0 {
            return (result, false);
        }
        if result.is_ok() {
            self.close(state, &mut data);
            (result, false)
        } else {
            (result, true)
        }
    }

    /// Retry the upload of a state without handles. Returns true when the state
    /// is done (uploaded, unlinked, closed or taken over by a new handle).
    pub async fn retry(&self, op: &Operator, state: &Arc<FileState>) -> bool {
        let mut data = state.lock().await;
        if data.closed || data.handles > 0 {
            return true;
        }
        if state.is_unlinked() || data.upload(op).await.is_ok() {
            self.close(state, &mut data);
            return true;
        }
        false
    }

    fn close(&self, state: &Arc<FileState>, data: &mut StateData) {
        data.remove_files();
        data.closed = true;
        let key = data.key.clone();
        self.unmap(&key, state);
    }

    /// Truncate the live state of `key`. Returns false if there is none.
    pub async fn truncate(&self, key: &str, size: u64) -> std::io::Result<bool> {
        let Some(state) = self.get(key) else {
            return Ok(false);
        };
        let Some(mut data) = state.lock_alive().await else {
            return Ok(false);
        };
        data.set_len(size)?;
        Ok(true)
    }

    /// Remove the name `key`. `delete` removes the objects with the given keys
    /// from storage; only if it succeeds, the state loses its name. Open handles
    /// keep their content, but it is never uploaded again.
    pub async fn unlink<F, Fut>(&self, key: &str, delete: F) -> Result<bool, Status>
    where
        F: FnOnce(Vec<String>) -> Fut,
        Fut: std::future::Future<Output = Result<(), Status>>,
    {
        let Some(state) = self.get(key) else {
            return Ok(false);
        };
        let Some(mut data) = state.lock_alive().await else {
            return Ok(false);
        };
        delete(vec![data.key.clone()]).await?;
        state.unlinked.store(true, Ordering::SeqCst);
        data.dirty = false;
        if let Err(err) = std::fs::remove_file(&data.meta_path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("cannot remove {:?}: {err}", data.meta_path);
        }
        data.complete = false;
        self.unmap(key, &state);
        Ok(true)
    }

    /// Rename the live state of `from` to `to`. Returns false if there is none.
    ///
    /// The object under the old name is removed from storage right away, while
    /// the state is locked. Removing it later could remove a new file that was
    /// created under the old name in the meantime. The content stays in the spool
    /// and is uploaded under the new name. A live state at `to` is retired: its
    /// handles keep working, but it never uploads under that name again.
    pub async fn rename(&self, op: &Operator, from: &str, to: &str) -> Result<bool, Status> {
        loop {
            let Some(src) = self.get(from) else {
                return Ok(false);
            };
            let dst = self.get(to).filter(|d| !Arc::ptr_eq(d, &src));
            // Two states are always locked in the order of their ids.
            let (mut src_data, dst_data) = match &dst {
                Some(dst) if dst.id < src.id => {
                    let d = dst.lock().await;
                    (src.lock().await, Some(d))
                }
                Some(dst) => {
                    let sd = src.lock().await;
                    (sd, Some(dst.lock().await))
                }
                None => (src.lock().await, None),
            };
            if src_data.closed || src.is_unlinked() {
                // It closed while we waited; look again (it may be a closed file now).
                if self.get(from).is_some_and(|s| Arc::ptr_eq(&s, &src)) {
                    return Ok(false);
                }
                continue;
            }
            let map_ok = {
                let map = self.map();
                map.get(from).is_some_and(|s| Arc::ptr_eq(s, &src))
                    && match &dst {
                        Some(d) => map.get(to).is_some_and(|s| Arc::ptr_eq(s, d)),
                        None => !map.contains_key(to),
                    }
            };
            if !map_ok {
                continue;
            }
            self.pause.hit("rename_locked").await;
            match op.delete(from).await {
                Ok(()) => {}
                Err(err) if err.kind() == opendal::ErrorKind::NotFound => {}
                Err(err) => return Err(opendal_status(err, "remove old name")),
            }
            if let (Some(dst), Some(mut dst_data)) = (&dst, dst_data) {
                retire(dst, &mut dst_data).map_err(|e| io_status(e, "retire old file"))?;
            }
            {
                let mut map = self.map();
                map.remove(from);
                map.insert(to.to_string(), src.clone());
            }
            src_data.key = to.to_string();
            src_data.dirty = true;
            src_data
                .write_meta()
                .map_err(|e| io_status(e, "write spool metadata"))?;
            return Ok(true);
        }
    }

    /// Retire the live state of `key`, because another file takes its name (a
    /// rename of a closed file onto it). Its handles keep working, but it never
    /// uploads under that name again.
    pub async fn retire(&self, key: &str) -> Result<(), Status> {
        let Some(state) = self.get(key) else {
            return Ok(());
        };
        let Some(mut data) = state.lock_alive().await else {
            return Ok(());
        };
        retire(&state, &mut data).map_err(|e| io_status(e, "retire old file"))?;
        self.unmap(key, &state);
        Ok(())
    }
}

/// Upload every complete spool file left in `dir` to its object key and remove it.
///
/// This runs when no FUSE file system uses `dir` (before a mount and on
/// unpublish), so content that could not be uploaded before, also from before a
/// restart, is not lost. Data files without a JSON file are unfinished loads or
/// belong to removed names; they are removed.
pub async fn upload_pending(op: &Operator, dir: &Path) -> Result<usize, Status> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(io_status(err, "list spool folder")),
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
            Err(err) => return Err(io_status(err, "read spool metadata")),
        };
        upload_data_file(op, data_path, &meta.key).await?;
        let _ = std::fs::remove_file(&meta_path);
        let _ = std::fs::remove_file(data_path);
        uploaded += 1;
    }
    for tmp in paths
        .iter()
        .filter(|p| p.to_string_lossy().ends_with(".json.tmp"))
    {
        let _ = std::fs::remove_file(tmp);
    }
    Ok(uploaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opendal::services::Memory;

    fn states(dir: &Path) -> States {
        States::new(dir.to_path_buf(), Arc::new(SpoolBudget::new(u64::MAX)))
    }

    #[tokio::test]
    async fn handles_share_one_state() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let states = states(dir.path());
        let a = states.open_write(&op, "a", false).await.unwrap();
        let b = states.open_write(&op, "a", true).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.lock().await.handles, 2);
    }

    #[tokio::test]
    async fn closed_states_are_never_joined() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let states = states(dir.path());
        let a = states.open_write(&op, "a", false).await.unwrap();
        let (result, pending) = states.release(&op, &a).await;
        assert!(result.is_ok() && !pending);
        assert!(a.lock().await.closed);
        let b = states.open_write(&op, "a", true).await.unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
        // A closed state cannot be truncated or renamed through its old name.
        assert!(states.get("a").is_some_and(|s| Arc::ptr_eq(&s, &b)));
    }

    #[tokio::test]
    async fn unfinished_loads_are_never_replayed() {
        let dir = tempfile::tempdir().unwrap();
        // A spool file without metadata, like after a crash during loading.
        std::fs::write(dir.path().join("x.data"), "partial").unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        op.write("original", "complete content").await.unwrap();
        assert_eq!(upload_pending(&op, dir.path()).await.unwrap(), 0);
        assert_eq!(
            op.read("original").await.unwrap().to_vec(),
            b"complete content"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn metadata_is_written_only_after_loading() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        op.write("big", vec![5u8; 3 * 1024 * 1024]).await.unwrap();
        let states = states(dir.path());
        let joined = states.open_write(&op, "big", true).await.unwrap();
        let data = joined.lock().await;
        assert_eq!(data.len, 3 * 1024 * 1024);
        assert!(!data.dirty);
        assert!(data.complete);
        assert_eq!(data.read_at(10, 4).unwrap(), vec![5u8; 4]);
    }

    #[tokio::test]
    async fn pending_content_and_renames_are_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        op.write("a.tmp", "old").await.unwrap();
        let states = states(dir.path());
        let joined = states.open_write(&op, "a.tmp", false).await.unwrap();
        joined.lock().await.write_at(b"new", 0).unwrap();
        assert!(states.rename(&op, "a.tmp", "a").await.unwrap());
        drop(states);
        // The driver stopped before the upload. Replay uploads and cleans up.
        assert_eq!(upload_pending(&op, dir.path()).await.unwrap(), 1);
        assert_eq!(op.read("a").await.unwrap().to_vec(), b"new");
        assert!(!op.exists("a.tmp").await.unwrap());
    }

    #[tokio::test]
    async fn failed_unlink_keeps_the_pending_content() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let states = states(dir.path());
        let joined = states.open_write(&op, "f", false).await.unwrap();
        joined.lock().await.write_at(b"keep me", 0).unwrap();
        let err = states
            .unlink("f", |_| async { Err(Status::unavailable("backend down")) })
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);
        let (result, pending) = states.release(&op, &joined).await;
        assert!(result.is_ok() && !pending);
        assert_eq!(op.read("f").await.unwrap().to_vec(), b"keep me");
    }

    #[tokio::test]
    async fn rename_after_close_does_not_touch_the_closed_state() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let states = states(dir.path());
        let joined = states.open_write(&op, "a", false).await.unwrap();
        joined.lock().await.write_at(b"data", 0).unwrap();
        let (result, _) = states.release(&op, &joined).await;
        assert!(result.is_ok());
        // The last release won the race: rename finds no live state, so the
        // caller renames the uploaded object instead.
        assert!(!states.rename(&op, "a", "b").await.unwrap());
        assert_eq!(op.read("a").await.unwrap().to_vec(), b"data");
    }

    #[tokio::test]
    async fn truncate_after_close_falls_back_to_storage() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let states = states(dir.path());
        let joined = states.open_write(&op, "a", false).await.unwrap();
        joined.lock().await.write_at(b"12345", 0).unwrap();
        let _ = states.release(&op, &joined).await;
        assert!(!states.truncate("a", 2).await.unwrap());
    }

    #[tokio::test]
    async fn failed_writes_give_their_space_back() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let budget = Arc::new(SpoolBudget::new(u64::MAX));
        let states = States::new(dir.path().to_path_buf(), budget.clone());
        let joined = states.open_write(&op, "a", false).await.unwrap();
        let mut data = joined.lock().await;
        data.write_at(b"12", 0).unwrap();
        // An offset past the largest file size fails in the kernel (EFBIG or EINVAL).
        assert!(data.write_at(b"x", i64::MAX as u64).is_err());
        assert!(data.set_len(i64::MAX as u64 + 1).is_err());
        assert_eq!(budget.used(), 2);
        assert_eq!(data.len, 2);
    }

    #[tokio::test]
    async fn spool_space_is_limited() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let budget = Arc::new(SpoolBudget::new(4));
        let states = States::new(dir.path().to_path_buf(), budget.clone());
        let joined = states.open_write(&op, "a", false).await.unwrap();
        let mut data = joined.lock().await;
        data.write_at(b"1234", 0).unwrap();
        let err = data.write_at(b"5", 4).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOSPC));
        assert_eq!(data.len, 4);
        data.set_len(1).unwrap();
        assert_eq!(budget.used(), 1);
        drop(data);
        let _ = states.release(&op, &joined).await;
        assert_eq!(budget.used(), 0);
    }
}
