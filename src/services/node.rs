use crate::csi::node_server::Node;
use crate::csi::{
    NodeExpandVolumeRequest, NodeExpandVolumeResponse, NodeGetCapabilitiesRequest,
    NodeGetCapabilitiesResponse, NodeGetInfoRequest, NodeGetInfoResponse, NodePublishVolumeRequest,
    NodePublishVolumeResponse, NodeServiceCapability, NodeUnpublishVolumeRequest,
    NodeUnpublishVolumeResponse,
};
use crate::resource::crd::{AccessMode, Datasource, MountMode};
use crate::util::mount_helper::Mount;
use crate::util::opendal::get_operator;
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::Secret;
use kube::api::PostParams;
use kube::{Api, Client};
use opendal::Operator;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tonic::{Request, Response, Status};

pub struct FullDataSource {
    pub source: Datasource,
    pub secret: Option<Secret>,
}

pub struct PodInfo {
    pub name: String,
    pub namespace: String,
    pub service_account: String,
    pub uid: String,
}

pub struct NodeService {
    client: Client,
    node_id: String,
    // Track mounted volumes for our dummy driver
    mounts: Arc<Mutex<HashMap<String, Mount>>>,
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

        let full_data_source =
            get_full_data_source(self.client.clone(), &request.volume_context).await?;
        let pod_info = get_pod_info(&request.volume_context)?;
        // Checks if the pods service account has access to the data source / secret
        check_access(self.client.clone(), &full_data_source, &pod_info).await?;

        // Read dataset into target path
        tracing::info!(
            "Publishing volume '{}' with data source '{}' to '{}'",
            volume_id,
            full_data_source
                .source
                .metadata
                .name
                .clone()
                .unwrap_or_default(),
            target_path
        );

        let (operator, mount_mode, access_mode) = full_data_source.into_parts()?;

        let mut mount = Mount::new(target_path, operator, mount_mode, access_mode);
        mount.mount().await?;

        // Track this mount in our in-memory state
        let mut mounts = self.mounts.lock().unwrap();
        mounts.insert(volume_id, mount);

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

#[tracing::instrument]
fn get_pod_info(volume_context: &HashMap<String, String>) -> Result<PodInfo, Status> {
    // Fetch Pod meta from context and check with SubjectAccessReview
    let pod_name = volume_context
        .get("csi.storage.k8s.io/pod.name")
        .expect("Pod name not provided");
    let pod_namespace = volume_context
        .get("csi.storage.k8s.io/pod.namespace")
        .expect("Pod namespace not provided");
    let pod_service_account = volume_context
        .get("csi.storage.k8s.io/serviceAccount.name")
        .expect("Pod service account not provided");

    let pod_uid = volume_context
        .get("csi.storage.k8s.io/pod.uid")
        .expect("Pod UID not provided");

    Ok(PodInfo {
        name: pod_name.clone(),
        namespace: pod_namespace.clone(),
        service_account: pod_service_account.clone(),
        uid: pod_uid.clone(),
    })
}

async fn get_full_data_source(
    client: Client,
    volume_context: &HashMap<String, String>,
) -> Result<FullDataSource, Status> {
    // Get DataSource meta from volume context
    let datasource_name = volume_context
        .get("kubedal.arunaengine.org/resource")
        .cloned()
        .ok_or_else(|| Status::invalid_argument("Resource name not provided"))?;

    let datasource_namespace = volume_context
        .get("kubedal.arunaengine.org/resource_namespace")
        .cloned()
        .ok_or_else(|| Status::invalid_argument("Resource namespace not provided"))?;

    // Fetch DataSource if access review success
    let res_api: Api<Datasource> = Api::namespaced(client.clone(), &datasource_namespace);
    let res = res_api.get(&datasource_name).await.map_err(|e| {
        tracing::error!("Error getting DataSource: {:?}", e);
        Status::internal("Error getting DataSource")
    })?;

    let secret = match res.spec.credentials.as_ref() {
        None => None,
        Some(cred) => {
            let secret_name = &cred.secret_ref.name;
            let secret_ns = cred
                .secret_ref
                .namespace
                .as_ref()
                .or(res.metadata.namespace.as_ref())
                .ok_or_else(|| Status::invalid_argument("Secret namespace not provided"))?;
            let secret_api: Api<Secret> = Api::namespaced(client.clone(), &secret_ns);
            let secret = secret_api
                .get(secret_name)
                .await
                .map_err(|_| Status::not_found("Secret not found"))?;
            Some(secret)
        }
    };

    Ok(FullDataSource {
        source: res,
        secret,
    })
}

#[tracing::instrument(skip(client, full_data_source, pod_info))]
async fn check_access(
    client: Client,
    full_data_source: &FullDataSource,
    pod_info: &PodInfo,
) -> Result<(), Status> {
    let auth_api: Api<SubjectAccessReview> = Api::all(client.clone());
    let response = auth_api
        .create(
            &PostParams::default(),
            &SubjectAccessReview {
                metadata: Default::default(),
                spec: SubjectAccessReviewSpec {
                    resource_attributes: Some(ResourceAttributes {
                        group: Some("kubedal.arunaengine.org".to_string()),
                        name: full_data_source.source.metadata.name.clone(),
                        namespace: full_data_source.source.metadata.namespace.clone(),
                        resource: Some("datasources".to_string()),
                        verb: Some("get".to_string()),
                        ..Default::default()
                    }),
                    user: Some(format!(
                        "system:serviceaccount:{}:{}",
                        pod_info.namespace, pod_info.service_account
                    )),
                    ..Default::default()
                },
                status: None,
            },
        )
        .await
        .map_err(|_| Status::internal("SubjectAccessReview failed"))?;
    tracing::info!("SubjectAccessReview response: {:#?}", &response);

    if let Some(status) = response.status {
        if !status.allowed {
            return Err(Status::permission_denied(format!(
                "system:serviceaccount:{}:{}, has unsufficient permission to GET datasource.kubedal.arunaengine.org {:?} in namespace {:?}",
                pod_info.namespace,
                pod_info.service_account,
                full_data_source
                    .source
                    .metadata
                    .name
                    .as_ref()
                    .unwrap_or(&"".to_string()),
                full_data_source
                    .source
                    .metadata
                    .namespace
                    .as_ref()
                    .unwrap_or(&"".to_string())
            )));
        }
    } else {
        return Err(Status::permission_denied(
            "SubjectAccessReview did not return status",
        ));
    }

    if let Some(secret) = full_data_source.secret.as_ref() {
        let response = auth_api
            .create(
                &PostParams::default(),
                &SubjectAccessReview {
                    metadata: Default::default(),
                    spec: SubjectAccessReviewSpec {
                        resource_attributes: Some(ResourceAttributes {
                            name: secret.metadata.name.clone(),
                            namespace: secret.metadata.namespace.clone(),
                            resource: Some("secrets".to_string()),
                            verb: Some("get".to_string()),
                            ..Default::default()
                        }),
                        user: Some(format!(
                            "system:serviceaccount:{}:{}",
                            pod_info.namespace, pod_info.service_account
                        )),
                        ..Default::default()
                    },
                    status: None,
                },
            )
            .await
            .map_err(|_| Status::internal("SubjectAccessReview failed"))?;
        tracing::info!("SubjectAccessReview response: {:#?}", &response);

        if let Some(status) = response.status {
            if !status.allowed {
                return Err(Status::permission_denied(format!(
                    "system:serviceaccount:{}:{}, has unsufficient permission to GET secret.kubernetes.io/secret {:?} in namespace {:?}",
                    pod_info.namespace,
                    pod_info.service_account,
                    secret.metadata.name.as_ref().unwrap_or(&"".to_string()),
                    secret
                        .metadata
                        .namespace
                        .as_ref()
                        .unwrap_or(&"".to_string())
                )));
            }
        } else {
            return Err(Status::permission_denied(
                "SubjectAccessReview did not return status",
            ));
        }
    }

    return Ok(());
}

impl FullDataSource {
    pub fn into_parts(self) -> Result<(Operator, MountMode, AccessMode), Status> {
        let mut config = HashMap::from_iter(self.source.spec.config.into_iter());

        if let Some(secret) = self.secret {
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
        }
        Ok((
            get_operator(self.source.spec.backend, config)?,
            self.source.spec.mount,
            self.source.spec.access_mode,
        ))
    }
}
