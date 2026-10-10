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
use opendal::Operator;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tonic::Status;

/// Content of the JSON file next to a spool file.
#[derive(Debug, Serialize, Deserialize)]
struct SpoolMeta {
    /// Object key, relative to the root of the FUSE file system.
    key: String,
    /// Old keys of the content (after renames). They are removed after upload.
    #[serde(default)]
    old_keys: Vec<String>,
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

    #[allow(dead_code)]
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

    #[allow(dead_code)]
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

async fn upload_data_file(op: &Operator, data_path: &Path, key: &str) -> Result<(), Status> {
    let file = tokio::fs::File::open(data_path)
        .await
        .map_err(|e| io_status(e, "open spool file"))?;
    crate::node::upload::upload_file(op, file, key, key).await?;
    Ok(())
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
        for old in &meta.old_keys {
            match op.delete(old).await {
                Ok(()) => {}
                Err(err) if err.kind() == opendal::ErrorKind::NotFound => {}
                Err(err) => return Err(opendal_status(err, "remove old name")),
            }
        }
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
}
