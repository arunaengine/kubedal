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

use std::ffi::OsString;
use std::sync::Arc;

use fuse3::Errno;

use super::states::FileState;

/// Opened file represents file that opened in memory.
///
/// # FIXME
///
/// We should remove the `pub` filed to avoid unexpected changes.
pub struct OpenedFile {
    pub path: OsString,
    pub is_read: bool,
    /// Shared write state, if the file is open for writing.
    pub write: Option<Arc<FileState>>,
    /// State a read-only handle joined when it opened, if one was live.
    pub read_state: Option<Arc<FileState>>,
    /// Version of the object when a read-only handle without a state opened it.
    pub etag: Option<String>,
    /// Modification time of the object when a read-only handle without a state
    /// opened it. With `size`, getattr answers from it.
    pub mtime: Option<std::time::SystemTime>,
    /// Opened with O_APPEND: writes go to the end of the file.
    pub append: bool,
    /// Size of the file when it was opened for reading. Reads are limited to it,
    /// because some backends fail when a range reaches past the end.
    pub size: Option<u64>,
    /// Private copy of the content for a read-only handle without a state,
    /// taken before its name was removed or replaced in storage.
    pub snapshot: std::sync::OnceLock<Arc<FileState>>,
    /// The name of a read-only handle without a state was removed (unlink, or
    /// replaced by a rename): it has no links left.
    pub name_removed: std::sync::atomic::AtomicBool,
}

/// File key is the key of opened file.
///
/// # FIXME
///
/// We should remove the `pub` filed to avoid unexpected changes.
#[derive(Debug, Clone, Copy)]
pub struct FileKey(pub usize);

impl TryFrom<u64> for FileKey {
    type Error = Errno;

    fn try_from(value: u64) -> std::result::Result<Self, Self::Error> {
        match value {
            0 => Err(Errno::from(libc::EBADF)),
            _ => Ok(FileKey(value as usize - 1)),
        }
    }
}

impl FileKey {
    pub fn to_fh(self) -> u64 {
        self.0 as u64 + 1 // ensure fh is not 0
    }
}
