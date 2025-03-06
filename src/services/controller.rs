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
use crate::resource::crd::Datasource;

pub struct ControllerService {
    // In-memory storage for volumes (for a dummy driver)
    volumes: Arc<Mutex<HashMap<String, Volume>>>,
    client: Client,
}

impl ControllerService {
    pub async fn new(client: Client) -> Self {
        // Spawn the controller / resource reconciler
        tokio::spawn(crate::resource::controller::run(client.clone()));

        Self {
            client,
            volumes: Arc::new(Mutex::new(HashMap::new())),
        }
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

        {
            let volumes = self.volumes.lock().unwrap();
            // Check if volume already exists
            if volumes.contains_key(&request.name) {
                let existing_volume = volumes.get(&request.name).unwrap().clone();
                return Ok(Response::new(CreateVolumeResponse {
                    volume: Some(existing_volume),
                }));
            }
        }

        let namespace = request.parameters.get("resourceNamespace").ok_or_else(|| {
            tracing::error!("Resource namespace not provided in parameters");
            Status::invalid_argument("Resource namespace not provided in parameters")
        })?;

        let client = self.client.clone();
        let pvc_meta = get_pvc(client.clone(), namespace, &request.name).await?;
        let context = get_config_from_pvc_meta(client, pvc_meta.clone()).await?;

        tracing::trace!("PVC meta: {:?}", pvc_meta);
        tracing::trace!("Volume context: {:?}", context);

        tracing::info!(
            "Creating volume '{}' with annotations '{:?}'",
            request.name,
            pvc_meta.annotations.unwrap_or_default()
        );

        // Create a new volume entry
        let volume_id = format!("kubedal-{}", uuid::Uuid::new_v4());
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

        let mut volumes = self.volumes.lock().unwrap();
        volumes.insert(request.name, volume.clone());

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

        let mut volumes = self.volumes.lock().unwrap();

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
        let volumes = self.volumes.lock().unwrap();
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

async fn get_pvc(client: Client, namespace: &str, uid: &str) -> Result<ObjectMeta, Status> {
    let uid = uid.strip_prefix("pvc-").ok_or_else(|| {
        tracing::error!("PVC name not provided in request");
        Status::invalid_argument("PVC name not provided in request")
    })?;

    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client, namespace);

    let all_pvc = pvcs
        .list_metadata(&ListParams::default())
        .await
        .map_err(|e| {
            tracing::error!("Error listing PVCs: {:?}", e);
            Status::internal("Error listing PVCs")
        })?;

    let pvc = all_pvc
        .items
        .into_iter()
        .find(|pvc| {
            pvc.metadata
                .uid
                .as_ref()
                .is_some_and(|has_uid| has_uid == uid)
        })
        .ok_or_else(|| {
            tracing::error!("PVC not found");
            Status::not_found("PVC not found")
        })?;

    let pvc_meta = pvc.metadata;

    Ok(pvc_meta)
}

async fn get_config_from_pvc_meta(
    client: Client,
    pvc_meta: ObjectMeta,
) -> Result<HashMap<String, String>, Status> {
    let annotations = pvc_meta.annotations.unwrap_or_default();

    let resource = annotations
        .get("kubedal.arunaengine.org/resource")
        .ok_or_else(|| {
            tracing::error!("Resource annotation not found");
            Status::invalid_argument("Resource annotation not found")
        })?;

    let resource_ns = annotations
        .get("kubedal.arunaengine.org/namespace")
        .cloned()
        .unwrap_or(pvc_meta.namespace.clone().unwrap_or_default());

    let res_api: Api<Datasource> = Api::namespaced(client, &resource_ns);
    let res = res_api.get(resource).await.map_err(|e| {
        tracing::error!("Error getting resource: {:?}", e);
        Status::internal("Error getting resource")
    })?;

    let mut config = res.spec.config.clone();
    config.insert(
        "kubedal.arunaengine.org/resource".to_string(),
        resource.to_string(),
    );
    match res.spec.backend {
        crate::resource::crd::Backend::S3 => config.insert("backend".to_string(), "s3".to_string()),
        crate::resource::crd::Backend::HTTP => {
            config.insert("backend".to_string(), "http".to_string())
        }
    };

    // Override the mount mode if specified in the PVC annotations
    config.insert(
        "mount".to_string(),
        annotations
            .get("kubedal.arunaengine.org/mount")
            .and_then(|mode| match mode.as_str() {
                m @ ("cached" | "fuse") => Some(m),
                _ => None,
            })
            .unwrap_or(match res.spec.mount {
                crate::resource::crd::MountMode::Cached => "cached",
                crate::resource::crd::MountMode::Fuse => "fuse",
            })
            .to_string(),
    );

    if let Some(cred) = res.spec.credentials {
        let secret_ns = cred.secret_ref.namespace.unwrap_or(resource_ns);
        let secret_name = cred.secret_ref.name;
        config.insert("secret".to_string(), secret_name);
        config.insert("secret_ns".to_string(), secret_ns);
    }

    Ok(config)
}
