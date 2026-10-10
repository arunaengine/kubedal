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
use tokio::sync::Mutex;

/// Opened file represents file that opened in memory.
///
/// # FIXME
///
/// We should remove the `pub` filed to avoid unexpected changes.
pub struct OpenedFile {
    pub path: OsString,
    pub is_read: bool,
    pub inner_writer: Option<Arc<Mutex<InnerWriter>>>,
    /// Size of the file when it was opened for reading. Reads are limited to it,
    /// because some backends fail when a range reaches past the end.
    pub size: Option<u64>,
}

/// Content of a file opened for writing.
///
/// Writes go to a local spool file. `flush`, `fsync` and `release` upload the
/// whole spool file as one object, so an upload error reaches `close` or `fsync`.
pub struct InnerWriter {
    pub spool: std::fs::File,
    /// Size of the file content.
    pub written: u64,
    /// The spool file has content that is not uploaded yet.
    pub dirty: bool,
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
