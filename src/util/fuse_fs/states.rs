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
    /// Generation from a per-volume counter that only grows. Each journal write
    /// gets a new, higher number. Replay uploads only the newest journal of each
    /// key; older ones are obsolete.
    #[serde(default)]
    generation: u64,
    /// A tombstone has no spool data. It says: the name was removed or replaced
    /// in storage, so older journals of this key must never be uploaded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    tombstone: bool,
}

/// Highest journal generation found in `dir`, or 0.
fn max_generation(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| std::fs::read(p).ok())
        .filter_map(|data| serde_json::from_slice::<SpoolMeta>(&data).ok())
        .map(|m| m.generation)
        .max()
        .unwrap_or(0)
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

    /// Count space that is already used, without checking the limit (spool
    /// files found on disk at startup).
    pub fn charge_existing(&self, bytes: u64) {
        self.used.fetch_add(bytes, Ordering::Relaxed);
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

/// Stops a file system cleanly: no new operations, uploads or workers once it
/// closes, and a way to wait for operations that are still running.
///
/// Entering and closing use the same lock, so an operation either entered
/// before shutdown started (and shutdown waits for it) or is refused.
#[derive(Default)]
pub struct Fence {
    /// (closing, number of running operations)
    inner: std::sync::Mutex<(bool, usize)>,
    idle: tokio::sync::Notify,
}

/// Counts one running operation until it is dropped.
pub struct FenceGuard(Arc<Fence>);

impl Drop for FenceGuard {
    fn drop(&mut self) {
        let mut inner = self.0.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.1 = inner.1.saturating_sub(1);
        if inner.1 == 0 {
            self.0.idle.notify_waiters();
        }
    }
}

impl Fence {
    /// Count a running operation, or refuse it if the file system is closing.
    pub fn enter(self: &Arc<Self>) -> Result<FenceGuard, Status> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if inner.0 {
            return Err(Status::unavailable("the file system is shutting down"));
        }
        inner.1 += 1;
        Ok(FenceGuard(self.clone()))
    }

    pub fn is_closing(&self) -> bool {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).0
    }

    /// Refuse new operations, uploads and workers, then wait until no operation
    /// runs.
    pub async fn close_and_wait(&self) {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).0 = true;
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.inner.lock().unwrap_or_else(|p| p.into_inner()).1 == 0 {
                return;
            }
            idle.await;
        }
    }

    fn check(&self) -> Result<(), Status> {
        if self.is_closing() {
            return Err(Status::unavailable("the file system is shutting down"));
        }
        Ok(())
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
    /// Per-volume journal generation counter (the next free number).
    generations: Arc<AtomicU64>,
    /// Set when a local disk step failed in the middle of a change.
    broken: Arc<AtomicBool>,
    /// Pause and failure points for tests.
    hooks: Arc<super::pause::PausePoints>,
    /// The spool data file was removed from disk.
    data_removed: bool,
    /// Test hook: the next write writes this many bytes and then fails.
    #[cfg(test)]
    fail_after: Option<usize>,
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
        let key = self.key.clone();
        self.write_meta_for(&key)
    }

    /// Write the journal with `key` durably: the spool data is synced first, then
    /// the JSON file is written to a temp file, synced, renamed and the folder
    /// synced. After a crash the journal is either the old or the new one.
    fn write_meta_for(&mut self, key: &str) -> std::io::Result<()> {
        self.file.sync_all()?;
        let generation = self.generations.fetch_add(1, Ordering::SeqCst);
        let data = serde_json::to_vec(&SpoolMeta {
            key: key.to_string(),
            generation,
            tombstone: false,
        })
        .map_err(std::io::Error::other)?;
        let tmp = self.meta_path.with_extension("json.tmp");
        let mut file = File::create(&tmp)?;
        file.write_all(&data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &self.meta_path)?;
        // From here on the journal may have changed. If the folder sync fails,
        // nobody can tell which journal is current: fail closed.
        self.sync_dir()?;
        self.complete = true;
        Ok(())
    }

    /// Sync the spool folder. A failure marks the file system as broken.
    fn sync_dir(&self) -> std::io::Result<()> {
        let result = if self.hooks.should_fail("dir_fsync") {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        } else {
            match self.meta_path.parent() {
                Some(dir) => File::open(dir).and_then(|d| d.sync_all()),
                None => Ok(()),
            }
        };
        if let Err(err) = &result {
            mark_broken(
                &self.broken,
                &format!("syncing the spool folder failed: {err}"),
            );
        }
        result
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

    /// Write a change. The first change after the content was published gets a
    /// journal before the write reports success, so a replay keeps it.
    pub fn write_at(&mut self, data: &[u8], offset: u64) -> std::io::Result<()> {
        // A request that cannot work fails before the journal is written.
        let end = checked_end(offset, data.len())?;
        self.reserved_journal_first(self.len.max(end))?;
        let written = self.write_spool(data, offset);
        self.after_change(written)
    }

    /// Reserve space for `bytes`, then write the first journal. If the journal
    /// fails, the reservation is given back.
    fn reserved_journal_first(&mut self, bytes: u64) -> std::io::Result<()> {
        let before = self.charged;
        self.charge_to(bytes)?;
        if let Err(err) = self.journal_first() {
            let _ = self.charge_to(before);
            return Err(err);
        }
        Ok(())
    }

    /// Before the first change after the content was published (loaded from
    /// storage or uploaded), write a journal durably. The spool data is complete
    /// content at this point. If this fails, nothing has changed yet.
    fn journal_first(&mut self) -> std::io::Result<()> {
        if !self.complete {
            self.write_meta()?;
        }
        Ok(())
    }

    /// Finish a change. A failure that may have changed the spool file in part
    /// leaves a journal over unknown content: stop the file system.
    fn after_change(&mut self, result: Result<(), (std::io::Error, bool)>) -> std::io::Result<()> {
        match result {
            Ok(()) => {
                self.dirty = true;
                Ok(())
            }
            Err((err, touched)) => {
                if touched && self.complete {
                    mark_broken(
                        &self.broken,
                        &format!("changing the spool file of {} failed: {err}", self.key),
                    );
                }
                Err(err)
            }
        }
    }

    /// Write into the spool file without marking the content as changed. An
    /// error says whether the file may have changed.
    fn write_spool(&mut self, data: &[u8], offset: u64) -> Result<(), (std::io::Error, bool)> {
        let end = checked_end(offset, data.len()).map_err(|e| (e, false))?;
        let before = self.charged;
        self.charge_to(self.len.max(end)).map_err(|e| (e, false))?;
        if let Err((err, written)) = self.write_file(data, offset) {
            // The write may have extended the file in part. Cut it back to the
            // old length and charge what is really on disk.
            if written > 0 {
                let _ = self.file.set_len(self.len);
            }
            let on_disk = self.file.metadata().map(|m| m.len()).unwrap_or(before);
            let _ = self.charge_to(on_disk);
            // Only a failure after some bytes were written changed the file.
            return Err((err, written > 0));
        }
        self.len = self.len.max(end);
        Ok(())
    }

    /// Write all of `data` at `offset`. An error also says how many bytes were
    /// written before it.
    fn write_file(&mut self, data: &[u8], offset: u64) -> Result<(), (std::io::Error, usize)> {
        // Test hook: write only part of the data, then fail.
        #[cfg(test)]
        let fail_after = self.fail_after.take();
        #[cfg(not(test))]
        let fail_after: Option<usize> = None;
        let limit = fail_after.map_or(data.len(), |n| n.min(data.len()));
        let mut done = 0;
        while done < limit {
            match self.file.write_at(&data[done..limit], offset + done as u64) {
                Ok(0) => return Err((std::io::ErrorKind::WriteZero.into(), done)),
                Ok(n) => done += n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => return Err((err, done)),
            }
        }
        if fail_after.is_some() {
            return Err((std::io::Error::from_raw_os_error(libc::EIO), done));
        }
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
        checked_end(len, 0)?;
        self.reserved_journal_first(len)?;
        let resized = self.resize(len);
        self.after_change(resized)
    }

    /// Change the length of the spool file without marking the content as
    /// changed. An error says whether the file may have changed.
    fn resize(&mut self, len: u64) -> Result<(), (std::io::Error, bool)> {
        checked_end(len, 0).map_err(|e| (e, false))?;
        let before = self.charged;
        self.charge_to(len).map_err(|e| (e, false))?;
        if let Err(err) = self.file.set_len(len) {
            let _ = self.charge_to(before);
            return Err((err, true));
        }
        self.len = len;
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
            self.write_spool(&chunk, offset)
                .map_err(|(e, _)| io_status(e, "write spool file"))?;
        }
        self.dirty = false;
        Ok(())
    }

    /// Upload the content to its key.
    ///
    /// The spool data is synced before every upload, and a journal exists while
    /// the upload runs, so a failed upload can be replayed. After a successful
    /// upload the journal is removed durably: published content is never
    /// replayed again (a replay could overwrite newer content in storage).
    async fn upload(&mut self, op: &Operator) -> Result<(), Status> {
        if !self.dirty {
            return Ok(());
        }
        self.file
            .sync_all()
            .map_err(|e| io_status(e, "sync spool file"))?;
        if !self.complete {
            self.write_meta()
                .map_err(|e| io_status(e, "write spool metadata"))?;
        }
        upload_data_file(op, &self.data_path, &self.key).await?;
        self.dirty = false;
        let removed = match std::fs::remove_file(&self.meta_path) {
            Ok(()) => self.sync_dir(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => {
                mark_broken(
                    &self.broken,
                    &format!("removing a used journal failed: {err}"),
                );
                Err(err)
            }
        };
        if removed.is_err() {
            return Err(broken_status());
        }
        self.complete = false;
        Ok(())
    }

    /// Remove the spool files and give back their space. The open file stays
    /// readable for handles that still hold the state.
    fn remove_files(&mut self) {
        for path in [&self.data_path, &self.meta_path] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    tracing::warn!("cannot remove {path:?}: {err}");
                    continue;
                }
            }
            if path == &self.data_path {
                self.data_removed = true;
            }
        }
        self.complete = false;
    }
}

impl Drop for StateData {
    /// The spool file is really gone only when the last holder (also a reader)
    /// drops the state, because an unlinked file keeps its disk space while it
    /// is open. Only then the space goes back to the budget.
    fn drop(&mut self) {
        // A spool file that is still on disk waits for a replay, which gives its
        // space back when it removes the file. Giving it back here too would
        // count it twice.
        if self.data_removed {
            self.budget.release(self.charged);
        }
    }
}

/// Mark a state as unlinked and remove its journal durably, so neither the state
/// nor a replay after a crash uploads it again.
///
/// The state only changes after the journal is durably gone. If that fails, the
/// state stays as it was (alive and mapped), so nobody waits on a dead state.
fn retire(state: &FileState, data: &mut StateData) -> std::io::Result<()> {
    match std::fs::remove_file(&data.meta_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    data.sync_dir()?;
    data.complete = false;
    data.dirty = false;
    state.unlinked.store(true, Ordering::SeqCst);
    Ok(())
}

/// Mark a file system as broken after a local disk step failed in the middle of
/// a change. From then on it refuses changes and uploads; the spool folder is
/// left as it is for the replay at unpublish or remount.
fn mark_broken(broken: &AtomicBool, why: &str) {
    if !broken.swap(true, Ordering::SeqCst) {
        tracing::error!(
            "FUSE file system stops accepting changes: {why}; its spool is replayed at unpublish"
        );
    }
}

/// The end of `len` bytes at `offset`, or EFBIG if no file can be that large.
fn checked_end(offset: u64, len: usize) -> std::io::Result<u64> {
    offset
        .checked_add(len as u64)
        .filter(|end| *end <= i64::MAX as u64)
        .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EFBIG))
}

/// A refused [`States::may_persist`] as an I/O error.
fn gate_error(status: Status) -> std::io::Error {
    tracing::warn!("change refused: {}", status.message());
    std::io::Error::from_raw_os_error(libc::EIO)
}

fn broken_status() -> Status {
    Status::internal("the FUSE file system stopped after a local disk error")
}

/// Remove a spool data file and give its space back.
fn remove_and_refund(path: &Path, budget: &SpoolBudget) -> std::io::Result<()> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    match std::fs::remove_file(path) {
        Ok(()) => {
            budget.release(size);
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Remove a file; a missing file is fine.
fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Sync a folder, so removed or renamed files stay that way after a crash.
fn sync_folder(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

async fn upload_data_file(op: &Operator, data_path: &Path, key: &str) -> Result<(), Status> {
    let file = tokio::fs::File::open(data_path)
        .await
        .map_err(|e| io_status(e, "open spool file"))?;
    crate::node::upload::upload_file(op, file, key, key).await?;
    Ok(())
}

/// Proof that the caller holds the namespace lock of a file system.
pub struct NsGuard<'a>(#[allow(dead_code)] tokio::sync::MutexGuard<'a, ()>);

/// All file states of one FUSE file system.
///
/// Every operation that changes which state or object a name points to (open
/// or create that adds a state, the close of the last release, rename, unlink,
/// retire) holds the namespace lock for its whole run, also across storage
/// calls. The lock order is: namespace lock first, then state locks by id.
/// Reads, writes, flush and fsync of an open state do not take it.
pub struct States {
    ns: Mutex<()>,
    /// Pause points for tests that force an order of operations.
    pub pause: Arc<super::pause::PausePoints>,
    /// Shutdown fence of the file system.
    pub fence: Arc<Fence>,
    dir: PathBuf,
    budget: Arc<SpoolBudget>,
    next_id: AtomicU64,
    /// Next journal generation. It starts above every journal already on disk.
    generations: Arc<AtomicU64>,
    /// Set when a local disk step failed in the middle of a change.
    broken: Arc<AtomicBool>,
    map: std::sync::Mutex<HashMap<String, Arc<FileState>>>,
}

impl States {
    pub fn new(dir: PathBuf, budget: Arc<SpoolBudget>) -> Self {
        let generations = Arc::new(AtomicU64::new(max_generation(&dir) + 1));
        Self {
            ns: Mutex::new(()),
            pause: Arc::default(),
            fence: Arc::default(),
            dir,
            budget,
            next_id: AtomicU64::new(1),
            generations,
            broken: Arc::default(),
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

    /// Durably write a tombstone for `key` with a new generation, before a
    /// storage step that removes or replaces the object of `key`. Returns its
    /// path. A failure before the tombstone is in place changes nothing; a
    /// failed folder sync afterwards stops the file system.
    pub fn write_tombstone(&self, key: &str) -> Result<PathBuf, Status> {
        std::fs::create_dir_all(&self.dir).map_err(|e| io_status(e, "create spool folder"))?;
        let name = format!("tomb-{}", uuid::Uuid::new_v4().simple());
        let path = self.dir.join(format!("{name}.json"));
        let tmp = self.dir.join(format!("{name}.json.tmp"));
        let data = serde_json::to_vec(&SpoolMeta {
            key: key.to_string(),
            generation: self.generations.fetch_add(1, Ordering::SeqCst),
            tombstone: true,
        })
        .map_err(|e| Status::internal(format!("encode tombstone: {e}")))?;
        let written = (|| {
            let mut file = File::create(&tmp)?;
            file.write_all(&data)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &path)
        })();
        written.map_err(|e| io_status(e, "write tombstone"))?;
        let synced = if self.pause.should_fail("tombstone_sync") {
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        } else {
            sync_folder(&self.dir)
        };
        if let Err(err) = synced {
            mark_broken(&self.broken, &format!("syncing a tombstone failed: {err}"));
            return Err(broken_status());
        }
        Ok(path)
    }

    /// Remove a tombstone once every older journal of its key is durably gone.
    /// A tombstone left behind is harmless: newer journals win against it.
    pub fn remove_tombstone(&self, path: &Path) {
        if let Err(err) = remove_if_present(path).and_then(|()| sync_folder(&self.dir)) {
            tracing::warn!("cannot remove tombstone {path:?}: {err}");
        }
    }

    /// The one check before a state writes a journal, uploads or publishes.
    /// Fails when the file system is broken or closing. Returns false for a
    /// retired state (unlinked, closed, or no longer mapped under its key): it
    /// may still change its local spool for its open handles, but it never
    /// writes a journal or uploads again. The caller holds the state lock.
    fn may_persist(&self, state: &FileState, data: &StateData) -> Result<bool, Status> {
        self.check_broken()?;
        self.fence.check()?;
        if data.closed || state.is_unlinked() {
            return Ok(false);
        }
        Ok(self
            .get(&data.key)
            .is_some_and(|mapped| std::ptr::eq(Arc::as_ptr(&mapped), state)))
    }

    /// Write to a state through an open handle, behind [`States::may_persist`].
    pub fn write(
        &self,
        state: &FileState,
        data: &mut StateData,
        bytes: &[u8],
        offset: u64,
    ) -> std::io::Result<()> {
        if self.may_persist(state, data).map_err(gate_error)? {
            return data.write_at(bytes, offset);
        }
        data.write_spool(bytes, offset).map_err(|(err, _)| err)
    }

    /// Change the length of a state, behind [`States::may_persist`].
    pub fn set_len(
        &self,
        state: &FileState,
        data: &mut StateData,
        len: u64,
    ) -> std::io::Result<()> {
        if self.may_persist(state, data).map_err(gate_error)? {
            return data.set_len(len);
        }
        data.resize(len).map_err(|(err, _)| err)
    }

    /// True after a local disk error in the middle of a change.
    pub fn is_broken(&self) -> bool {
        self.broken.load(Ordering::SeqCst)
    }

    fn check_broken(&self) -> Result<(), Status> {
        if self.is_broken() {
            return Err(broken_status());
        }
        Ok(())
    }

    /// True if a live state has a key below `prefix` (like `dir/`).
    pub async fn has_live_under(&self, ns: &NsGuard<'_>, prefix: &str) -> bool {
        let keys: Vec<String> = self
            .map()
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        for key in keys {
            if self.live(ns, &key).await.is_some() {
                return true;
            }
        }
        false
    }

    /// Take the namespace lock.
    pub async fn lock_namespace(&self) -> NsGuard<'_> {
        NsGuard(self.ns.lock().await)
    }

    /// The live state of `key`. A dead entry (closed or unlinked) is removed
    /// from the map, so no caller can ever wait on it in a loop.
    async fn live(&self, _ns: &NsGuard<'_>, key: &str) -> Option<Arc<FileState>> {
        let state = self.get(key)?;
        let dead = {
            let data = state.lock().await;
            data.closed || state.is_unlinked()
        };
        if dead {
            self.unmap(key, &state);
            return None;
        }
        Some(state)
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
        let ns = self.lock_namespace().await;
        self.check_broken()?;
        if let Some(state) = self.live(&ns, key).await {
            let mut data = state.lock().await;
            if !keep {
                self.set_len(&state, &mut data, 0)
                    .map_err(|e| io_status(e, "truncate"))?;
            }
            data.handles += 1;
            drop(data);
            return Ok(state);
        }
        self.create(op, key, keep).await
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
                data_removed: false,
                #[cfg(test)]
                fail_after: None,
                budget: self.budget.clone(),
                generations: self.generations.clone(),
                broken: self.broken.clone(),
                hooks: self.pause.clone(),
            }),
        });
        // The caller holds the namespace lock, so nobody else adds this key now.
        let mut data = state.data.lock().await;
        self.map().insert(key.to_string(), state.clone());
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
        // Content that was just loaded from storage is published already and
        // needs no journal until it changes.
        let loaded = loaded.and_then(|()| {
            if !data.dirty {
                return Ok(());
            }
            data.write_meta()
                .map_err(|e| io_status(e, "write spool metadata"))
        });
        if let Err(err) = loaded {
            if self.is_broken() {
                // The journal may exist; leave the spool for the replay.
                return Err(broken_status());
            }
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
        if !self.may_persist(state, &data)? {
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
        if self.is_broken() {
            // Nothing is uploaded or removed from memory any more.
            return (Err(broken_status()), false);
        }
        let result = match self.may_persist(state, &data) {
            Ok(true) => {
                self.pause.hit("release_upload").await;
                data.upload(op).await
            }
            Ok(false) => Ok(()),
            Err(err) => Err(err),
        };
        if data.handles > 0 {
            return (result, false);
        }
        if result.is_err() {
            return (result, true);
        }
        drop(data);
        self.pause.hit("release_close").await;
        // Closing removes the name from the map, so it needs the namespace lock,
        // taken before the state lock.
        let _ns = self.lock_namespace().await;
        let mut data = state.lock().await;
        if data.closed || data.handles > 0 {
            return (result, false);
        }
        // Something may have stopped the file system meanwhile: then nothing is
        // uploaded or removed, everything stays for the replay.
        if self.is_broken() {
            return (Err(broken_status()), false);
        }
        if self.fence.is_closing() {
            return (result, true);
        }
        self.pause.hit("release_gate").await;
        let persist = match self.may_persist(state, &data) {
            Ok(persist) => persist,
            // Broken or closing since the checks above: keep every recovery
            // file, change nothing.
            Err(err) => return (Err(err), !self.is_broken()),
        };
        // A rename may have changed the key meanwhile; upload under the new key.
        // Only a retired state closes without upload.
        if persist
            && data.dirty
            && let Err(err) = data.upload(op).await
        {
            return (Err(err), true);
        }
        self.close(state, &mut data);
        (result, false)
    }

    /// Retry the upload of a state without handles. Returns true when the state
    /// is done (uploaded, unlinked, closed or taken over by a new handle).
    pub async fn retry(&self, op: &Operator, state: &Arc<FileState>) -> bool {
        let mut data = state.lock().await;
        if data.closed || data.handles > 0 {
            return true;
        }
        if self.fence.is_closing() || self.is_broken() {
            return true;
        }
        match self.may_persist(state, &data) {
            Ok(true) => {
                if data.upload(op).await.is_err() {
                    return false;
                }
            }
            Ok(false) => {}
            // Leave everything for the replay.
            Err(_) => return true,
        }
        drop(data);
        self.pause.hit("retry_close").await;
        let _ns = self.lock_namespace().await;
        let mut data = state.lock().await;
        if data.closed || data.handles > 0 {
            return true;
        }
        if self.is_broken() || self.fence.is_closing() {
            // Leave everything for the replay.
            return true;
        }
        self.pause.hit("retry_gate").await;
        let persist = match self.may_persist(state, &data) {
            Ok(persist) => persist,
            // Broken or closing since the checks above: leave everything for
            // the replay.
            Err(_) => return true,
        };
        if data.dirty && persist {
            // A rename made it dirty again meanwhile: another upload is needed.
            return false;
        }
        self.close(state, &mut data);
        true
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
        self.set_len(&state, &mut data, size)?;
        Ok(true)
    }

    /// Remove the name `key` in storage; only if that succeeds, a live state
    /// loses its name. Open handles keep their content, but it is never uploaded
    /// again. Returns true if a live state was retired.
    ///
    /// The namespace lock is held also for a file without a live state, so no
    /// writable open can load the old content before the delete.
    pub async fn unlink(&self, op: &Operator, key: &str) -> Result<bool, Status> {
        let ns = self.lock_namespace().await;
        self.check_broken()?;
        let Some(state) = self.live(&ns, key).await else {
            self.pause.hit("unlink_closed").await;
            self.delete_object(op, key).await?;
            return Ok(false);
        };
        let mut data = state.lock().await;
        let key = data.key.clone();
        // Write ahead: after a crash from here on, replay drops the journal of
        // this name instead of bringing the file back.
        let tomb = self.write_tombstone(&key)?;
        self.pause.hit("unlink_tombstoned").await;
        if let Err(err) = self.delete_object(op, &key).await {
            // The delete may have applied anyway. Only a name that is still
            // there is a clean failure.
            if !self.object_gone(op, &key).await? {
                self.undo_tombstone(&mut data, &tomb)?;
                return Err(err);
            }
        }
        self.pause.hit("unlink_deleted").await;
        // The object is gone in storage. If the journal cannot be removed, the
        // name could come back from memory: fail closed. The tombstone stays.
        if let Err(err) = retire(&state, &mut data) {
            mark_broken(
                &self.broken,
                &format!("retiring {key} during unlink failed: {err}"),
            );
            return Err(broken_status());
        }
        self.unmap(&key, &state);
        self.pause.hit("unlink_retired").await;
        self.remove_tombstone(&tomb);
        Ok(true)
    }

    /// Delete the object of `key`; a missing object counts as deleted. Test
    /// failure points: "delete_fails" fails without deleting,
    /// "delete_fails_after_apply" deletes and then reports a failure.
    async fn delete_object(&self, op: &Operator, key: &str) -> Result<(), Status> {
        if self.pause.should_fail("delete_fails") {
            return Err(Status::unavailable("injected delete failure"));
        }
        match op.delete(key).await {
            Ok(()) => {}
            Err(err) if err.kind() == opendal::ErrorKind::NotFound => {}
            Err(err) => return Err(opendal_status(err, "delete")),
        }
        if self.pause.should_fail("delete_fails_after_apply") {
            return Err(Status::unavailable("injected delete failure after apply"));
        }
        Ok(())
    }

    /// After a storage step that removes `key` reported an error, check whether
    /// the object is gone anyway. If the check fails too, nobody can tell what
    /// happened: the file system stops.
    async fn object_gone(&self, op: &Operator, key: &str) -> Result<bool, Status> {
        match op.stat(key).await {
            Ok(_) => Ok(false),
            Err(err) if err.kind() == opendal::ErrorKind::NotFound => Ok(true),
            Err(err) => {
                mark_broken(
                    &self.broken,
                    &format!("cannot check {key} after a failed storage step: {err}"),
                );
                Err(broken_status())
            }
        }
    }

    /// Take back a tombstone after its storage step did not happen. The journal
    /// of the state (if any) first gets a newer generation, so it wins against
    /// the tombstone even if the tombstone cannot be removed.
    fn undo_tombstone(&self, data: &mut StateData, tomb: &Path) -> Result<(), Status> {
        if data.complete
            && let Err(err) = data.write_meta()
        {
            mark_broken(
                &self.broken,
                &format!("renewing the journal of {} failed: {err}", data.key),
            );
            return Err(broken_status());
        }
        self.remove_tombstone(tomb);
        Ok(())
    }

    /// Rename the live state of `from` to `to`. Returns false if there is none.
    ///
    /// The object under the old name is removed from storage right away, while
    /// the state is locked. Removing it later could remove a new file that was
    /// created under the old name in the meantime. The content stays in the spool
    /// and is uploaded under the new name. A live state at `to` is retired: its
    /// handles keep working, but it never uploads under that name again.
    pub async fn rename(
        &self,
        ns: &NsGuard<'_>,
        op: &Operator,
        from: &str,
        to: &str,
    ) -> Result<bool, Status> {
        let Some(src) = self.live(ns, from).await else {
            return Ok(false);
        };
        let dst = self.live(ns, to).await.filter(|d| !Arc::ptr_eq(d, &src));
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
        self.pause.hit("rename_locked").await;
        // Write-ahead: first the journal points to the new key. If that fails,
        // nothing changed at all.
        self.check_broken()?;
        if let Err(err) = src_data.write_meta_for(to) {
            if self.is_broken() {
                return Err(broken_status());
            }
            return Err(io_status(err, "write spool metadata"));
        }
        // The journal already names the new key. Any failure from here on has no
        // clean way back: fail closed, the replay follows the newest journal.
        match op.delete(from).await {
            Ok(()) => {}
            Err(err) if err.kind() == opendal::ErrorKind::NotFound => {}
            Err(err) => {
                mark_broken(
                    &self.broken,
                    &format!("removing {from} during a rename failed: {err}"),
                );
                return Err(broken_status());
            }
        }
        if let (Some(dst), Some(mut dst_data)) = (&dst, dst_data)
            && let Err(err) = retire(dst, &mut dst_data)
        {
            mark_broken(
                &self.broken,
                &format!("retiring {to} during a rename failed: {err}"),
            );
            return Err(broken_status());
        }
        // Only now the state changes in memory.
        {
            let mut map = self.map();
            map.remove(from);
            map.insert(to.to_string(), src.clone());
        }
        src_data.key = to.to_string();
        src_data.dirty = true;
        Ok(true)
    }

    /// Rename the closed file `from` onto `to`. `place` makes the storage
    /// rename or copy and returns true for a copy, whose source is then deleted
    /// here.
    ///
    /// A live state at `to` stays locked, so it cannot upload in between. Its
    /// journal is covered by a tombstone first, so a replay after a crash never
    /// uploads the old destination over the renamed content. Once the new
    /// content is in place, every later failure stops the file system.
    pub async fn rename_closed<F, Fut, E>(
        &self,
        ns: &NsGuard<'_>,
        op: &Operator,
        from: &str,
        to: &str,
        place: F,
        on_status: impl Fn(Status) -> E,
    ) -> Result<(), E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<bool, E>>,
    {
        self.check_broken().map_err(&on_status)?;
        let Some(dst) = self.live(ns, to).await else {
            if place().await? {
                self.delete_object(op, from).await.map_err(&on_status)?;
            }
            return Ok(());
        };
        let mut data = dst.lock().await;
        // A missing source is a clean failure, before anything changes.
        op.stat(from)
            .await
            .map_err(|e| on_status(opendal_status(e, "stat")))?;
        let tomb = self.write_tombstone(to).map_err(&on_status)?;
        self.pause.hit("rename_closed_tombstoned").await;
        let copied = match place().await {
            Ok(copied) => copied,
            // A storage rename that applied despite the error left no source.
            Err(err) => match self.object_gone(op, from).await {
                Ok(true) => false,
                // A copy may have applied with a lost reply, also while the
                // source is still there. The destination is unknown: keep the
                // tombstone, so the old destination never uploads, and stop.
                Ok(false) if !op.info().capability().rename => {
                    mark_broken(
                        &self.broken,
                        &format!("copying {from} to {to} failed with an unclear result"),
                    );
                    return Err(on_status(broken_status()));
                }
                Ok(false) => {
                    self.undo_tombstone(&mut data, &tomb).map_err(&on_status)?;
                    return Err(err);
                }
                Err(status) => return Err(on_status(status)),
            },
        };
        if copied {
            self.pause.hit("rename_closed_copied").await;
            if self.delete_object(op, from).await.is_err()
                && !self.object_gone(op, from).await.map_err(&on_status)?
            {
                mark_broken(
                    &self.broken,
                    &format!("deleting {from} after copying it to {to} failed"),
                );
                return Err(on_status(broken_status()));
            }
        }
        self.pause.hit("rename_closed_moved").await;
        // The renamed object is in storage now. If the old file cannot be
        // retired, it must never upload again: fail closed. The tombstone stays.
        if let Err(err) = retire(&dst, &mut data) {
            mark_broken(
                &self.broken,
                &format!("retiring {to} after a rename failed: {err}"),
            );
            return Err(on_status(broken_status()));
        }
        self.unmap(to, &dst);
        self.pause.hit("rename_closed_retired").await;
        self.remove_tombstone(&tomb);
        Ok(())
    }
}

/// Upload every complete spool file left in `dir` to its object key and remove it.
///
/// This runs when no FUSE file system uses `dir` (before a mount and on
/// unpublish), so content that could not be uploaded before, also from before a
/// restart, is not lost. Data files without a JSON file are unfinished loads or
/// belong to removed names; they are removed.
///
/// Each removed spool file gives its space back to `budget`.
pub async fn upload_pending(
    op: &Operator,
    dir: &Path,
    budget: &SpoolBudget,
) -> Result<usize, Status> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(io_status(err, "list spool folder")),
    };
    let paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    let cleanup = |e: std::io::Error| io_status(e, "clean up spool folder");
    // Spool data without a journal is unfinished or removed: drop it.
    for data_path in paths
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "data"))
    {
        if !data_path.with_extension("json").exists() {
            remove_and_refund(data_path, budget).map_err(cleanup)?;
        }
    }
    // Group all records (journals and tombstones) by key. Only the newest
    // generation of a key counts; the order never depends on file names.
    let mut newest: HashMap<String, (u64, PathBuf, bool)> = HashMap::new();
    let mut obsolete: Vec<PathBuf> = Vec::new();
    for meta_path in paths
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
    {
        let meta: SpoolMeta = match std::fs::read(meta_path) {
            Ok(data) => serde_json::from_slice(&data)
                .map_err(|e| Status::internal(format!("bad spool metadata {meta_path:?}: {e}")))?,
            Err(err) => return Err(io_status(err, "read spool metadata")),
        };
        let record = (meta.generation, meta_path.clone(), meta.tombstone);
        match newest.get(&meta.key) {
            Some((generation, _, _)) if *generation > meta.generation => {
                obsolete.push(meta_path.clone());
            }
            _ => {
                if let Some((_, older, _)) = newest.insert(meta.key.clone(), record) {
                    obsolete.push(older);
                }
            }
        }
    }
    let mut tombstones = Vec::new();
    let newest: Vec<(String, PathBuf)> = newest
        .into_iter()
        .filter_map(|(key, (_, meta_path, tombstone))| {
            if tombstone {
                // Older journals of this key are dropped; the tombstone goes last.
                tombstones.push(meta_path);
                None
            } else {
                Some((key, meta_path.with_extension("data")))
            }
        })
        .collect();
    let obsolete: Vec<PathBuf> = obsolete
        .into_iter()
        .map(|meta_path| meta_path.with_extension("data"))
        .collect();
    // Every cleanup step is checked: a journal left behind could later restore
    // old bytes over newer content. Journals go first, then the folder is synced.
    let cleanup = |e: std::io::Error| io_status(e, "clean up spool folder");
    for data_path in &obsolete {
        remove_if_present(&data_path.with_extension("json")).map_err(cleanup)?;
    }
    sync_folder(dir).map_err(cleanup)?;
    for data_path in &obsolete {
        remove_and_refund(data_path, budget).map_err(cleanup)?;
    }
    let mut uploaded = 0;
    for (key, data_path) in newest {
        upload_data_file(op, &data_path, &key).await?;
        remove_if_present(&data_path.with_extension("json")).map_err(cleanup)?;
        sync_folder(dir).map_err(cleanup)?;
        remove_and_refund(&data_path, budget).map_err(cleanup)?;
        uploaded += 1;
    }
    // A tombstone goes only after every older journal of its key is durably gone.
    sync_folder(dir).map_err(cleanup)?;
    for tombstone in &tombstones {
        remove_if_present(tombstone).map_err(cleanup)?;
    }
    for tmp in paths
        .iter()
        .filter(|p| p.to_string_lossy().ends_with(".json.tmp"))
    {
        remove_if_present(tmp).map_err(cleanup)?;
    }
    sync_folder(dir).map_err(cleanup)?;
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
        assert_eq!(
            upload_pending(&op, dir.path(), &SpoolBudget::new(u64::MAX))
                .await
                .unwrap(),
            0
        );
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
        // Loaded content is published already: no journal to replay.
        assert!(!data.complete);
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
        let ns = states.lock_namespace().await;
        assert!(states.rename(&ns, &op, "a.tmp", "a").await.unwrap());
        drop(ns);
        drop(states);
        // The driver stopped before the upload. Replay uploads and cleans up.
        assert_eq!(
            upload_pending(&op, dir.path(), &SpoolBudget::new(u64::MAX))
                .await
                .unwrap(),
            1
        );
        assert_eq!(op.read("a").await.unwrap().to_vec(), b"new");
        assert!(!op.exists("a.tmp").await.unwrap());
    }

    /// Make the journal of `key` impossible to remove (a folder in its place).
    fn block_journal(dir: &Path, key: &str) {
        let meta = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.extension().is_some_and(|x| x == "json")
                    && std::fs::read_to_string(p)
                        .unwrap()
                        .contains(&format!("\"{key}\""))
            })
            .unwrap();
        std::fs::remove_file(&meta).unwrap();
        std::fs::create_dir(&meta).unwrap();
    }

    #[tokio::test]
    async fn failed_retirement_in_a_live_rename_keeps_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let states = states(dir.path());
        let a = states.open_write(&op, "a", false).await.unwrap();
        a.lock().await.write_at(b"SOURCE", 0).unwrap();
        states.upload(&op, &a).await.unwrap();
        let b = states.open_write(&op, "b", false).await.unwrap();
        b.lock().await.write_at(b"OLDDEST", 0).unwrap();
        block_journal(dir.path(), "b");
        let ns = states.lock_namespace().await;
        assert!(states.rename(&ns, &op, "a", "b").await.is_err());
        drop(ns);
        assert!(states.is_broken());
        // The release neither uploads nor removes the only copy of the source.
        assert!(states.release(&op, &a).await.0.is_err());
        assert!(!a.lock().await.closed);
        drop((a, b));
        // Remove the blocked journal folder, then replay: the source wins.
        for e in std::fs::read_dir(dir.path()).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                std::fs::remove_dir(&p).unwrap();
            }
        }
        upload_pending(&op, dir.path(), &SpoolBudget::new(u64::MAX))
            .await
            .unwrap();
        assert_eq!(op.read("b").await.unwrap().to_vec(), b"SOURCE");
    }

    /// Make `states` broken through a rename whose folder sync fails.
    async fn break_states(states: &States, op: &Operator) {
        let other = states.open_write(op, "other", false).await.unwrap();
        other.lock().await.write_at(b"x", 0).unwrap();
        states.pause.fail_once("dir_fsync");
        let ns = states.lock_namespace().await;
        assert!(states.rename(&ns, op, "other", "moved").await.is_err());
        assert!(states.is_broken());
    }

    fn spool_files(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    #[tokio::test]
    async fn release_keeps_recovery_files_when_broken_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let states = Arc::new(states(dir.path()));
        let a = states.open_write(&op, "a", false).await.unwrap();
        a.lock().await.write_at(b"A", 0).unwrap();
        let pause = states.pause.arm("release_close");
        let release = {
            let (states, op, a) = (states.clone(), op.clone(), a.clone());
            tokio::spawn(async move { states.release(&op, &a).await })
        };
        pause.reached().await;
        break_states(&states, &op).await;
        let before = spool_files(dir.path());
        pause.resume();
        assert!(release.await.unwrap().0.is_err());
        assert!(!a.lock().await.closed);
        assert_eq!(spool_files(dir.path()), before);
    }

    #[tokio::test]
    async fn retry_keeps_recovery_files_when_broken_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let states = Arc::new(states(dir.path()));
        let a = states.open_write(&op, "a", false).await.unwrap();
        {
            let mut data = a.lock().await;
            data.write_at(b"A", 0).unwrap();
            data.handles = 0;
        }
        let pause = states.pause.arm("retry_close");
        let retry = {
            let (states, op, a) = (states.clone(), op.clone(), a.clone());
            tokio::spawn(async move { states.retry(&op, &a).await })
        };
        pause.reached().await;
        break_states(&states, &op).await;
        let before = spool_files(dir.path());
        pause.resume();
        retry.await.unwrap();
        assert!(!a.lock().await.closed);
        assert_eq!(spool_files(dir.path()), before);
    }

    /// Run `release` (or `retry`) of state a: a rename to b in its unlocked
    /// phase, then shutdown starts right before its last gate. Replay must
    /// still restore b.
    async fn close_during_gate_keeps_the_spool(retry: bool) {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let states = Arc::new(states(dir.path()));
        let a = states.open_write(&op, "a", false).await.unwrap();
        a.lock().await.write_at(b"A", 0).unwrap();
        if retry {
            a.lock().await.handles = 0;
        }
        let (unlocked, gate) = if retry {
            ("retry_close", "retry_gate")
        } else {
            ("release_close", "release_gate")
        };
        let unlocked = states.pause.arm(unlocked);
        let gate = states.pause.arm(gate);
        let task = {
            let (states, op, a) = (states.clone(), op.clone(), a.clone());
            tokio::spawn(async move {
                if retry {
                    states.retry(&op, &a).await;
                } else {
                    let _ = states.release(&op, &a).await;
                }
            })
        };
        unlocked.reached().await;
        {
            let ns = states.lock_namespace().await;
            assert!(states.rename(&ns, &op, "a", "b").await.unwrap());
        }
        unlocked.resume();
        gate.reached().await;
        // Shutdown starts; nothing is in flight, so this returns at once.
        states.fence.close_and_wait().await;
        gate.resume();
        task.await.unwrap();
        assert!(!a.lock().await.closed);
        upload_pending(&op, dir.path(), &SpoolBudget::new(u64::MAX))
            .await
            .unwrap();
        assert_eq!(op.read("b").await.unwrap().to_vec(), b"A");
    }

    #[tokio::test]
    async fn forced_shutdown_before_the_release_gate_keeps_the_spool() {
        close_during_gate_keeps_the_spool(false).await;
    }

    #[tokio::test]
    async fn forced_shutdown_before_the_retry_gate_keeps_the_spool() {
        close_during_gate_keeps_the_spool(true).await;
    }

    #[tokio::test]
    async fn retry_is_needed_again_after_a_rename_during_it() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let states = Arc::new(states(dir.path()));
        let a = states.open_write(&op, "a", false).await.unwrap();
        {
            let mut data = a.lock().await;
            data.write_at(b"SOURCE", 0).unwrap();
            // Like a closed file whose upload failed and waits for a retry.
            data.handles = 0;
        }
        let pause = states.pause.arm("retry_close");
        let retry = {
            let (states, op, a) = (states.clone(), op.clone(), a.clone());
            tokio::spawn(async move { states.retry(&op, &a).await })
        };
        pause.reached().await;
        let ns = states.lock_namespace().await;
        assert!(states.rename(&ns, &op, "a", "b").await.unwrap());
        drop(ns);
        pause.resume();
        assert!(!retry.await.unwrap(), "the rename needs another upload");
        assert!(states.retry(&op, &a).await);
        assert_eq!(op.read("b").await.unwrap().to_vec(), b"SOURCE");
    }

    #[tokio::test]
    async fn uploaded_content_has_no_replayable_journal() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let states = states(dir.path());
        let a = states.open_write(&op, "a", false).await.unwrap();
        a.lock().await.write_at(b"v1", 0).unwrap();
        states.upload(&op, &a).await.unwrap();
        let journals = || {
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|x| x == "json")
                })
                .count()
        };
        assert_eq!(journals(), 0);
        // Someone else writes a newer version; a replay must not overwrite it.
        op.write("a", "newer elsewhere").await.unwrap();
        drop(a);
        drop(states);
        upload_pending(&op, dir.path(), &SpoolBudget::new(u64::MAX))
            .await
            .unwrap();
        assert_eq!(op.read("a").await.unwrap().to_vec(), b"newer elsewhere");
    }

    #[tokio::test]
    async fn failed_unlink_keeps_the_pending_content() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let states = states(dir.path());
        let joined = states.open_write(&op, "f", false).await.unwrap();
        joined.lock().await.write_at(b"keep me", 0).unwrap();
        op.write("f", "old").await.unwrap();
        states.pause.fail_once("delete_fails");
        let err = states.unlink(&op, "f").await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);
        let (result, pending) = states.release(&op, &joined).await;
        assert!(result.is_ok() && !pending);
        assert_eq!(op.read("f").await.unwrap().to_vec(), b"keep me");
        assert_eq!(spool_files(dir.path()), 0);
    }

    #[tokio::test]
    async fn an_unlink_whose_delete_applied_despite_an_error_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        op.write("f", "old").await.unwrap();
        let states = states(dir.path());
        let joined = states.open_write(&op, "f", false).await.unwrap();
        joined.lock().await.write_at(b"pending", 0).unwrap();
        states.pause.fail_once("delete_fails_after_apply");
        assert!(states.unlink(&op, "f").await.unwrap());
        let (result, pending) = states.release(&op, &joined).await;
        assert!(result.is_ok() && !pending);
        assert_eq!(
            op.stat("f").await.unwrap_err().kind(),
            opendal::ErrorKind::NotFound
        );
        assert_eq!(spool_files(dir.path()), 0);
    }

    fn fs_op(dir: &Path) -> Operator {
        let root = dir.join("root");
        std::fs::create_dir_all(&root).unwrap();
        Operator::new(opendal::services::Fs::default().root(&root.to_string_lossy())).unwrap()
    }

    /// The child side of the crash tests. It does nothing unless a parent test
    /// started it with the crash variables set.
    #[tokio::test]
    async fn crash_child() {
        let (Ok(scenario), Ok(point), Ok(dir)) = (
            std::env::var("KUBEDAL_CRASH_SCENARIO"),
            std::env::var("KUBEDAL_CRASH_AT"),
            std::env::var("KUBEDAL_CRASH_DIR"),
        ) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let op = fs_op(&dir);
        let states = states(&dir.join("spool"));
        states
            .pause
            .crash_at(Box::leak(point.clone().into_boxed_str()));
        match scenario.as_str() {
            "unlink" => {
                let f = states.open_write(&op, "f", false).await.unwrap();
                f.lock().await.write_at(b"new", 0).unwrap();
                states.unlink(&op, "f").await.unwrap();
            }
            "rename" => {
                let b = states.open_write(&op, "b", false).await.unwrap();
                b.lock().await.write_at(b"pending b", 0).unwrap();
                let ns = states.lock_namespace().await;
                let copy = || async {
                    op.copy("a", "b")
                        .await
                        .map_err(|e| opendal_status(e, "copy"))?;
                    Ok(true)
                };
                states
                    .rename_closed(&ns, &op, "a", "b", copy, |s| s)
                    .await
                    .unwrap();
            }
            other => panic!("unknown crash scenario {other}"),
        }
        panic!("the child did not reach the crash point {point}");
    }

    /// Run `scenario` in a child process that exits at `point`.
    fn crash_in_child(scenario: &str, point: &str, dir: &Path) {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "util::fuse_fs::states::tests::crash_child",
                "--test-threads=1",
            ])
            .env("KUBEDAL_CRASH_SCENARIO", scenario)
            .env("KUBEDAL_CRASH_AT", point)
            .env("KUBEDAL_CRASH_DIR", dir)
            .status()
            .unwrap();
        assert_eq!(
            status.code(),
            Some(super::super::pause::CRASH_EXIT_CODE),
            "{scenario} did not crash at {point}"
        );
    }

    #[tokio::test]
    async fn a_crash_during_unlink_never_brings_back_the_pending_content() {
        for point in ["unlink_tombstoned", "unlink_deleted", "unlink_retired"] {
            let dir = tempfile::tempdir().unwrap();
            let op = fs_op(dir.path());
            op.write("f", "old").await.unwrap();
            crash_in_child("unlink", point, dir.path());
            let spool = dir.path().join("spool");
            upload_pending(&op, &spool, &SpoolBudget::new(u64::MAX))
                .await
                .unwrap();
            if point == "unlink_tombstoned" {
                // The delete did not run: the old object is still there.
                assert_eq!(op.read("f").await.unwrap().to_vec(), b"old");
            } else {
                let err = op.stat("f").await.unwrap_err();
                assert_eq!(err.kind(), opendal::ErrorKind::NotFound, "{point}");
            }
            assert_eq!(spool_files(&spool), 0, "{point}");
        }
    }

    #[tokio::test]
    async fn a_crash_during_a_rename_onto_an_open_file_keeps_the_source_content() {
        for point in [
            "rename_closed_tombstoned",
            "rename_closed_copied",
            "rename_closed_moved",
            "rename_closed_retired",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let op = fs_op(dir.path());
            op.write("a", "source").await.unwrap();
            op.write("b", "old b").await.unwrap();
            crash_in_child("rename", point, dir.path());
            let spool = dir.path().join("spool");
            upload_pending(&op, &spool, &SpoolBudget::new(u64::MAX))
                .await
                .unwrap();
            let read = |key| {
                let op = op.clone();
                async move { op.read(key).await.ok().map(|b| b.to_vec()) }
            };
            let (a, b) = (read("a").await, read("b").await);
            // The pending content of the old b never comes back, and the source
            // content is always somewhere.
            let expected: (Option<&[u8]>, &[u8]) = match point {
                "rename_closed_tombstoned" => (Some(b"source"), b"old b"),
                "rename_closed_copied" => (Some(b"source"), b"source"),
                _ => (None, b"source"),
            };
            assert_eq!(a.as_deref(), expected.0, "{point}");
            assert_eq!(b.as_deref(), Some(expected.1), "{point}");
            assert_eq!(spool_files(&spool), 0, "{point}");
        }
    }

    #[tokio::test]
    async fn a_copy_with_an_unclear_result_keeps_the_old_destination_from_uploading() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        assert!(!op.info().capability().rename);
        op.write("a", "SOURCE").await.unwrap();
        let states = states(dir.path());
        let b = states.open_write(&op, "b", false).await.unwrap();
        b.lock().await.write_at(b"OLD DEST", 0).unwrap();
        let ns = states.lock_namespace().await;
        // The copy applies, but its reply is lost.
        let copy = || async {
            op.write("b", "SOURCE").await.unwrap();
            Err::<bool, _>(Status::unavailable("reply lost"))
        };
        let result = states.rename_closed(&ns, &op, "a", "b", copy, |s| s).await;
        drop(ns);
        assert!(result.is_err());
        assert!(states.is_broken());
        assert!(states.upload(&op, &b).await.is_err());
        assert_eq!(op.read("b").await.unwrap().to_vec(), b"SOURCE");
        // The tombstone stays, so a replay drops the old destination too.
        upload_pending(&op, dir.path(), &SpoolBudget::new(u64::MAX))
            .await
            .unwrap();
        assert_eq!(op.read("b").await.unwrap().to_vec(), b"SOURCE");
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
        let ns = states.lock_namespace().await;
        assert!(!states.rename(&ns, &op, "a", "b").await.unwrap());
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
    async fn space_comes_back_only_when_the_last_reader_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let budget = Arc::new(SpoolBudget::new(u64::MAX));
        let states = States::new(dir.path().to_path_buf(), budget.clone());
        let writer = states.open_write(&op, "a", false).await.unwrap();
        writer.lock().await.write_at(b"1234", 0).unwrap();
        let reader = states.open_read("a").await.unwrap();
        let (result, _) = states.release(&op, &writer).await;
        result.unwrap();
        drop(writer);
        // The spool file is unlinked but still open for the reader.
        assert_eq!(budget.used(), 4);
        drop(reader);
        assert_eq!(budget.used(), 0);
    }

    #[tokio::test]
    async fn spool_space_is_never_given_back_twice() {
        let op = Operator::new(Memory::default()).unwrap();
        let budget = Arc::new(SpoolBudget::new(100));
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let vol1 = States::new(dir1.path().to_path_buf(), budget.clone());
        let vol2 = States::new(dir2.path().to_path_buf(), budget.clone());
        let a = vol1.open_write(&op, "a", false).await.unwrap();
        a.lock().await.write_at(&[1u8; 60], 0).unwrap();
        let b = vol2.open_write(&op, "b", false).await.unwrap();
        b.lock().await.write_at(&[2u8; 40], 0).unwrap();
        // Volume 1 goes away without uploading (unmount), then its spool replays.
        drop(a);
        drop(vol1);
        assert_eq!(budget.used(), 100);
        upload_pending(&op, dir1.path(), &budget).await.unwrap();
        assert_eq!(budget.used(), 40);
        // The limit still holds for the other volume.
        let err = b.lock().await.write_at(&[2u8; 61], 40).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOSPC));
    }

    fn journal(dir: &Path, name: &str, key: &str, generation: u64, content: &str) {
        std::fs::write(dir.join(format!("{name}.data")), content).unwrap();
        let meta = serde_json::json!({"key": key, "generation": generation});
        std::fs::write(dir.join(format!("{name}.json")), meta.to_string()).unwrap();
    }

    #[tokio::test]
    async fn a_newer_tombstone_drops_older_journals_of_its_key() {
        let dir = tempfile::tempdir().unwrap();
        journal(dir.path(), "old", "b", 3, "old destination");
        journal(dir.path(), "other", "c", 4, "other file");
        let states = states(dir.path());
        // The tombstone gets a generation above everything on disk.
        let tomb = states.write_tombstone("b").unwrap();
        assert!(tomb.exists());
        drop(states);
        let op = Operator::new(Memory::default()).unwrap();
        let budget = SpoolBudget::new(u64::MAX);
        budget.reserve(25).unwrap();
        assert_eq!(upload_pending(&op, dir.path(), &budget).await.unwrap(), 1);
        assert!(!op.exists("b").await.unwrap());
        assert_eq!(op.read("c").await.unwrap().to_vec(), b"other file");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert_eq!(budget.used(), 0);
    }

    #[tokio::test]
    async fn a_newer_journal_wins_against_an_old_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let states = states(dir.path());
        states.write_tombstone("b").unwrap();
        drop(states);
        // A file created later under the same name has a newer journal.
        let max = max_generation(dir.path());
        journal(dir.path(), "new", "b", max + 1, "new file");
        let op = Operator::new(Memory::default()).unwrap();
        upload_pending(&op, dir.path(), &SpoolBudget::new(u64::MAX))
            .await
            .unwrap();
        assert_eq!(op.read("b").await.unwrap().to_vec(), b"new file");
    }

    #[tokio::test]
    async fn replay_fails_when_it_cannot_clean_up() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        journal(dir.path(), "a", "k", 1, "data");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let result = upload_pending(&op, dir.path(), &SpoolBudget::new(u64::MAX)).await;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        // The journal is still there, so the next replay tries again.
        assert!(dir.path().join("a.json").exists());
    }

    #[tokio::test]
    async fn replay_uploads_only_the_newest_journal_of_a_key() {
        // Both file name orders: the newest generation wins either way.
        for (old_name, new_name) in [("a", "z"), ("z", "a")] {
            let dir = tempfile::tempdir().unwrap();
            journal(dir.path(), old_name, "b", 5, "old destination");
            journal(dir.path(), new_name, "b", 9, "renamed source");
            let op = Operator::new(Memory::default()).unwrap();
            let budget = SpoolBudget::new(u64::MAX);
            budget.reserve(29).unwrap();
            assert_eq!(upload_pending(&op, dir.path(), &budget).await.unwrap(), 1);
            assert_eq!(op.read("b").await.unwrap().to_vec(), b"renamed source");
            let left: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert!(left.is_empty(), "{left:?}");
            assert_eq!(budget.used(), 0);
        }
    }

    #[tokio::test]
    async fn generations_keep_growing_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        journal(dir.path(), "x", "k", 41, "data");
        let op = Operator::new(Memory::default()).unwrap();
        let states = states(dir.path());
        let joined = states.open_write(&op, "new", false).await.unwrap();
        drop(joined);
        let generations: Vec<u64> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .map(|p| serde_json::from_slice::<SpoolMeta>(&std::fs::read(p).unwrap()).unwrap())
            .map(|m| m.generation)
            .collect();
        assert!(generations.contains(&41));
        assert!(generations.iter().any(|g| *g > 41));
    }

    #[tokio::test]
    async fn replay_gives_the_space_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.data"), "1234").unwrap();
        std::fs::write(dir.path().join("x.json"), r#"{"key": "x"}"#).unwrap();
        let budget = SpoolBudget::new(4);
        budget.reserve(4).unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        upload_pending(&op, dir.path(), &budget).await.unwrap();
        assert_eq!(budget.used(), 0);
        // The next byte fits again.
        budget.reserve(1).unwrap();
    }

    #[tokio::test]
    async fn partly_failed_writes_charge_what_is_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let budget = Arc::new(SpoolBudget::new(u64::MAX));
        let states = States::new(dir.path().to_path_buf(), budget.clone());
        let joined = states.open_write(&op, "a", false).await.unwrap();
        let mut data = joined.lock().await;
        data.write_at(b"12", 0).unwrap();
        // The next write extends the file by 3 of 10 bytes, then fails.
        data.fail_after = Some(3);
        assert!(data.write_at(&[9u8; 10], 2).is_err());
        let on_disk = data.file.metadata().unwrap().len();
        assert_eq!(on_disk, 2);
        assert_eq!(budget.used(), on_disk);
        assert_eq!(data.len, 2);
        // The journal now covers content that changed in part: stop.
        assert!(states.is_broken());
    }

    #[tokio::test]
    async fn a_refused_change_writes_no_journal() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        op.write("f", "OLD").await.unwrap();
        let budget = Arc::new(SpoolBudget::new(10));
        let states = States::new(dir.path().to_path_buf(), budget.clone());
        let f = states.open_write(&op, "f", true).await.unwrap();
        let journals = || {
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|x| x == "json")
                })
                .count()
        };
        let mut data = f.lock().await;
        // Over the budget: refused before the journal.
        let err = data.write_at(&[1u8; 100], 0).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOSPC));
        let err = data.set_len(100).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOSPC));
        // Not representable: refused before the journal.
        let err = data.write_at(b"x", u64::MAX).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EFBIG));
        assert_eq!(journals(), 0);
        assert!(!data.dirty);
        // A failed journal gives the reserved space back.
        block_journal_tmp(&data.meta_path);
        assert!(data.write_at(b"0123456", 0).is_err());
        assert_eq!(budget.used(), 3);
        assert_eq!(journals(), 0);
        assert!(!states.is_broken());
    }

    /// Block the temp file of a journal with a folder of the same name.
    fn block_journal_tmp(meta: &Path) {
        std::fs::create_dir(meta.with_extension("json.tmp")).unwrap();
    }

    #[tokio::test]
    async fn failures_before_any_byte_changed_do_not_stop_the_file_system() {
        let dir = tempfile::tempdir().unwrap();
        let op = Operator::new(Memory::default()).unwrap();
        let states = states(dir.path());
        let joined = states.open_write(&op, "a", false).await.unwrap();
        let mut data = joined.lock().await;
        data.write_at(b"12", 0).unwrap();
        // Sizes no file can have fail before the spool file is touched.
        let err = data.set_len(u64::MAX).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EFBIG));
        // A write that fails before its first byte changes nothing.
        data.fail_after = Some(0);
        assert!(data.write_at(b"xyz", 0).is_err());
        assert!(!states.is_broken());
        assert_eq!(data.read_at(0, 10).unwrap(), b"12");
        // One that fails after some bytes stops the file system.
        data.fail_after = Some(1);
        assert!(data.write_at(b"xyz", 0).is_err());
        assert!(states.is_broken());
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
        drop(joined);
        assert_eq!(budget.used(), 0);
    }
}
