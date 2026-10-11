use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kube::api::{ListParams, ObjectMeta};
use kube::{Api, Client};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tonic::{Request, Response, Status};

use crate::csi::controller_server::Controller;
use crate::csi::{
    ControllerExpandVolumeRequest, ControllerExpandVolumeResponse,
    ControllerGetCapabilitiesRequest, ControllerGetCapabilitiesResponse,
    ControllerModifyVolumeRequest, ControllerModifyVolumeResponse, ControllerPublishVolumeRequest,
    ControllerPublishVolumeResponse, ControllerServiceCapability, ControllerUnpublishVolumeRequest,
    ControllerUnpublishVolumeResponse, CreateVolumeRequest, CreateVolumeResponse,
    DeleteVolumeRequest, DeleteVolumeResponse, GetCapacityRequest, GetCapacityResponse,
    ListVolumesRequest, ListVolumesResponse, ValidateVolumeCapabilitiesRequest,
    ValidateVolumeCapabilitiesResponse, Volume,
};
use crate::node::request::{
    ANN_DATA_NODE_NAME, ANN_DATA_NODE_NAMESPACE, ANN_DATA_POD_NAME, ANN_DATA_POD_NAMESPACE,
    ANN_MOUNT, parse_mount,
};
use crate::util::error::kube_status;

pub struct ControllerService {
    // In-memory storage for volumes (for a dummy driver)
    volumes: Arc<Mutex<HashMap<String, Volume>>>,
    client: Client,
}

impl ControllerService {
    pub async fn new(client: Client, storage_class: String, sync_image: String) -> Self {
        // Spawn the controller / resource reconciler
        tokio::spawn(crate::resource::controller::run(
            client.clone(),
            storage_class,
            sync_image,
        ));

        Self {
            client,
            volumes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Lock the volume map. A poisoned lock still holds valid data, so it is used as is.
    fn lock_volumes(&self) -> std::sync::MutexGuard<'_, HashMap<String, Volume>> {
        self.volumes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[tonic::async_trait]
impl Controller for ControllerService {
    #[tracing::instrument(skip(self))]
    async fn controller_get_capabilities(
        &self,
        _request: Request<ControllerGetCapabilitiesRequest>,
    ) -> Result<Response<ControllerGetCapabilitiesResponse>, Status> {
        // Advertise our dummy driver capabilities
        let create_delete = ControllerServiceCapability {
            r#type: Some(crate::csi::controller_service_capability::Type::Rpc(
                crate::csi::controller_service_capability::Rpc {
                    r#type:
                        crate::csi::controller_service_capability::rpc::Type::CreateDeleteVolume
                            .into(),
                },
            )),
        };

        let response = ControllerGetCapabilitiesResponse {
            capabilities: vec![create_delete],
        };

        Ok(Response::new(response))
    }

    #[tracing::instrument(skip(self, request))]
    async fn create_volume(
        &self,
        request: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeResponse>, Status> {
        let request = request.into_inner();
        tracing::trace!("CreateVolume request: {:?}", request);

        if request.name.is_empty() {
            return Err(Status::invalid_argument("Volume name cannot be empty"));
        }

        // CreateVolume must be idempotent: return the volume created before.
        if let Some(existing_volume) = self.lock_volumes().get(&request.name).cloned() {
            return Ok(Response::new(CreateVolumeResponse {
                volume: Some(existing_volume),
            }));
        }

        let pvc_meta = find_pvc(self.client.clone(), &request.parameters, &request.name).await?;
        let context = context_from_pvc_meta(&pvc_meta)?;

        tracing::trace!("PVC meta: {:?}", pvc_meta);
        tracing::trace!("Volume context: {:?}", context);

        tracing::info!(
            "Creating volume '{}' with annotations '{:?}'",
            request.name,
            pvc_meta.annotations.unwrap_or_default()
        );

        // Create a new volume entry
        let volume_id = volume_id_for(&request.name, &request.parameters);
        let capacity_bytes = request
            .capacity_range
            .map_or(5 * 1024 * 1024 * 1024, |range| range.required_bytes);

        let volume = Volume {
            volume_id: volume_id.clone(),
            capacity_bytes,
            content_source: None,
            accessible_topology: vec![],
            volume_context: context,
        };

        self.lock_volumes().insert(request.name, volume.clone());

        Ok(Response::new(CreateVolumeResponse {
            volume: Some(volume),
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn delete_volume(
        &self,
        request: Request<DeleteVolumeRequest>,
    ) -> Result<Response<DeleteVolumeResponse>, Status> {
        let request = request.into_inner();
        let volume_id = request.volume_id;

        if volume_id.is_empty() {
            return Err(Status::invalid_argument("Volume ID cannot be empty"));
        }

        tracing::info!("Deleting volume with ID '{}'", volume_id);

        // In a real driver, we would clean up any resources associated with this volume
        // For our dummy driver, we'll just remove it from our in-memory map

        let mut volumes = self.lock_volumes();

        // Find and remove the volume by ID
        let volume_key = volumes
            .iter()
            .find(|(_, v)| v.volume_id == volume_id)
            .map(|(k, _)| k.clone());

        if let Some(key) = volume_key {
            volumes.remove(&key);
        }

        Ok(Response::new(DeleteVolumeResponse {}))
    }

    #[tracing::instrument(skip(self))]
    async fn controller_publish_volume(
        &self,
        _request: Request<ControllerPublishVolumeRequest>,
    ) -> Result<Response<ControllerPublishVolumeResponse>, Status> {
        // Our dummy driver doesn't need to do anything for publish
        Err(Status::unimplemented(
            "ControllerPublishVolume is not implemented",
        ))
    }

    #[tracing::instrument(skip(self))]
    async fn controller_unpublish_volume(
        &self,
        _request: Request<ControllerUnpublishVolumeRequest>,
    ) -> Result<Response<ControllerUnpublishVolumeResponse>, Status> {
        // Our dummy driver doesn't need to do anything for unpublish
        Err(Status::unimplemented(
            "ControllerUnpublishVolume is not implemented",
        ))
    }

    #[tracing::instrument(skip(self))]
    async fn validate_volume_capabilities(
        &self,
        _request: Request<ValidateVolumeCapabilitiesRequest>,
    ) -> Result<Response<ValidateVolumeCapabilitiesResponse>, Status> {
        // For a dummy driver, we'll say all capabilities are supported
        Ok(Response::new(ValidateVolumeCapabilitiesResponse {
            confirmed: None,
            message: "All capabilities are supported".into(),
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn list_volumes(
        &self,
        _request: Request<ListVolumesRequest>,
    ) -> Result<Response<ListVolumesResponse>, Status> {
        // List all volumes in our in-memory storage
        let volumes = self.lock_volumes();
        let entries = volumes
            .values()
            .map(|v| crate::csi::list_volumes_response::Entry {
                volume: Some(v.clone()),
                status: None,
            })
            .collect();

        Ok(Response::new(ListVolumesResponse {
            entries,
            next_token: "".into(),
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn get_capacity(
        &self,
        _request: Request<GetCapacityRequest>,
    ) -> Result<Response<GetCapacityResponse>, Status> {
        // For a dummy driver, we'll report a fixed large capacity
        Ok(Response::new(GetCapacityResponse {
            available_capacity: 1024 * 1024 * 1024 * 1024, // 1 TB
            ..Default::default()
        }))
    }

    #[tracing::instrument(skip(self))]
    async fn controller_expand_volume(
        &self,
        _request: Request<ControllerExpandVolumeRequest>,
    ) -> Result<Response<ControllerExpandVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "ControllerExpandVolume is not implemented",
        ))
    }

    #[tracing::instrument(skip(self))]
    async fn create_snapshot(
        &self,
        _request: Request<crate::csi::CreateSnapshotRequest>,
    ) -> Result<Response<crate::csi::CreateSnapshotResponse>, Status> {
        tracing::info!("Creating snapshot for {:?}", _request);
        Err(Status::unimplemented("CreateSnapshot is not implemented"))
    }

    #[tracing::instrument(skip(self))]
    async fn delete_snapshot(
        &self,
        _request: Request<crate::csi::DeleteSnapshotRequest>,
    ) -> Result<Response<crate::csi::DeleteSnapshotResponse>, Status> {
        Err(Status::unimplemented("DeleteSnapshot is not implemented"))
    }

    #[tracing::instrument(skip(self))]
    async fn list_snapshots(
        &self,
        _request: Request<crate::csi::ListSnapshotsRequest>,
    ) -> Result<Response<crate::csi::ListSnapshotsResponse>, Status> {
        Err(Status::unimplemented("ListSnapshots is not implemented"))
    }

    #[tracing::instrument(skip(self))]
    async fn controller_get_volume(
        &self,
        _request: Request<crate::csi::ControllerGetVolumeRequest>,
    ) -> Result<Response<crate::csi::ControllerGetVolumeResponse>, Status> {
        Err(Status::unimplemented(
            "ControllerGetVolume is not implemented",
        ))
    }

    #[tracing::instrument(skip(self))]
    async fn controller_modify_volume(
        &self,
        _request: tonic::Request<ControllerModifyVolumeRequest>,
    ) -> std::result::Result<tonic::Response<ControllerModifyVolumeResponse>, tonic::Status> {
        Err(Status::unimplemented(
            "ControllerModifyVolume is not implemented",
        ))
    }
}

/// The volume ID for a CreateVolume request. It only depends on the request
/// name and parameters, so retries, overlapping calls and calls after a restart
/// all get the same ID.
fn volume_id_for(name: &str, parameters: &HashMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let mut entries: Vec<_> = parameters.iter().collect();
    entries.sort();
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    for (key, value) in entries {
        hasher.update([0u8]);
        hasher.update(key.as_bytes());
        hasher.update([0u8]);
        hasher.update(value.as_bytes());
    }
    format!(
        "kubedal-{}",
        &crate::node::paths::to_hex(&hasher.finalize())[..32]
    )
}

/// Find the PVC that a CreateVolume call is for.
///
/// With the csi-provisioner flag `--extra-create-metadata`, the parameters hold the
/// PVC name and namespace. Without it, the PVC is found by its UID (the volume name
/// is `pvc-<uid>`) in the namespace given by the `resourceNamespace` parameter.
async fn find_pvc(
    client: Client,
    parameters: &HashMap<String, String>,
    volume_name: &str,
) -> Result<ObjectMeta, Status> {
    if let (Some(name), Some(namespace)) = (
        parameters.get("csi.storage.k8s.io/pvc/name"),
        parameters.get("csi.storage.k8s.io/pvc/namespace"),
    ) {
        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client, namespace);
        let pvc = pvcs
            .get_metadata(name)
            .await
            .map_err(|e| kube_status(e, &format!("PVC {namespace}/{name}")))?;
        return Ok(pvc.metadata);
    }

    let namespace = parameters.get("resourceNamespace").ok_or_else(|| {
        Status::invalid_argument(
            "cannot find the PVC: enable --extra-create-metadata in csi-provisioner \
             or set the resourceNamespace parameter in the StorageClass",
        )
    })?;
    let uid = volume_name.strip_prefix("pvc-").ok_or_else(|| {
        Status::invalid_argument(format!(
            "volume name {volume_name} does not start with pvc-"
        ))
    })?;
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client, namespace);
    let all_pvc = pvcs
        .list_metadata(&ListParams::default())
        .await
        .map_err(|e| kube_status(e, &format!("PVCs in {namespace}")))?;
    all_pvc
        .items
        .into_iter()
        .find(|pvc| pvc.metadata.uid.as_deref() == Some(uid))
        .map(|pvc| pvc.metadata)
        .ok_or_else(|| Status::not_found(format!("PVC with uid {uid} not found in {namespace}")))
}

/// Build the volume context from the kubedal annotations of a PVC.
///
/// A PVC names a DataPod, a DataNode, or both. Namespaces default to the PVC namespace.
fn context_from_pvc_meta(pvc_meta: &ObjectMeta) -> Result<HashMap<String, String>, Status> {
    let annotations = pvc_meta.annotations.clone().unwrap_or_default();
    let pvc_namespace = pvc_meta.namespace.clone().unwrap_or_default();
    let mut context = HashMap::new();

    for (name_key, namespace_key) in [
        (ANN_DATA_NODE_NAME, ANN_DATA_NODE_NAMESPACE),
        (ANN_DATA_POD_NAME, ANN_DATA_POD_NAMESPACE),
    ] {
        if let Some(name) = annotations.get(name_key).filter(|v| !v.is_empty()) {
            context.insert(name_key.to_string(), name.clone());
            let namespace = annotations
                .get(namespace_key)
                .cloned()
                .unwrap_or_else(|| pvc_namespace.clone());
            context.insert(namespace_key.to_string(), namespace);
        }
    }
    if context.is_empty() {
        return Err(Status::invalid_argument(format!(
            "PVC needs the annotation {ANN_DATA_POD_NAME} or {ANN_DATA_NODE_NAME}"
        )));
    }

    let mount = annotations
        .get(ANN_MOUNT)
        .cloned()
        .unwrap_or_else(|| "cache-read-only".to_string());
    // Reject unknown modes now instead of when a pod starts.
    parse_mount(&mount)?;
    context.insert(ANN_MOUNT.to_string(), mount);
    Ok(context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn meta(annotations: &[(&str, &str)]) -> ObjectMeta {
        ObjectMeta {
            namespace: Some("team".into()),
            annotations: Some(
                annotations
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<BTreeMap<_, _>>(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn volume_id_is_stable_per_request() {
        let params = HashMap::from([("a".to_string(), "1".to_string())]);
        let id = volume_id_for("pvc-1", &params);
        assert_eq!(id, volume_id_for("pvc-1", &params.clone()));
        assert_ne!(id, volume_id_for("pvc-2", &params));
        assert_ne!(id, volume_id_for("pvc-1", &HashMap::new()));
        assert!(id.starts_with("kubedal-"));
    }

    #[test]
    fn data_pod_annotation_is_enough() {
        let ctx = context_from_pvc_meta(&meta(&[(ANN_DATA_POD_NAME, "dp")])).unwrap();
        assert_eq!(ctx[ANN_DATA_POD_NAME], "dp");
        assert_eq!(ctx[ANN_DATA_POD_NAMESPACE], "team");
        assert_eq!(ctx[ANN_MOUNT], "cache-read-only");
        assert!(!ctx.contains_key(ANN_DATA_NODE_NAME));
    }

    #[test]
    fn missing_or_bad_annotations_fail() {
        assert!(context_from_pvc_meta(&meta(&[])).is_err());
        assert!(
            context_from_pvc_meta(&meta(&[(ANN_DATA_NODE_NAME, "dn"), (ANN_MOUNT, "nfs")]))
                .is_err()
        );
    }
}
