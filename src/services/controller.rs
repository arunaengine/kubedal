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

#[derive(Debug)]
pub struct ControllerService {
    // In-memory storage for volumes (for a dummy driver)
    volumes: Arc<Mutex<HashMap<String, Volume>>>,
}

impl ControllerService {
    pub fn new() -> Self {
        Self {
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

    #[tracing::instrument(skip(self))]
    async fn create_volume(
        &self,
        request: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeResponse>, Status> {
        let request = request.into_inner();
        let volume_name = request.name;

        if volume_name.is_empty() {
            return Err(Status::invalid_argument("Volume name cannot be empty"));
        }

        // Check for resource annotation in parameters
        let parameters = request.parameters;
        let resource_name = parameters.get("resource").cloned().unwrap_or_default();

        tracing::info!(
            "Creating volume '{}' with resource '{}'",
            volume_name,
            resource_name
        );

        // In a real driver, we would validate and process the resource reference here
        // For our dummy driver, we'll just create a simple volume record

        let mut volumes = self.volumes.lock().unwrap();

        // Check if volume already exists
        if volumes.contains_key(&volume_name) {
            let existing_volume = volumes.get(&volume_name).unwrap().clone();
            return Ok(Response::new(CreateVolumeResponse {
                volume: Some(existing_volume),
            }));
        }

        // Create a new volume entry
        let volume_id = format!("kubedal-{}", uuid::Uuid::new_v4());
        let capacity_bytes = request
            .capacity_range
            .map_or(5 * 1024 * 1024 * 1024, |range| range.required_bytes);

        let mut context = HashMap::new();
        context.insert("resource".to_string(), resource_name);

        let volume = Volume {
            volume_id: volume_id.clone(),
            capacity_bytes,
            content_source: None,
            accessible_topology: vec![],
            volume_context: context,
        };

        volumes.insert(volume_name, volume.clone());

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
