//! Prepare file-list volumes: place the files listed in a manifest.

use crate::node::content_cache::ContentCache;
use crate::node::manifest::{self, FilePlan, MAX_MANIFEST_BYTES, PlannedFile};
use crate::node::state::{DataNodeRef, PodRef, UploadPlan, VolumeRecord};
use crate::node::transfer::{self, ByteBudget, CONCURRENCY};
use crate::node::volume::VolumeManager;
use crate::util::error::{io_status, opendal_status};
use futures::TryStreamExt;
use opendal::Operator;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tonic::Status;

/// Read and check a manifest object.
pub async fn read_manifest(op: &Operator, key: &str) -> Result<FilePlan, Status> {
    let key = key.trim_start_matches('/');
    let meta = op
        .stat(key)
        .await
        .map_err(|e| opendal_status(e, &format!("manifest {key}")))?;
    if meta.content_length() > MAX_MANIFEST_BYTES {
        return Err(Status::invalid_argument(format!(
            "manifest {key} is larger than {MAX_MANIFEST_BYTES} bytes"
        )));
    }
    let data = op
        .read(key)
        .await
        .map_err(|e| opendal_status(e, &format!("read manifest {key}")))?;
    manifest::parse(&data.to_vec())
}

impl VolumeManager {
    pub(crate) async fn prepare_file_list(
        &self,
        id: &str,
        node: &DataNodeRef,
        manifest_key: &str,
        pod: &PodRef,
        record: &mut VolumeRecord,
    ) -> Result<(), Status> {
        let resolved = self.cluster().resolve(node, pod).await?;
        let op = resolved.operator("")?;
        let plan = read_manifest(&op, manifest_key).await?;

        // Check the upload target now, so a pod never runs without a way to save results.
        let upload = match &plan.upload {
            None => None,
            Some(_) if record.read_only => {
                return Err(Status::invalid_argument(
                    "a read-only file-list volume cannot have an upload target",
                ));
            }
            Some(up) => {
                let target = DataNodeRef {
                    name: up.data_node.clone(),
                    namespace: up
                        .data_node_namespace
                        .clone()
                        .unwrap_or_else(|| pod.namespace.clone()),
                };
                self.cluster()
                    .resolve(&target, pod)
                    .await?
                    .require_writable()?;
                Some(UploadPlan {
                    data_node: target,
                    prefix: up.prefix.clone(),
                    local_path: up.path.clone(),
                    only_changed: false,
                })
            }
        };

        let data = self.fresh_data_dir(id).await?;
        let budget = ByteBudget::new(resolved.limit);
        budget.check_total(plan.declared_size())?;

        for dir in &plan.directories {
            transfer::create_dir_all(&data.join(dir)).await?;
        }
        if let Some(up) = &upload
            && !up.local_path.is_empty()
        {
            transfer::create_dir_all(&data.join(&up.local_path)).await?;
        }

        let cache = self.content_cache();
        futures::stream::iter(plan.files.iter().map(Ok::<_, Status>))
            .try_for_each_concurrent(CONCURRENCY, |file| {
                place_file(cache, &op, &data, file, &budget)
            })
            .await?;
        tracing::info!(
            "placed {} files ({} bytes) from manifest {manifest_key} of {node}",
            plan.files.len(),
            budget.used()
        );

        match cache.prune() {
            Ok(report) if report.removed_files > 0 => {
                tracing::info!("content cache pruned: {report:?}")
            }
            Ok(_) => {}
            Err(err) => tracing::warn!("content cache prune failed: {err}"),
        }

        record.upload = upload;
        transfer::create_dir_all(&record.target_path).await?;
        self.mounter()
            .bind(&data, &record.target_path, record.read_only)
            .await
    }
}

async fn place_file(
    cache: &ContentCache,
    op: &Operator,
    data: &Path,
    file: &PlannedFile,
    budget: &ByteBudget,
) -> Result<(), Status> {
    let dest = data.join(&file.path);
    if let Some(parent) = dest.parent() {
        transfer::create_dir_all(parent).await?;
    }
    let size = match &file.sha256 {
        Some(digest) => {
            let cached = cache.fetch(op, &file.source, digest, budget).await?;
            ContentCache::place(&cached, &dest, file.executable).await?;
            tokio::fs::metadata(&dest)
                .await
                .map_err(|e| io_status(e, "stat placed file"))?
                .len()
        }
        None => {
            let downloaded =
                transfer::download_file(op, &file.source, &dest, budget, false).await?;
            if file.executable {
                tokio::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
                    .await
                    .map_err(|e| io_status(e, "make file executable"))?;
            }
            downloaded.size
        }
    };
    if let Some(expected) = file.size
        && expected != size
    {
        return Err(Status::data_loss(format!(
            "{} has {size} bytes, but the manifest expects {expected}",
            file.source
        )));
    }
    Ok(())
}
