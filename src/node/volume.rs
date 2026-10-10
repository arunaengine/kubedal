//! Publish and unpublish volumes on a node.
//!
//! Local layout below the data directory:
//! - `state/<id>.json`: one record per published volume (see [`StateStore`])
//! - `volumes/<id>/data`: the files of a cache or file-list volume
//! - `volumes/<id>/baseline.json`: file sizes and times right after the download

use crate::node::cluster::{Cluster, EventKind, ResolvedNode};
use crate::node::content_cache::ContentCache;
use crate::node::mounter::Mounter;
use crate::node::paths::{self, publish_id};
use crate::node::state::{
    DataNodeRef, Phase, PodRef, StateStore, UploadPlan, VolumeMode, VolumeRecord,
};
use crate::node::transfer::{self, ByteBudget};
use crate::node::upload::{self, RetryPolicy, UploadReport};
use crate::util::error::io_status;
use crate::util::fuse_fs::states as spool;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tonic::Status;

/// Default size limit of the node content cache (10 GiB).
pub const DEFAULT_CONTENT_CACHE_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// How a DataNode prefix is presented in the pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeMode {
    /// Copy all files to the node before the pod starts.
    Cache,
    /// Serve files directly from storage through FUSE.
    Fuse,
}

/// Where the volume content comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeSource {
    /// Everything below `path` in a DataNode.
    Tree {
        node: DataNodeRef,
        path: String,
        mode: TreeMode,
        writable: bool,
    },
    /// Files listed in a manifest object inside a DataNode.
    FileList { node: DataNodeRef, manifest: String },
}

/// A request to make a volume available at a target path.
#[derive(Debug, Clone)]
pub struct PublishRequest {
    pub volume_id: String,
    pub target_path: PathBuf,
    /// The CSI `readonly` flag. It overrides a writable source.
    pub read_only: bool,
    pub pod: PodRef,
    pub source: VolumeSource,
}

pub struct VolumeManager {
    root: PathBuf,
    state: StateStore,
    cluster: Arc<dyn Cluster>,
    mounter: Arc<dyn Mounter>,
    busy: Mutex<HashSet<String>>,
    retry: RetryPolicy,
    content: ContentCache,
}

/// Marks a volume as busy while an operation runs on it.
struct BusyGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    id: String,
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        let mut set = self.set.lock().unwrap_or_else(|p| p.into_inner());
        set.remove(&self.id);
    }
}

impl VolumeManager {
    pub fn new(
        root: impl Into<PathBuf>,
        cluster: Arc<dyn Cluster>,
        mounter: Arc<dyn Mounter>,
    ) -> std::io::Result<Self> {
        let root = root.into();
        let state = StateStore::open(root.join("state"))?;
        std::fs::create_dir_all(root.join("volumes"))?;
        let content = ContentCache::open(&root.join("content"), DEFAULT_CONTENT_CACHE_BYTES)?;
        Ok(Self {
            root,
            state,
            cluster,
            mounter,
            busy: Mutex::new(HashSet::new()),
            retry: RetryPolicy::default(),
            content,
        })
    }

    /// Change how failed uploads are retried.
    /// Change the size limit of the node content cache.
    pub fn with_content_cache_limit(mut self, max_bytes: u64) -> Self {
        self.content.set_max_bytes(max_bytes);
        self
    }

    pub fn content_cache(&self) -> &ContentCache {
        &self.content
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn state(&self) -> &StateStore {
        &self.state
    }

    pub fn cluster(&self) -> &Arc<dyn Cluster> {
        &self.cluster
    }

    pub fn mounter(&self) -> &Arc<dyn Mounter> {
        &self.mounter
    }

    pub fn volume_dir(&self, id: &str) -> PathBuf {
        self.root.join("volumes").join(id)
    }

    pub fn data_dir(&self, id: &str) -> PathBuf {
        self.volume_dir(id).join("data")
    }

    /// Spool files of a FUSE volume: content that is not uploaded yet.
    pub fn spool_dir(&self, id: &str) -> PathBuf {
        self.volume_dir(id).join("spool")
    }

    fn baseline_path(&self, id: &str) -> PathBuf {
        self.volume_dir(id).join("baseline.json")
    }

    /// The CSI spec asks to return ABORTED while another call for the same volume runs.
    fn begin(&self, id: &str) -> Result<BusyGuard<'_>, Status> {
        let mut set = self.busy.lock().unwrap_or_else(|p| p.into_inner());
        if !set.insert(id.to_string()) {
            return Err(Status::aborted(
                "another operation for this volume is in progress",
            ));
        }
        Ok(BusyGuard {
            set: &self.busy,
            id: id.to_string(),
        })
    }

    fn save(&self, id: &str, record: &VolumeRecord) -> Result<(), Status> {
        self.state
            .save(id, record)
            .map_err(|e| io_status(e, "save volume state"))
    }

    /// Run [`VolumeManager::publish`] in its own task and wait for it.
    ///
    /// If the caller stops waiting (for example a gRPC call that kubelet cancels
    /// after a timeout), the work still runs to its end. So the volume stays
    /// marked as busy until all started work, including blocking copies, is done,
    /// and a retry cannot run on top of it.
    pub async fn publish_detached(self: &Arc<Self>, req: PublishRequest) -> Result<(), Status> {
        let manager = self.clone();
        tokio::spawn(async move { manager.publish(req).await })
            .await
            .map_err(|e| Status::internal(format!("publish task failed: {e}")))?
    }

    /// Run [`VolumeManager::unpublish`] in its own task and wait for it.
    /// See [`VolumeManager::publish_detached`].
    pub async fn unpublish_detached(
        self: &Arc<Self>,
        volume_id: &str,
        target_path: &Path,
    ) -> Result<(), Status> {
        let manager = self.clone();
        let volume_id = volume_id.to_string();
        let target_path = target_path.to_path_buf();
        tokio::spawn(async move { manager.unpublish(&volume_id, &target_path).await })
            .await
            .map_err(|e| Status::internal(format!("unpublish task failed: {e}")))?
    }

    pub async fn publish(&self, req: PublishRequest) -> Result<(), Status> {
        let target = req.target_path.to_string_lossy().to_string();
        let id = publish_id(&req.volume_id, &target);
        let _guard = self.begin(&id)?;

        let (mode, read_only) = match &req.source {
            VolumeSource::Tree { mode, writable, .. } => {
                let mode = match mode {
                    TreeMode::Cache => VolumeMode::Cache,
                    TreeMode::Fuse => VolumeMode::Fuse,
                };
                (mode, req.read_only || !writable)
            }
            VolumeSource::FileList { .. } => (VolumeMode::FileList, req.read_only),
        };

        let existing = self
            .state
            .load(&id)
            .map_err(|e| io_status(e, "load volume state"))?;
        if let Some(record) = existing {
            if record.mode != mode || record.read_only != read_only {
                return Err(Status::already_exists(
                    "the volume is already published at this path with other settings",
                ));
            }
            if self.resume_publish(&id, &record, &req.target_path).await? {
                return Ok(());
            }
        }
        let mut record = VolumeRecord {
            volume_id: req.volume_id.clone(),
            target_path: req.target_path.clone(),
            pod: req.pod.clone(),
            mode,
            read_only,
            phase: Phase::Preparing,
            upload: None,
        };
        self.save(&id, &record)?;

        match self.prepare(&id, &req, &mut record).await {
            Ok(()) => {
                record.phase = Phase::Published;
                self.save(&id, &record)
            }
            Err(err) => {
                tracing::warn!("publish of {} failed: {}", req.volume_id, err.message());
                if let Err(cleanup) = self.discard(&id, &req.target_path).await {
                    tracing::warn!("cleanup after failed publish failed: {cleanup}");
                }
                Err(err)
            }
        }
    }

    /// Handle a publish call for a volume that has a record already.
    /// Returns true if nothing more needs to be done.
    async fn resume_publish(
        &self,
        id: &str,
        record: &VolumeRecord,
        target: &Path,
    ) -> Result<bool, Status> {
        match record.phase {
            Phase::Unpublishing => Err(Status::failed_precondition(
                "the volume is still being unpublished and its upload has not finished",
            )),
            Phase::Preparing => {
                // A publish call was interrupted. Start over.
                self.discard(id, target).await?;
                Ok(false)
            }
            Phase::Published => {
                match record.mode {
                    VolumeMode::Fuse => {
                        if self.mounter.has_fuse_session(target).await {
                            return Ok(true);
                        }
                        // The FUSE process from before a restart is gone. Mount again.
                        self.mounter.unmount(target).await?;
                        self.state
                            .remove(id)
                            .map_err(|e| io_status(e, "remove volume state"))?;
                        Ok(false)
                    }
                    VolumeMode::Cache | VolumeMode::FileList => {
                        if self.mounter.is_mounted(target) {
                            return Ok(true);
                        }
                        let data = self.data_dir(id);
                        if !data.is_dir() {
                            self.discard(id, target).await?;
                            return Ok(false);
                        }
                        // The data is still here (for example after a node reboot).
                        transfer::create_dir_all(target).await?;
                        self.mounter.bind(&data, target, record.read_only).await?;
                        Ok(true)
                    }
                }
            }
        }
    }

    async fn prepare(
        &self,
        id: &str,
        req: &PublishRequest,
        record: &mut VolumeRecord,
    ) -> Result<(), Status> {
        match &req.source {
            VolumeSource::Tree {
                node, path, mode, ..
            } => {
                let resolved = self.cluster.resolve(node, &req.pod).await?;
                if !record.read_only {
                    resolved.require_writable()?;
                }
                match mode {
                    TreeMode::Cache => self.prepare_cache(id, &resolved, path, record).await,
                    TreeMode::Fuse => {
                        let prefix = paths::normalize_prefix(path).map_err(|e| {
                            Status::invalid_argument(format!("invalid path {path:?}: {e}"))
                        })?;
                        let op = resolved.operator(&prefix)?;
                        let spool_dir = self.spool_dir(id);
                        if !record.read_only {
                            // Content left from an earlier mount goes first, so it
                            // cannot overwrite newer writes later.
                            spool::upload_pending(&op, &spool_dir).await?;
                            record.upload = Some(UploadPlan {
                                data_node: resolved.node.clone(),
                                prefix: prefix.clone(),
                                local_path: String::new(),
                                only_changed: false,
                            });
                        }
                        transfer::create_dir_all(&req.target_path).await?;
                        self.mounter
                            .mount_fuse(op, &req.target_path, record.read_only, &spool_dir)
                            .await
                    }
                }
            }
            VolumeSource::FileList { node, manifest } => {
                self.prepare_file_list(id, node, manifest, &req.pod, record)
                    .await
            }
        }
    }

    async fn prepare_cache(
        &self,
        id: &str,
        resolved: &ResolvedNode,
        path: &str,
        record: &mut VolumeRecord,
    ) -> Result<(), Status> {
        let data = self.fresh_data_dir(id).await?;
        let op = resolved.operator("")?;
        let tree = transfer::list_tree(&op, path).await?;
        let budget = ByteBudget::new(resolved.limit);
        transfer::download_tree(&op, &tree, &data, &budget).await?;
        tracing::info!(
            "downloaded {} files ({} bytes) from {}",
            tree.files.len(),
            budget.used(),
            resolved.node
        );

        if !record.read_only {
            // Remember the downloaded state, so only new and changed files go back.
            let snapshot_dir = data.clone();
            let baseline = tokio::task::spawn_blocking(move || upload::snapshot(&snapshot_dir))
                .await
                .map_err(|e| Status::internal(format!("snapshot task failed: {e}")))?
                .map_err(|e| io_status(e, "snapshot volume content"))?;
            upload::save_baseline(&self.baseline_path(id), &baseline)?;
            record.upload = Some(UploadPlan {
                data_node: resolved.node.clone(),
                prefix: tree.base.clone(),
                local_path: String::new(),
                only_changed: true,
            });
        }

        transfer::create_dir_all(&record.target_path).await?;
        self.mounter
            .bind(&data, &record.target_path, record.read_only)
            .await
    }

    /// Create an empty data directory for a volume, removing old content.
    pub(crate) async fn fresh_data_dir(&self, id: &str) -> Result<PathBuf, Status> {
        let dir = self.volume_dir(id);
        remove_dir_all(&dir).await?;
        let data = self.data_dir(id);
        transfer::create_dir_all(&data).await?;
        Ok(data)
    }

    /// Unmount and delete everything of a recorded volume.
    pub(crate) async fn discard_record(
        &self,
        id: &str,
        record: &VolumeRecord,
    ) -> Result<(), Status> {
        self.discard(id, &record.target_path).await
    }

    /// Unmount and delete everything of a volume that has nothing left to upload.
    async fn discard(&self, id: &str, target: &Path) -> Result<(), Status> {
        self.mounter.unmount(target).await?;
        remove_dir_all(&self.volume_dir(id)).await?;
        self.state
            .remove(id)
            .map_err(|e| io_status(e, "remove volume state"))
    }

    pub async fn unpublish(&self, volume_id: &str, target_path: &Path) -> Result<(), Status> {
        let target = target_path.to_string_lossy().to_string();
        let id = publish_id(volume_id, &target);
        let _guard = self.begin(&id)?;

        let record = self
            .state
            .load(&id)
            .map_err(|e| io_status(e, "load volume state"))?;

        match record {
            None => {
                // Unknown volume: make sure nothing is left mounted.
                self.mounter.unmount(target_path).await?;
            }
            Some(mut record) => {
                if record.phase == Phase::Published {
                    record.phase = Phase::Unpublishing;
                    self.save(&id, &record)?;
                }
                self.mounter.unmount(target_path).await?;
                if record.phase == Phase::Unpublishing
                    && let Some(plan) = record.upload.clone()
                {
                    // Local data is only removed after a successful upload.
                    self.write_back(&id, &record, &plan).await?;
                }
                self.discard(&id, target_path).await?;
            }
        }

        remove_target_dir(target_path).await
    }
}

impl VolumeManager {
    /// Upload the volume content as described by the plan, and tell the pod about it.
    async fn write_back(
        &self,
        id: &str,
        record: &VolumeRecord,
        plan: &UploadPlan,
    ) -> Result<(), Status> {
        let destination = format!("{}:{}", plan.data_node, plan.prefix);
        match self.try_write_back(id, record, plan).await {
            Ok(report) => {
                let message = format!(
                    "uploaded {} files ({} bytes) to {destination}, {} unchanged files skipped",
                    report.files, report.bytes, report.skipped
                );
                tracing::info!("volume {}: {message}", record.volume_id);
                self.cluster
                    .pod_event(&record.pod, EventKind::Normal, "VolumeUploaded", &message)
                    .await;
                Ok(())
            }
            Err(err) => {
                let message = format!(
                    "upload to {destination} failed, the data is kept on the node and the \
                     upload will be retried: {}",
                    err.message()
                );
                tracing::warn!("volume {}: {message}", record.volume_id);
                self.cluster
                    .pod_event(
                        &record.pod,
                        EventKind::Warning,
                        "VolumeUploadFailed",
                        &message,
                    )
                    .await;
                Err(Status::new(err.code(), message))
            }
        }
    }

    async fn try_write_back(
        &self,
        id: &str,
        record: &VolumeRecord,
        plan: &UploadPlan,
    ) -> Result<UploadReport, Status> {
        let resolved = self.cluster.resolve(&plan.data_node, &record.pod).await?;
        resolved.require_writable()?;
        if record.mode == VolumeMode::Fuse {
            // FUSE writes went to storage already; only spool files are left.
            let op = resolved.operator(&plan.prefix)?;
            let files = spool::upload_pending(&op, &self.spool_dir(id)).await?;
            return Ok(UploadReport {
                files,
                ..Default::default()
            });
        }
        let op = resolved.operator("")?;
        let baseline = if plan.only_changed {
            upload::load_baseline(&self.baseline_path(id))?
        } else {
            None
        };
        upload::upload_tree(
            &op,
            &self.data_dir(id),
            &plan.local_path,
            &plan.prefix,
            baseline.as_ref(),
            self.retry,
        )
        .await
    }
}

async fn remove_dir_all(path: &Path) -> Result<(), Status> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(io_status(err, &format!("remove {}", path.display()))),
    }
}

/// Remove the target directory if it is empty. Content is never deleted here.
async fn remove_target_dir(path: &Path) -> Result<(), Status> {
    match tokio::fs::remove_dir(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) if err.raw_os_error() == Some(libc::ENOTEMPTY) => {
            tracing::warn!("target path {path:?} is not empty after unmount; leaving it");
            Ok(())
        }
        Err(err) => Err(io_status(err, "remove target path")),
    }
}
