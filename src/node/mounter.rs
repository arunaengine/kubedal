//! Mount operations. A trait keeps them replaceable, so tests can run without root.

use crate::util::error::io_status;
use crate::util::fuse_fs::{Filesystem, SpoolBudget, Workers};
use crate::util::mountinfo;
use fuse3::MountOptions;
use fuse3::raw::MountHandle;
use opendal::Operator;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Status;

// async_trait adds `#[must_use]` to methods that return a Future.
#[allow(clippy::double_must_use)]
#[tonic::async_trait]
pub trait Mounter: Send + Sync {
    /// Mount `source` at `target`: a bind mount, or a read-only overlay when
    /// `read_only` is set. `target` must exist.
    async fn bind(&self, source: &Path, target: &Path, read_only: bool) -> Result<(), Status>;

    /// Serve `op` as a FUSE file system at `target`. `target` must exist.
    /// Files open for writing are kept in `spool_dir` until they are uploaded.
    async fn mount_fuse(
        &self,
        op: Operator,
        target: &Path,
        read_only: bool,
        spool_dir: &Path,
    ) -> Result<(), Status>;

    /// Unmount `target`. Unmounting a path that is not mounted is not an error.
    async fn unmount(&self, target: &Path) -> Result<(), Status>;

    /// Is `target` a mount point right now?
    fn is_mounted(&self, target: &Path) -> bool;

    /// Does this process serve a FUSE file system at `target`?
    async fn has_fuse_session(&self, target: &Path) -> bool;
}

/// Real mounts through the Linux mount system calls.
#[derive(Default)]
pub struct SystemMounter {
    fuse_sessions: Mutex<HashMap<PathBuf, (MountHandle, Arc<Workers>)>>,
    /// Space limit for FUSE spool files on this node. None means no limit.
    spool_budget: Option<Arc<SpoolBudget>>,
}

impl SystemMounter {
    /// A mounter whose FUSE spool files may use at most `bytes` on this node.
    pub fn with_spool_limit(bytes: u64) -> Self {
        Self {
            spool_budget: Some(Arc::new(SpoolBudget::new(bytes))),
            ..Default::default()
        }
    }
}

fn c_path(path: &Path) -> Result<CString, Status> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Status::invalid_argument(format!("path {path:?} contains a NUL byte")))
}

fn sys_mount(
    source: Option<&CString>,
    target: &CString,
    flags: libc::c_ulong,
) -> std::io::Result<()> {
    let source_ptr = source.map_or(std::ptr::null(), |s| s.as_ptr());
    // SAFETY: all pointers are valid NUL terminated strings or null, as mount(2) allows.
    let rc = unsafe {
        libc::mount(
            source_ptr,
            target.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn sys_umount(target: &CString, flags: libc::c_int) -> std::io::Result<()> {
    // SAFETY: target is a valid NUL terminated string.
    let rc = unsafe { libc::umount2(target.as_ptr(), flags) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Mount `source` read-only at `target`.
///
/// A read-only bind mount is not enough: the container runtime remounts the
/// volume with its own flags and makes it writable again. A read-only overlay
/// file system is read-only for every mount of it. Overlay needs two lower
/// layers when it has no upper layer, so an empty folder is the second one.
fn mount_read_only(source: &Path, empty: &Path, target: &CString) -> Result<(), Status> {
    for path in [source, empty] {
        let text = path.to_string_lossy();
        if text.contains([':', ',', '\\']) {
            return Err(Status::internal(format!(
                "path {text} cannot be used as an overlay layer"
            )));
        }
    }
    std::fs::create_dir_all(empty).map_err(|e| io_status(e, "create empty overlay layer"))?;
    let options = CString::new(format!(
        "lowerdir={}:{}",
        source.to_string_lossy(),
        empty.to_string_lossy()
    ))
    .map_err(|_| Status::internal("overlay options contain a NUL byte"))?;
    let fs_type = c"overlay";
    // SAFETY: all pointers are valid NUL terminated strings, as mount(2) expects.
    let rc = unsafe {
        libc::mount(
            fs_type.as_ptr(),
            target.as_ptr(),
            fs_type.as_ptr(),
            libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
            options.as_ptr().cast(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io_status(
            std::io::Error::last_os_error(),
            "mount read-only overlay",
        ))
    }
}

#[tonic::async_trait]
impl Mounter for SystemMounter {
    async fn bind(&self, source: &Path, target: &Path, read_only: bool) -> Result<(), Status> {
        let source_path = source.to_path_buf();
        let source = c_path(source)?;
        let target_c = c_path(target)?;
        tokio::task::spawn_blocking(move || {
            if read_only {
                let empty = source_path.with_extension("empty");
                mount_read_only(&source_path, &empty, &target_c)
            } else {
                sys_mount(Some(&source), &target_c, libc::MS_BIND)
                    .map_err(|e| io_status(e, "bind mount"))
            }
        })
        .await
        .map_err(|e| Status::internal(format!("mount task failed: {e}")))?
    }

    async fn mount_fuse(
        &self,
        op: Operator,
        target: &Path,
        read_only: bool,
        spool_dir: &Path,
    ) -> Result<(), Status> {
        let mut options = MountOptions::default();
        options
            .fs_name("kubedal")
            .allow_other(true)
            .read_only(read_only);
        let mut fs = Filesystem::new(op, 0, 0, spool_dir.to_path_buf());
        if let Some(budget) = &self.spool_budget {
            fs = fs.with_spool_budget(budget.clone());
        }
        let workers = fs.workers();
        let handle = fuse3::path::Session::new(options)
            .mount(fs, target)
            .await
            .map_err(|e| io_status(e, "mount FUSE file system"))?;
        self.fuse_sessions
            .lock()
            .await
            .insert(target.to_path_buf(), (handle, workers));
        Ok(())
    }

    async fn unmount(&self, target: &Path) -> Result<(), Status> {
        let session = self.fuse_sessions.lock().await.remove(target);
        if let Some((handle, workers)) = session {
            let result = handle.unmount().await;
            // Stop upload retries before the caller replays the spool files, so an
            // old upload can never finish after a newer one.
            workers.shutdown().await;
            match result {
                Ok(()) => return Ok(()),
                Err(err) => tracing::warn!("FUSE unmount of {target:?} failed: {err}"),
            }
        }
        if !self.is_mounted(target) {
            return Ok(());
        }
        let target_c = c_path(target)?;
        tokio::task::spawn_blocking(move || match sys_umount(&target_c, 0) {
            Ok(()) => Ok(()),
            Err(err) if err.raw_os_error() == Some(libc::EINVAL) => Ok(()),
            // A dead FUSE mount or a busy mount: detach it from the tree.
            Err(_) => sys_umount(&target_c, libc::MNT_DETACH)
                .map_err(|e| io_status(e, "unmount target path")),
        })
        .await
        .map_err(|e| Status::internal(format!("unmount task failed: {e}")))?
    }

    fn is_mounted(&self, target: &Path) -> bool {
        mountinfo::is_mount_point(target)
    }

    async fn has_fuse_session(&self, target: &Path) -> bool {
        self.fuse_sessions.lock().await.contains_key(target)
    }
}
