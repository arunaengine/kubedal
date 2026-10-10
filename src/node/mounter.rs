//! Mount operations. A trait keeps them replaceable, so tests can run without root.

use crate::util::error::io_status;
use crate::util::fuse_fs::Filesystem;
use crate::util::mountinfo;
use fuse3::MountOptions;
use fuse3::raw::MountHandle;
use opendal::Operator;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;
use tonic::Status;

// async_trait adds `#[must_use]` to methods that return a Future.
#[allow(clippy::double_must_use)]
#[tonic::async_trait]
pub trait Mounter: Send + Sync {
    /// Bind mount `source` at `target`. `target` must exist.
    async fn bind(&self, source: &Path, target: &Path, read_only: bool) -> Result<(), Status>;

    /// Serve `op` as a FUSE file system at `target`. `target` must exist.
    async fn mount_fuse(&self, op: Operator, target: &Path, read_only: bool) -> Result<(), Status>;

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
    fuse_sessions: Mutex<HashMap<PathBuf, MountHandle>>,
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

/// Make a read-only bind mount of `source` at `target`.
///
/// MS_RDONLY is ignored by a plain bind, and a later remount does not reach the
/// copies that mount propagation already made for kubelet and the container.
/// So the read-only mount is prepared at a staging path first; the bind to the
/// target then copies the read-only flag, and so do all propagated copies.
fn bind_read_only(source: &CString, staging: &CString, target: &CString) -> Result<(), Status> {
    let staging_path = Path::new(std::ffi::OsStr::from_bytes(staging.as_bytes()));
    std::fs::create_dir_all(staging_path).map_err(|e| io_status(e, "create staging directory"))?;
    let result = (|| {
        sys_mount(Some(source), staging, libc::MS_BIND).map_err(|e| io_status(e, "bind mount"))?;
        let flags = libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY;
        sys_mount(None, staging, flags).map_err(|e| io_status(e, "remount read-only"))?;
        sys_mount(Some(staging), target, libc::MS_BIND).map_err(|e| io_status(e, "bind mount"))?;
        // Also mark the target itself, in case the kernel did not copy the flag.
        if let Err(err) = sys_mount(None, target, flags) {
            let _ = sys_umount(target, libc::MNT_DETACH);
            return Err(io_status(err, "remount read-only"));
        }
        Ok(())
    })();
    let _ = sys_umount(staging, libc::MNT_DETACH);
    let _ = std::fs::remove_dir(staging_path);
    result
}

#[tonic::async_trait]
impl Mounter for SystemMounter {
    async fn bind(&self, source: &Path, target: &Path, read_only: bool) -> Result<(), Status> {
        let staging = read_only.then(|| source.with_extension("ro"));
        let source = c_path(source)?;
        let target_c = c_path(target)?;
        let staging_c = staging.as_deref().map(c_path).transpose()?;
        tokio::task::spawn_blocking(move || match staging_c {
            None => sys_mount(Some(&source), &target_c, libc::MS_BIND)
                .map_err(|e| io_status(e, "bind mount")),
            Some(staging_c) => bind_read_only(&source, &staging_c, &target_c),
        })
        .await
        .map_err(|e| Status::internal(format!("mount task failed: {e}")))?
    }

    async fn mount_fuse(&self, op: Operator, target: &Path, read_only: bool) -> Result<(), Status> {
        let mut options = MountOptions::default();
        options
            .fs_name("kubedal")
            .allow_other(true)
            .read_only(read_only);
        let handle = fuse3::path::Session::new(options)
            .mount(Filesystem::new(op, 0, 0), target)
            .await
            .map_err(|e| io_status(e, "mount FUSE file system"))?;
        self.fuse_sessions
            .lock()
            .await
            .insert(target.to_path_buf(), handle);
        Ok(())
    }

    async fn unmount(&self, target: &Path) -> Result<(), Status> {
        let session = self.fuse_sessions.lock().await.remove(target);
        if let Some(handle) = session {
            match handle.unmount().await {
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
