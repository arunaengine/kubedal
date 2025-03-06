use crate::csi::node_server::Node;
use crate::csi::{
    NodeExpandVolumeRequest, NodeExpandVolumeResponse, NodeGetCapabilitiesRequest,
    NodeGetCapabilitiesResponse, NodeGetInfoRequest, NodeGetInfoResponse, NodePublishVolumeRequest,
    NodePublishVolumeResponse, NodeServiceCapability, NodeUnpublishVolumeRequest,
    NodeUnpublishVolumeResponse,
};
use crate::resource::crd::{Backend, Resource};
use crate::util::opendal::get_operator;
use futures::TryStreamExt;
use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tonic::{Request, Response, Status};

pub struct NodeService {
    client: Client,
    node_id: String,
    // Track mounted volumes for our dummy driver
    mounts: Arc<Mutex<HashMap<String, String>>>,
}

impl NodeService {
    pub fn new(client: Client, node_id: &str) -> Self {
        Self {
            client,
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
            r#type: Some(crate::csi::node_service_capability::Type::Rpc(
                crate::csi::node_service_capability::Rpc {
                    r#type: crate::csi::node_service_capability::rpc::Type::StageUnstageVolume
                        .into(),
                },
            )),
        };

        let response = NodeGetCapabilitiesResponse {
            capabilities: vec![stage_unstage],
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

    #[tracing::instrument(skip(self, request))]
    async fn node_publish_volume(
        &self,
        request: Request<NodePublishVolumeRequest>,
    ) -> Result<Response<NodePublishVolumeResponse>, Status> {
        let request = request.into_inner();
        let volume_id = request.volume_id; // This is just the volume-handle 
        let target_path = request.target_path;

        if volume_id.is_empty() {
            return Err(Status::invalid_argument("Volume ID cannot be empty"));
        }

        if target_path.is_empty() {
            return Err(Status::invalid_argument("Target path cannot be empty"));
        }

        // Get the resource info from volume context
        let resource_name = request
            .volume_context
            .get("kubedal.arunaengine.org/resource")
            .cloned()
            .expect("No resource propagated");

        // Fetch resource
        let res_ns = match request
            .volume_context
            .get("kubedal.arunaengine.org/resource_ns")
        {
            Some(ns) => ns,
            None => "default",
        };
        let res_api: Api<Resource> = Api::namespaced(self.client.clone(), &res_ns);
        let res = res_api.get(&resource_name).await.map_err(|e| {
            tracing::error!("Error getting resource: {:?}", e);
            Status::internal("Error getting resource")
        })?;

        // Fetch secret
        let mut config = match res.spec.credentials.as_ref() {
            None => HashMap::new(),
            Some(cred) => {
                let secret_ns = cred.secret_ref.namespace.as_deref().unwrap_or("default");
                let secret_name = &cred.secret_ref.name;
                let secret_api: Api<Secret> = Api::namespaced(self.client.clone(), secret_ns);
                let secret = secret_api
                    .get(&secret_name)
                    .await
                    .map_err(|_| Status::not_found("Secret not found"))?;

                let mut config = HashMap::new();

                if let Some(data) = secret.data {
                    for (k, v) in data.into_iter() {
                        config.insert(
                            k,
                            std::str::from_utf8(&v.0)
                                .map_err(|e| {
                                    Status::internal(format!("Failed to deserialize secret: {}", e))
                                })?
                                .to_string(),
                        );
                    }
                }

                config
            }
        };

        // Create openDAL config and operator
        config.extend(res.spec.config);
        let operator = get_operator(Backend::S3, config.clone())?; //TODO: Match backend provided from resource

        // Read dataset into target path
        tracing::info!(
            "Publishing volume '{}' with resource '{}' to '{}'",
            volume_id,
            resource_name,
            target_path
        );

        //TODO: Match Resource mount type. Currently the data source is just mirrored into the volume.
        //let data_source_children = operator.list_with("").recursive(true).await.map_err(|e| {
        let data_source_children = operator
            .list("")
            .await
            .map_err(|e| Status::internal(format!("Data source listing failed: {}", e)))?;

        // Create the target directory if it doesn't exist
        let target_path_obj = Path::new(&target_path);
        if !target_path_obj.exists() {
            let mut builder = fs::DirBuilder::new();
            builder
                .mode(0o755)
                .recursive(true)
                .create(target_path_obj)
                .map_err(|e| {
                    Status::internal(format!("Failed to create target directory: {}", e))
                })?;
        }

        // Cache data source in target directory
        //TODO: More sophisticated directory structure to cache for resource/resource-version
        for entry in data_source_children {
            let entry_path = Path::new(&target_path).join(entry.path());
            match entry.metadata().mode() {
                opendal::EntryMode::FILE => {
                    // Create file
                    let mut file = std::fs::File::create(entry_path)?;
                    // Create stream
                    let mut r = operator
                        .reader(entry.path())
                        .await
                        .map_err(|e| {
                            Status::internal(format!("Failed to open file reader: {}", e))
                        })?
                        .into_bytes_stream(..)
                        .await
                        .map_err(|e| {
                            Status::internal(format!("Failed to convert reader into stream: {}", e))
                        })?;
                    // Write stream into file
                    while let Some(bytes) = r.try_next().await? {
                        file.write_all(&bytes)?;
                    }
                }
                opendal::EntryMode::DIR => {
                    if !entry_path.exists() {
                        let mut builder = fs::DirBuilder::new();
                        builder
                            .mode(0o755)
                            .recursive(true)
                            .create(entry_path)
                            .map_err(|e| {
                                Status::internal(format!(
                                    "Failed to create target directory: {}",
                                    e
                                ))
                            })?;
                    }
                }
                opendal::EntryMode::Unknown => {
                    return Err(Status::unknown("Data source entry type is unknown"));
                }
            }
        }

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

        tracing::info!("Unpublishing volume '{}' from '{}'", volume_id, target_path);

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
        _request: Request<NodeExpandVolumeRequest>,
    ) -> Result<Response<NodeExpandVolumeResponse>, Status> {
        // For our dummy driver, we don't support volume expansion
        Err(Status::unimplemented("Volume expansion is not supported"))
    }

    // Additional required methods
    async fn node_get_volume_stats(
        &self,
        request: Request<crate::csi::NodeGetVolumeStatsRequest>,
    ) -> Result<Response<crate::csi::NodeGetVolumeStatsResponse>, Status> {
        let request = request.into_inner();
        let volume_id = request.volume_id;
        let volume_path = request.volume_path;

        tracing::info!(
            "Getting stats for volume '{}' at '{}'",
            volume_id,
            volume_path
        );

        // For our dummy driver, return fixed values
        let available = 1024 * 1024 * 1024 * 10; // 10 GB
        let total = 1024 * 1024 * 1024 * 20; // 20 GB
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
            volume_condition: None,
        }))
    }
}
