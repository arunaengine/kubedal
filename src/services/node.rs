use crate::csi::node_server::Node;
use crate::csi::volume_capability::AccessType;
use crate::csi::{
    NodeExpandVolumeRequest, NodeExpandVolumeResponse, NodeGetCapabilitiesRequest,
    NodeGetCapabilitiesResponse, NodeGetInfoRequest, NodeGetInfoResponse, NodePublishVolumeRequest,
    NodePublishVolumeResponse, NodeUnpublishVolumeRequest, NodeUnpublishVolumeResponse,
};
use crate::node::request::{self, ContextSource};
use crate::node::state::{DataNodeRef, PodRef};
use crate::node::volume::{PublishRequest, VolumeManager, VolumeSource};
use crate::resource::crd::DataPod;
use crate::util::error::kube_status;
use kube::{Api, Client};
use std::path::PathBuf;
use std::sync::Arc;
use tonic::{Request, Response, Status};

pub struct NodeService {
    client: Client,
    node_id: String,
    manager: Arc<VolumeManager>,
}

impl NodeService {
    pub fn new(client: Client, node_id: &str, manager: Arc<VolumeManager>) -> Self {
        Self {
            client,
            node_id: node_id.to_string(),
            manager,
        }
    }

    /// Turn the parsed context into a source, looking up the DataPod if needed.
    async fn resolve_source(
        &self,
        source: ContextSource,
        pod: &PodRef,
    ) -> Result<VolumeSource, Status> {
        let (name, namespace, node, mode, writable) = match source {
            ContextSource::Direct(source) => return Ok(source),
            ContextSource::DataPod {
                name,
                namespace,
                node,
                mode,
                writable,
            } => (name, namespace, node, mode, writable),
        };

        // A PVC may only name a DataPod in the namespace of the pod.
        if namespace != pod.namespace {
            return Err(Status::permission_denied(format!(
                "DataPod {namespace}/{name} is not in the namespace of pod {}/{}",
                pod.namespace, pod.name
            )));
        }
        let api: Api<DataPod> = Api::namespaced(self.client.clone(), &namespace);
        let data_pod = api
            .get(&name)
            .await
            .map_err(|e| kube_status(e, &format!("DataPod {namespace}/{name}")))?;
        let path = data_pod_path(&data_pod)?;
        let node = match data_pod.spec.data_node_ref {
            Some(node_ref) => DataNodeRef {
                namespace: node_ref.namespace.unwrap_or_else(|| namespace.clone()),
                name: node_ref.name,
            },
            None => node.ok_or_else(|| {
                Status::failed_precondition(format!(
                    "DataPod {namespace}/{name} has no DataNode yet"
                ))
            })?,
        };
        Ok(VolumeSource::Tree {
            node,
            path,
            mode,
            writable,
        })
    }
}

/// The folder of a DataPod inside its DataNode.
///
/// Without a path, the DataPod controller assigns "/<uid>". The same default is
/// used here, so a pod that starts before the controller ran never sees the root
/// of the DataNode.
fn data_pod_path(data_pod: &DataPod) -> Result<String, Status> {
    match data_pod.spec.path.as_deref() {
        Some(path) if !path.is_empty() && path != "/" => Ok(path.to_string()),
        _ => {
            let uid = data_pod.metadata.uid.as_deref().ok_or_else(|| {
                Status::unavailable("the DataPod has no path and no uid yet; try again later")
            })?;
            Ok(format!("/{uid}"))
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
        Ok(Response::new(NodeGetCapabilitiesResponse {
            capabilities: vec![],
        }))
    }

    async fn node_get_info(
        &self,
        _request: Request<NodeGetInfoRequest>,
    ) -> Result<Response<NodeGetInfoResponse>, Status> {
        Ok(Response::new(NodeGetInfoResponse {
            node_id: self.node_id.clone(),
            max_volumes_per_node: 0, // no limit
            accessible_topology: None,
        }))
    }

    #[tracing::instrument(skip(self, request), fields(volume_id))]
    async fn node_publish_volume(
        &self,
        request: Request<NodePublishVolumeRequest>,
    ) -> Result<Response<NodePublishVolumeResponse>, Status> {
        let request = request.into_inner();
        tracing::Span::current().record("volume_id", request.volume_id.as_str());
        if request.volume_id.is_empty() {
            return Err(Status::invalid_argument("Volume ID cannot be empty"));
        }
        if request.target_path.is_empty() {
            return Err(Status::invalid_argument("Target path cannot be empty"));
        }
        let capability = request
            .volume_capability
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("Volume capability is missing"))?;
        if !matches!(capability.access_type, Some(AccessType::Mount(_))) {
            return Err(Status::invalid_argument(
                "kubedal only supports file system volumes, not block volumes",
            ));
        }

        let pod = request::pod_from_context(&request.volume_context)?;
        let source = request::source_from_context(&request.volume_context, &pod)?;
        let source = self.resolve_source(source, &pod).await?;

        tracing::info!(
            "publishing volume {} for pod {}/{} at {}",
            request.volume_id,
            pod.namespace,
            pod.name,
            request.target_path
        );
        self.manager
            .publish_detached(PublishRequest {
                volume_id: request.volume_id,
                target_path: PathBuf::from(request.target_path),
                read_only: request.readonly,
                pod,
                source,
            })
            .await?;
        Ok(Response::new(NodePublishVolumeResponse {}))
    }

    #[tracing::instrument(skip(self, request), fields(volume_id))]
    async fn node_unpublish_volume(
        &self,
        request: Request<NodeUnpublishVolumeRequest>,
    ) -> Result<Response<NodeUnpublishVolumeResponse>, Status> {
        let request = request.into_inner();
        tracing::Span::current().record("volume_id", request.volume_id.as_str());
        if request.volume_id.is_empty() {
            return Err(Status::invalid_argument("Volume ID cannot be empty"));
        }
        if request.target_path.is_empty() {
            return Err(Status::invalid_argument("Target path cannot be empty"));
        }
        tracing::info!(
            "unpublishing volume {} from {}",
            request.volume_id,
            request.target_path
        );
        self.manager
            .unpublish_detached(&request.volume_id, &PathBuf::from(request.target_path))
            .await?;
        Ok(Response::new(NodeUnpublishVolumeResponse {}))
    }

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
        Err(Status::unimplemented("Volume expansion is not supported"))
    }

    async fn node_get_volume_stats(
        &self,
        _request: Request<crate::csi::NodeGetVolumeStatsRequest>,
    ) -> Result<Response<crate::csi::NodeGetVolumeStatsResponse>, Status> {
        Err(Status::unimplemented("NodeGetVolumeStats is not supported"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::crd::DataPodSpec;

    fn data_pod(path: Option<&str>, uid: Option<&str>) -> DataPod {
        let mut dp = DataPod::new(
            "dp",
            DataPodSpec {
                path: path.map(str::to_string),
                ..Default::default()
            },
        );
        dp.metadata.uid = uid.map(str::to_string);
        dp
    }

    #[test]
    fn data_pod_without_path_uses_its_uid() {
        assert_eq!(data_pod_path(&data_pod(None, Some("u1"))).unwrap(), "/u1");
        assert_eq!(
            data_pod_path(&data_pod(Some("/"), Some("u1"))).unwrap(),
            "/u1"
        );
        assert_eq!(
            data_pod_path(&data_pod(Some(""), Some("u1"))).unwrap(),
            "/u1"
        );
        assert_eq!(
            data_pod_path(&data_pod(Some("/data"), Some("u1"))).unwrap(),
            "/data"
        );
        assert_eq!(
            data_pod_path(&data_pod(None, None)).unwrap_err().code(),
            tonic::Code::Unavailable
        );
    }
}
