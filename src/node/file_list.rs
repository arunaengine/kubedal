//! Prepare file-list volumes: place the files listed in a manifest.

use crate::node::content_cache::ContentCache;
use crate::node::manifest::{self, FilePlan, MAX_MANIFEST_BYTES, PlannedFile};
use crate::node::state::{DataNodeRef, PodRef, UploadPlan, VolumeRecord};
use crate::node::transfer::{self, ByteBudget, CONCURRENCY};
use crate::node::volume::VolumeManager;
use crate::util::error::{io_status, opendal_status};
use futures::StreamExt;
use opendal::Operator;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tonic::Status;

/// Read and check a manifest object.
pub async fn read_manifest(op: &Operator, key: &str) -> Result<FilePlan, Status> {
    // Check the key before any storage access, so ".." cannot leave the DataNode root.
    let key = crate::node::paths::safe_relative(key.trim_start_matches('/'))
        .map_err(|e| Status::invalid_argument(format!("invalid manifest path: {e}")))?;
    let key = key.as_str();
    let meta = op
        .stat(key)
        .await
        .map_err(|e| opendal_status(e, &format!("manifest {key}")))?;
    if meta.content_length() > MAX_MANIFEST_BYTES {
        return Err(Status::invalid_argument(format!(
            "manifest {key} is larger than {MAX_MANIFEST_BYTES} bytes"
        )));
    }
    // The size from stat may be wrong or change, so the read is limited as well.
    let data = read_capped(op, key, MAX_MANIFEST_BYTES).await?;
    manifest::parse(&data)
}

/// Read an object, but fail as soon as it is longer than `max` bytes.
async fn read_capped(op: &Operator, key: &str, max: u64) -> Result<Vec<u8>, Status> {
    let mut stream = op
        .reader(key)
        .await
        .map_err(|e| opendal_status(e, &format!("read manifest {key}")))?
        .into_bytes_stream(..)
        .await
        .map_err(|e| opendal_status(e, &format!("read manifest {key}")))?;
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| io_status(e, &format!("read manifest {key}")))?;
        if (data.len() + chunk.len()) as u64 > max {
            return Err(Status::invalid_argument(format!(
                "manifest {key} is larger than {max} bytes"
            )));
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
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
        let scope = node.to_string();
        // Wait for every started placement, also after an error. A dropped copy
        // task keeps running in the background and could write into the data
        // folder of the next attempt. After the first error no new file starts.
        let first_error: std::sync::Mutex<Option<Status>> = std::sync::Mutex::new(None);
        futures::stream::iter(plan.files.iter())
            .for_each_concurrent(CONCURRENCY, |file| async {
                let failed = || first_error.lock().is_ok_and(|e| e.is_some());
                if failed() {
                    return;
                }
                if let Err(err) = place_file(cache, &op, &scope, &data, file, &budget).await
                    && let Ok(mut slot) = first_error.lock()
                {
                    slot.get_or_insert(err);
                }
            })
            .await;
        // Prune also after a failure, so failed publish calls cannot grow the
        // cache beyond its limit.
        match cache.prune() {
            Ok(report) if report.removed_files > 0 => {
                tracing::info!("content cache pruned: {report:?}")
            }
            Ok(_) => {}
            Err(err) => tracing::warn!("content cache prune failed: {err}"),
        }

        if let Some(err) = first_error.into_inner().unwrap_or_else(|p| p.into_inner()) {
            return Err(err);
        }
        tracing::info!(
            "placed {} files ({} bytes) from manifest {manifest_key} of {node}",
            plan.files.len(),
            budget.used()
        );

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
    scope: &str,
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
            cache
                .fetch_into(
                    op,
                    scope,
                    &file.source,
                    digest,
                    budget,
                    &dest,
                    file.executable,
                )
                .await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use opendal::services::Memory;

    #[tokio::test]
    async fn manifest_path_must_stay_in_the_data_node() {
        let op = Operator::new(Memory::default()).unwrap();
        op.write("private/token.json", "{}").await.unwrap();
        for key in ["../private/token.json", "a/../../private/token.json", ""] {
            let err = read_manifest(&op, key).await.unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{key}");
        }
    }

    #[tokio::test]
    async fn manifest_read_is_limited() {
        let op = Operator::new(Memory::default()).unwrap();
        op.write("m", vec![b'x'; 11]).await.unwrap();
        assert_eq!(read_capped(&op, "m", 11).await.unwrap().len(), 11);
        let err = read_capped(&op, "m", 10).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
