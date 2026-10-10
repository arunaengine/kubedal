//! Clean up after a restart of the node plugin.
//!
//! Bind mounts of cache volumes survive a restart, but FUSE mounts do not: their
//! file system process is gone, and access to them fails. Interrupted publish calls
//! may also have left partial data behind.

use crate::node::cluster::EventKind;
use crate::node::state::{Phase, VolumeMode, VolumeRecord};
use crate::node::volume::VolumeManager;
use tonic::Status;

/// What was found on the node for one volume record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    pub target_exists: bool,
    pub target_mounted: bool,
}

/// What to do with one volume record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Everything is fine, or kubelet will call publish or unpublish again.
    Keep,
    /// A FUSE mount without its process. Unmount it, so the pod fails visibly
    /// instead of hanging, and keep the record for the coming unpublish.
    UnmountStaleFuse,
    /// Unmount, then remove the data and the record. Nothing needs uploading.
    Discard,
    /// Data is waiting for an upload. Keep it until unpublish succeeds.
    KeepPendingUpload,
}

/// Decide what to do with a record. This function has no side effects.
pub fn plan(record: &VolumeRecord, observed: Observed) -> Action {
    let has_upload = record.upload.is_some();
    match record.phase {
        Phase::Preparing => Action::Discard,
        Phase::Unpublishing if has_upload => Action::KeepPendingUpload,
        Phase::Unpublishing => Action::Discard,
        Phase::Published => match record.mode {
            VolumeMode::Fuse if observed.target_mounted => Action::UnmountStaleFuse,
            // Spool files of a writable FUSE volume may wait for upload.
            VolumeMode::Fuse if !observed.target_exists && has_upload => Action::KeepPendingUpload,
            VolumeMode::Fuse if !observed.target_exists => Action::Discard,
            VolumeMode::Fuse => Action::Keep,
            VolumeMode::Cache | VolumeMode::FileList => {
                if observed.target_mounted || observed.target_exists {
                    Action::Keep
                } else if has_upload {
                    // The pod is gone without an unpublish call. Never drop data that
                    // was not uploaded.
                    Action::KeepPendingUpload
                } else {
                    Action::Discard
                }
            }
        },
    }
}

/// Counts of what reconcile did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub kept: usize,
    pub stale_fuse_unmounted: usize,
    pub discarded: usize,
    pub pending_upload: usize,
    pub failed: usize,
}

impl VolumeManager {
    /// Check all volume records once at startup and act on them.
    pub async fn reconcile(&self) -> Result<ReconcileReport, Status> {
        let records = self
            .state()
            .list()
            .map_err(|e| crate::util::error::io_status(e, "list volume state"))?;
        let mut report = ReconcileReport::default();
        for (id, record) in records {
            let target = record.target_path.clone();
            let observed = Observed {
                target_exists: target.exists(),
                target_mounted: self.mounter().is_mounted(&target),
            };
            let action = plan(&record, observed);
            tracing::info!(
                "reconcile volume {} at {}: {action:?}",
                record.volume_id,
                target.display()
            );
            let result = match action {
                Action::Keep => {
                    report.kept += 1;
                    Ok(())
                }
                Action::KeepPendingUpload => {
                    report.pending_upload += 1;
                    Ok(())
                }
                Action::UnmountStaleFuse => {
                    report.stale_fuse_unmounted += 1;
                    self.unmount_stale_fuse(&record).await
                }
                Action::Discard => {
                    report.discarded += 1;
                    self.discard_record(&id, &record).await
                }
            };
            if let Err(err) = result {
                report.failed += 1;
                tracing::warn!(
                    "reconcile of volume {} failed: {}",
                    record.volume_id,
                    err.message()
                );
            }
        }
        Ok(report)
    }

    async fn unmount_stale_fuse(&self, record: &VolumeRecord) -> Result<(), Status> {
        self.mounter().unmount(&record.target_path).await?;
        self.cluster()
            .pod_event(
                &record.pod,
                EventKind::Warning,
                "FuseMountLost",
                "the FUSE mount of this volume ended when the kubedal node plugin restarted; \
                 restart the pod to get a working volume",
            )
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::state::tests::sample_record;

    fn observed(target_exists: bool, target_mounted: bool) -> Observed {
        Observed {
            target_exists,
            target_mounted,
        }
    }

    fn record(phase: Phase, mode: VolumeMode, upload: bool) -> VolumeRecord {
        let mut r = sample_record();
        r.phase = phase;
        r.mode = mode;
        if !upload {
            r.upload = None;
        }
        r
    }

    #[test]
    fn interrupted_publish_is_discarded() {
        for mode in [VolumeMode::Cache, VolumeMode::Fuse, VolumeMode::FileList] {
            let r = record(Phase::Preparing, mode, false);
            assert_eq!(plan(&r, observed(true, true)), Action::Discard);
        }
    }

    #[test]
    fn pending_uploads_are_kept() {
        let r = record(Phase::Unpublishing, VolumeMode::Cache, true);
        assert_eq!(plan(&r, observed(false, false)), Action::KeepPendingUpload);
        let r = record(Phase::Unpublishing, VolumeMode::Cache, false);
        assert_eq!(plan(&r, observed(false, false)), Action::Discard);
    }

    #[test]
    fn mounted_fuse_without_process_is_unmounted() {
        let r = record(Phase::Published, VolumeMode::Fuse, false);
        assert_eq!(plan(&r, observed(true, true)), Action::UnmountStaleFuse);
        assert_eq!(plan(&r, observed(true, false)), Action::Keep);
        assert_eq!(plan(&r, observed(false, false)), Action::Discard);
        let r = record(Phase::Published, VolumeMode::Fuse, true);
        assert_eq!(plan(&r, observed(false, false)), Action::KeepPendingUpload);
    }

    #[test]
    fn cache_mounts_survive() {
        let r = record(Phase::Published, VolumeMode::Cache, true);
        assert_eq!(plan(&r, observed(true, true)), Action::Keep);
        // After a reboot the target exists but is not mounted; kubelet publishes again.
        assert_eq!(plan(&r, observed(true, false)), Action::Keep);
        // The pod is gone: keep data that still needs an upload.
        assert_eq!(plan(&r, observed(false, false)), Action::KeepPendingUpload);
        let r = record(Phase::Published, VolumeMode::FileList, false);
        assert_eq!(plan(&r, observed(false, false)), Action::Discard);
    }
}
