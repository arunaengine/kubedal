use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tonic::{Request, Response, Status};

use crate::csi::node_server::Node;
use crate::csi::{
    NodeExpandVolumeRequest, NodeExpandVolumeResponse, NodeGetCapabilitiesRequest, NodeGetCapabilitiesResponse, NodeGetInfoRequest, NodeGetInfoResponse, NodePublishVolumeRequest, NodePublishVolumeResponse, NodeServiceCapability, NodeUnpublishVolumeRequest, NodeUnpublishVolumeResponse
};

#[derive(Debug)]
pub struct NodeService {
    node_id: String,
    // Track mounted volumes for our dummy driver
    mounts: Arc<Mutex<HashMap<String, String>>>,
}

impl NodeService {
    pub fn new(node_id: &str) -> Self {
        Self {
            node_id: node_id.to_string(),
            mounts: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[tonic::async_trait]
impl Node for NodeService {
    async fn node_get_capabilities(
        &self,
        _request: Request<NodeGetCapabilitiesRequest>,
    ) -> Result<Response<NodeGetCapabilitiesResponse>, Status> {
        // Advertise stage_unstage_volume capability
        let stage_unstage = NodeServiceCapability {
            r#type: Some(
                crate::csi::node_service_capability::Type::Rpc(
                    crate::csi::node_service_capability::Rpc {
                        r#type: crate::csi::node_service_capability::rpc::Type::StageUnstageVolume.into(),
                    }
                )
            ),
        };

        let expand_volume = NodeServiceCapability {
            r#type: Some(
                crate::csi::node_service_capability::Type::Rpc(
                    crate::csi::node_service_capability::Rpc {
                        r#type: crate::csi::node_service_capability::rpc::Type::ExpandVolume.into(),
                    }
                )
            ),
        };

        let response = NodeGetCapabilitiesResponse {
            capabilities: vec![stage_unstage, expand_volume],
        };

        Ok(Response::new(response))
    }

    async fn node_get_info(
        &self,
        _request: Request<NodeGetInfoRequest>,
    ) -> Result<Response<NodeGetInfoResponse>, Status> {
        // Return basic node information
        let response = NodeGetInfoResponse {
            node_id: self.node_id.clone(),
            max_volumes_per_node: 1000, // Arbitrary limit for the dummy driver
            accessible_topology: None,
        };

        Ok(Response::new(response))
    }

    async fn node_publish_volume(
        &self,
        request: Request<NodePublishVolumeRequest>,
    ) -> Result<Response<NodePublishVolumeResponse>, Status> {
        let request = request.into_inner();
        let volume_id = request.volume_id;
        let target_path = request.target_path;

        if volume_id.is_empty() {
            return Err(Status::invalid_argument("Volume ID cannot be empty"));
        }

        if target_path.is_empty() {
            return Err(Status::invalid_argument("Target path cannot be empty"));
        }

        // Get the resource info from volume context
        let resource_name = request.volume_context.get("resource")
            .cloned()
            .unwrap_or_default();

        log::info!(
            "Publishing volume '{}' with resource '{}' to '{}'",
            volume_id, resource_name, target_path
        );

        // Create the target directory if it doesn't exist
        let target_path_obj = Path::new(&target_path);
        if !target_path_obj.exists() {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o755).recursive(true).create(target_path_obj)
                .map_err(|e| Status::internal(format!("Failed to create target directory: {}", e)))?;
        }

        // In a real driver, we would:
        // 1. Either download data from the resource to the target path
        // 2. Or set up a FUSE mount at the target path

        // For our dummy driver, we'll just create a dummy file
        let dummy_file_path = Path::new(&target_path).join("dummy_data.txt");
        fs::write(&dummy_file_path, format!("Dummy content for resource: {}", resource_name))
            .map_err(|e| Status::internal(format!("Failed to write dummy file: {}", e)))?;

        // Track this mount in our in-memory state
        let mut mounts = self.mounts.lock().unwrap();
        mounts.insert(volume_id, target_path);

        Ok(Response::new(NodePublishVolumeResponse {}))
    }

    async fn node_unpublish_volume(
        &self,
        request: Request<NodeUnpublishVolumeRequest>,
    ) -> Result<Response<NodeUnpublishVolumeResponse>, Status> {
        let request = request.into_inner();
        let volume_id = request.volume_id;
        let target_path = request.target_path;

        if volume_id.is_empty() {
            return Err(Status::invalid_argument("Volume ID cannot be empty"));
        }

        if target_path.is_empty() {
            return Err(Status::invalid_argument("Target path cannot be empty"));
        }

        log::info!("Unpublishing volume '{}' from '{}'", volume_id, target_path);

        // In a real driver, we would:
        // 1. Unmount any FUSE mount
        // 2. Clean up any cached data if necessary

        // For our dummy driver, we'll just remove our dummy file
        let dummy_file_path = Path::new(&target_path).join("dummy_data.txt");
        if dummy_file_path.exists() {
            fs::remove_file(&dummy_file_path)
                .map_err(|e| Status::internal(format!("Failed to remove dummy file: {}", e)))?;
        }

        // Remove from our tracking
        let mut mounts = self.mounts.lock().unwrap();
        mounts.remove(&volume_id);

        Ok(Response::new(NodeUnpublishVolumeResponse {}))
    }

    async fn node_stage_volume(
        &self,
        _request: Request<crate::csi::NodeStageVolumeRequest>,
    ) -> Result<Response<crate::csi::NodeStageVolumeResponse>, Status> {
        // For our dummy driver, we don't need to do anything here
        Ok(Response::new(crate::csi::NodeStageVolumeResponse {}))
    }

    async fn node_unstage_volume(
        &self,
        _request: Request<crate::csi::NodeUnstageVolumeRequest>,
    ) -> Result<Response<crate::csi::NodeUnstageVolumeResponse>, Status> {
        // For our dummy driver, we don't need to do anything here
        Ok(Response::new(crate::csi::NodeUnstageVolumeResponse {}))
    }

    async fn node_expand_volume(
        &self,
        request: Request<NodeExpandVolumeRequest>,
    ) -> Result<Response<NodeExpandVolumeResponse>, Status> {
        let request = request.into_inner();
        let volume_id = request.volume_id;
        let volume_path = request.volume_path;
        
        log::info!("Expanding volume '{}' at '{}'", volume_id, volume_path);
        
        // In a real driver with size limits, we would update them here
        // For our dummy driver, we'll just acknowledge the expansion

        Ok(Response::new(NodeExpandVolumeResponse {
            capacity_bytes: request.capacity_range.map_or(0, |range| range.required_bytes),
        }))
    }

    // Additional required methods
    async fn node_get_volume_stats(
        &self,
        request: Request<crate::csi::NodeGetVolumeStatsRequest>,
    ) -> Result<Response<crate::csi::NodeGetVolumeStatsResponse>, Status> {
        let request = request.into_inner();
        let volume_id = request.volume_id;
        let volume_path = request.volume_path;
        
        log::info!("Getting stats for volume '{}' at '{}'", volume_id, volume_path);
        
        // For our dummy driver, return fixed values
        let available = 1024 * 1024 * 1024 * 10; // 10 GB
        let total = 1024 * 1024 * 1024 * 20;     // 20 GB
        let used = total - available;

        let usage = vec![
            crate::csi::VolumeUsage {
                available,
                total,
                used,
                unit: crate::csi::volume_usage::Unit::Bytes.into(),
            },
            crate::csi::VolumeUsage {
                available: 1000,
                total: 1000,
                used: 0,
                unit: crate::csi::volume_usage::Unit::Inodes.into(),
            },
        ];

        Ok(Response::new(crate::csi::NodeGetVolumeStatsResponse {
            usage,
            volume_condition: None
        }))
    }
}