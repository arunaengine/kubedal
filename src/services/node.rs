use crate::csi::node_server::Node;
use crate::csi::{
    NodeExpandVolumeRequest, NodeExpandVolumeResponse, NodeGetCapabilitiesRequest,
    NodeGetCapabilitiesResponse, NodeGetInfoRequest, NodeGetInfoResponse, NodePublishVolumeRequest,
    NodePublishVolumeResponse, NodeUnpublishVolumeRequest, NodeUnpublishVolumeResponse,
};
use crate::resource::crd::{DataNode, DataPod, MountAccess};
use crate::util::error::{io_status, kube_status};
use crate::util::mount_helper::{AccessMode, Mount, MountMode};
use crate::util::mountinfo::is_mount_point;
use crate::util::opendal::get_operator;
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::Secret;
use kube::api::PostParams;
use kube::{Api, Client};
use opendal::Operator;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};

pub struct FullDataSource {
    pub source: DataNode,
    pub pod: DataPod,
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
    /// Active mounts, keyed by target path.
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
        // kubedal does all work in NodePublishVolume, so no optional node RPCs are offered.
        let response = NodeGetCapabilitiesResponse {
            capabilities: vec![],
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

        // The CSI spec requires NodePublishVolume to be idempotent.
        let mut mounts = self.mounts.lock().await;
        if let Some(existing) = mounts.get(&target_path) {
            if existing.volume_id == volume_id {
                return Ok(Response::new(NodePublishVolumeResponse {}));
            }
            return Err(Status::already_exists(format!(
                "target path {target_path} is already used by volume {}",
                existing.volume_id
            )));
        }

        let (full_data_source, mount_access) =
            get_full_data_mount(self.client.clone(), &request.volume_context).await?;
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

        let (operator, mount_mode, access_mode) = full_data_source
            .into_parts(self.client.clone(), mount_access)
            .await?;

        let mut mount = Mount::new(
            volume_id.clone(),
            target_path.clone(),
            operator,
            mount_mode,
            access_mode,
        );
        mount.mount().await?;

        mounts.insert(target_path, mount);

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

        let mut mounts = self.mounts.lock().await;
        if let Some(mut mount) = mounts.remove(&target_path) {
            if let Err(err) = mount.unmount().await {
                // Keep tracking the mount so that a retry can finish the work.
                mounts.insert(target_path, mount);
                return Err(err);
            }
        } else if is_mount_point(std::path::Path::new(&target_path)) {
            // Unknown to this process (for example after a restart), but still mounted.
            unmount_path(&target_path).await?;
        }

        // The spec asks the plugin to remove the target path. A missing path is fine.
        match tokio::fs::remove_dir(&target_path).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_status(err, "failed to remove target path")),
        }

        Ok(Response::new(NodeUnpublishVolumeResponse {}))
    }

    // Not implemented methods (optional)
    async fn node_stage_volume(
        &self,
        _request: Request<crate::csi::NodeStageVolumeRequest>,
    ) -> Result<Response<crate::csi::NodeStageVolumeResponse>, Status> {
        Err(Status::unimplemented("NodeStageVolume is not supported"))
    }

    async fn node_unstage_volume(
        &self,
        _request: Request<crate::csi::NodeUnstageVolumeRequest>,
    ) -> Result<Response<crate::csi::NodeUnstageVolumeResponse>, Status> {
        Err(Status::unimplemented("NodeUnstageVolume is not supported"))
    }

    async fn node_expand_volume(
        &self,
        _request: Request<NodeExpandVolumeRequest>,
    ) -> Result<Response<NodeExpandVolumeResponse>, Status> {
        // Our simple driver doesn't support this
        Err(Status::unimplemented("Volume expansion is not supported"))
    }

    // Additional required methods
    async fn node_get_volume_stats(
        &self,
        _request: Request<crate::csi::NodeGetVolumeStatsRequest>,
    ) -> Result<Response<crate::csi::NodeGetVolumeStatsResponse>, Status> {
        // Our simple driver doesn't support this
        Err(Status::unimplemented("NodeGetVolumeStats is not supported"))
    }
}

#[tracing::instrument]
fn get_pod_info(volume_context: &HashMap<String, String>) -> Result<PodInfo, Status> {
    // Fetch Pod meta from context and check with SubjectAccessReview
    let get = |key: &str| {
        volume_context.get(key).cloned().ok_or_else(|| {
            Status::invalid_argument(format!(
                "volume context is missing {key}; is podInfoOnMount enabled in the CSIDriver?"
            ))
        })
    };

    Ok(PodInfo {
        name: get("csi.storage.k8s.io/pod.name")?,
        namespace: get("csi.storage.k8s.io/pod.namespace")?,
        service_account: get("csi.storage.k8s.io/serviceAccount.name")?,
        uid: get("csi.storage.k8s.io/pod.uid")?,
    })
}

async fn get_full_data_mount(
    client: Client,
    volume_context: &HashMap<String, String>,
) -> Result<(FullDataSource, MountAccess), Status> {
    // Get DataSource meta from volume context
    let data_node_name = volume_context
        .get("kubedal.arunaengine.org/data-node-name")
        .cloned()
        .ok_or_else(|| Status::invalid_argument("DataNode name not provided"))?;

    let data_node_namespace = volume_context
        .get("kubedal.arunaengine.org/data-node-namespace")
        .cloned()
        .ok_or_else(|| Status::invalid_argument("DataNode namespace not provided"))?;

    let data_pod_name = volume_context
        .get("kubedal.arunaengine.org/data-pod-name")
        .cloned()
        .ok_or_else(|| Status::invalid_argument("DataPod name not provided"))?;

    let data_pod_namespace = volume_context
        .get("kubedal.arunaengine.org/data-pod-namespace")
        .cloned()
        .ok_or_else(|| Status::invalid_argument("DataPod namespace not provided"))?;

    let mount_mode = match volume_context
        .get("kubedal.arunaengine.org/mount")
        .ok_or_else(|| Status::invalid_argument("Mount mode not provided"))?
        .as_str()
    {
        "cache" => MountAccess::CacheReadOnly,
        "cache-read-only" => MountAccess::CacheReadOnly,
        "cache-read-write" => MountAccess::CacheReadWrite,
        "fuse" => MountAccess::FuseReadOnly,
        "fuse-read-only" => MountAccess::FuseReadOnly,
        "fuse-read-write" => MountAccess::FuseReadWrite,
        _ => return Err(Status::invalid_argument("Unsupported mount type")),
    };

    // Fetch DataNode, DataPod, [optional] Secret
    let data_node_api: Api<DataNode> = Api::namespaced(client.clone(), &data_node_namespace);
    let data_node = data_node_api.get(&data_node_name).await.map_err(|e| {
        kube_status(
            e,
            &format!("DataNode {data_node_namespace}/{data_node_name}"),
        )
    })?;

    let data_pod_api: Api<DataPod> = Api::namespaced(client.clone(), &data_pod_namespace);
    let data_pod = data_pod_api
        .get(&data_pod_name)
        .await
        .map_err(|e| kube_status(e, &format!("DataPod {data_pod_namespace}/{data_pod_name}")))?;

    let secret = match data_node.spec.secret_ref.as_ref() {
        None => None,
        Some(secret_ref) => {
            let secret_name = &secret_ref.name;
            let secret_ns = secret_ref
                .namespace
                .as_ref()
                .or(data_node.metadata.namespace.as_ref())
                .ok_or_else(|| Status::invalid_argument("Secret namespace not provided"))?;
            let secret_api: Api<Secret> = Api::namespaced(client.clone(), secret_ns);
            let secret = secret_api
                .get(secret_name)
                .await
                .map_err(|e| kube_status(e, &format!("Secret {secret_ns}/{secret_name}")))?;
            Some(secret)
        }
    };

    Ok((
        FullDataSource {
            source: data_node,
            pod: data_pod,
            secret,
        },
        mount_mode,
    ))
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
                        resource: Some("datanodes".to_string()),
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
        .map_err(|e| kube_status(e, "SubjectAccessReview"))?;
    tracing::info!("SubjectAccessReview response: {:#?}", &response);

    if let Some(status) = response.status {
        if !status.allowed {
            return Err(Status::permission_denied(format!(
                "system:serviceaccount:{}:{}, has unsufficient permission to GET datanode.kubedal.arunaengine.org {:?} in namespace {:?}",
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
            .map_err(|e| kube_status(e, "SubjectAccessReview"))?;
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

    Ok(())
}

impl FullDataSource {
    pub async fn into_parts(
        self,
        client: Client,
        mount_mode: MountAccess,
    ) -> Result<(Operator, MountMode, AccessMode), Status> {
        let (mode, access) = match mount_mode {
            MountAccess::CacheReadWrite => (MountMode::Cached, AccessMode::ReadWrite),
            MountAccess::CacheReadOnly => (MountMode::Cached, AccessMode::ReadOnly),
            MountAccess::FuseReadWrite => (MountMode::Fuse, AccessMode::ReadWrite),
            MountAccess::FuseReadOnly => (MountMode::Fuse, AccessMode::ReadOnly),
        };

        Ok((get_operator(&client, &self.source).await?, mode, access))
    }
}

/// Unmount a path that kubedal does not track in memory.
async fn unmount_path(target_path: &str) -> Result<(), Status> {
    let path = target_path.to_string();
    tokio::task::spawn_blocking(move || {
        sys_mount::unmount(&path, sys_mount::UnmountFlags::DETACH)
            .map_err(|e| io_status(e, "failed to unmount target path"))
    })
    .await
    .map_err(|e| Status::internal(format!("unmount task failed: {e}")))?
}
