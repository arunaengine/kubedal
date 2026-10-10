//! [`Cluster`] implementation that talks to the Kubernetes API.

use crate::node::cluster::{Cluster, EventKind, ResolvedNode};
use crate::node::state::{DataNodeRef, PodRef};
use crate::resource::crd::DataNode;
use crate::util::error::kube_status;
use crate::util::opendal::{build_operator, data_node_config};
use crate::util::quantity;
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::ObjectReference;
use kube::api::PostParams;
use kube::runtime::events::{Event, EventType, Recorder, Reporter};
use kube::{Api, Client};
use std::sync::Arc;
use tonic::Status;

pub struct KubeCluster {
    client: Client,
    recorder: Recorder,
}

impl KubeCluster {
    pub fn new(client: Client, node_id: &str) -> Self {
        let reporter = Reporter {
            controller: "kubedal.arunaengine.org".into(),
            instance: Some(format!("kubedal-node-{node_id}")),
        };
        Self {
            recorder: Recorder::new(client.clone(), reporter),
            client,
        }
    }

    /// A pod may always use DataNodes in its own namespace, like it may mount any
    /// secret in its own namespace. For a DataNode in another namespace, the service
    /// account of the pod needs RBAC permission to `get` that DataNode.
    async fn check_access(&self, node: &DataNodeRef, pod: &PodRef) -> Result<(), Status> {
        if node.namespace == pod.namespace {
            return Ok(());
        }
        let user = format!(
            "system:serviceaccount:{}:{}",
            pod.namespace, pod.service_account
        );
        let review = SubjectAccessReview {
            metadata: Default::default(),
            spec: SubjectAccessReviewSpec {
                resource_attributes: Some(ResourceAttributes {
                    group: Some("kubedal.arunaengine.org".into()),
                    resource: Some("datanodes".into()),
                    verb: Some("get".into()),
                    name: Some(node.name.clone()),
                    namespace: Some(node.namespace.clone()),
                    ..Default::default()
                }),
                user: Some(user.clone()),
                groups: Some(vec![
                    "system:serviceaccounts".into(),
                    format!("system:serviceaccounts:{}", pod.namespace),
                ]),
                ..Default::default()
            },
            status: None,
        };
        let api: Api<SubjectAccessReview> = Api::all(self.client.clone());
        let response = api
            .create(&PostParams::default(), &review)
            .await
            .map_err(|e| kube_status(e, "SubjectAccessReview"))?;
        match response.status {
            Some(status) if status.allowed => Ok(()),
            _ => Err(Status::permission_denied(format!(
                "{user} may not use DataNode {node}: it is in another namespace and \
                 the service account has no permission to get it"
            ))),
        }
    }
}

#[tonic::async_trait]
impl Cluster for KubeCluster {
    async fn resolve(&self, node: &DataNodeRef, pod: &PodRef) -> Result<ResolvedNode, Status> {
        self.check_access(node, pod).await?;

        let api: Api<DataNode> = Api::namespaced(self.client.clone(), &node.namespace);
        let data_node = api
            .get(&node.name)
            .await
            .map_err(|e| kube_status(e, &format!("DataNode {node}")))?;

        let limit = match data_node.spec.limit.as_ref() {
            Some(q) => Some(quantity::parse_bytes(&q.0).map_err(|e| {
                Status::invalid_argument(format!("invalid limit of DataNode {node}: {e}"))
            })?),
            None => None,
        };

        let cfg = data_node_config(&self.client, &data_node).await?;
        let backend = data_node.spec.backend.clone();
        // The identity leaves out secret values, so rotated credentials keep it.
        let identity = cache_scope(
            node,
            data_node.metadata.uid.as_deref().unwrap_or_default(),
            &data_node.spec.backend,
            &data_node.spec.config,
        );
        let scope = cache_scope(
            node,
            data_node.metadata.uid.as_deref().unwrap_or_default(),
            &backend,
            &cfg,
        );
        // Check the configuration once, so errors show up at publish time.
        build_operator(&backend, cfg.clone(), "")?;
        let factory = Arc::new(move |root: &str| build_operator(&backend, cfg.clone(), root));

        Ok(
            ResolvedNode::new(node.clone(), limit, data_node.spec.read_only, factory)
                .with_cache_scope(scope)
                .with_identity(identity),
        )
    }

    async fn pod_event(&self, pod: &PodRef, kind: EventKind, reason: &str, message: &str) {
        let reference = ObjectReference {
            api_version: Some("v1".into()),
            kind: Some("Pod".into()),
            name: Some(pod.name.clone()),
            namespace: Some(pod.namespace.clone()),
            uid: Some(pod.uid.clone()),
            ..Default::default()
        };
        let event = Event {
            type_: match kind {
                EventKind::Normal => EventType::Normal,
                EventKind::Warning => EventType::Warning,
            },
            reason: reason.into(),
            note: Some(message.chars().take(1000).collect()),
            action: "Volume".into(),
            secondary: None,
        };
        if let Err(err) = self.recorder.publish(&event, &reference).await {
            tracing::warn!(
                "failed to record event on pod {}/{}: {err}",
                pod.namespace,
                pod.name
            );
        }
    }
}

/// Cache scope of a DataNode: its name, its UID and a hash of its backend and
/// configuration. A re-created DataNode (new UID) or a changed configuration
/// never sees content cached for the old one.
fn cache_scope(
    node: &DataNodeRef,
    uid: &str,
    backend: &crate::resource::crd::Backend,
    cfg: &std::collections::HashMap<String, String>,
) -> String {
    use sha2::{Digest, Sha256};
    let mut entries: Vec<_> = cfg.iter().collect();
    entries.sort();
    let mut hasher = Sha256::new();
    hasher.update(format!("{backend:?}").as_bytes());
    for (key, value) in entries {
        hasher.update([0u8]);
        hasher.update(key.as_bytes());
        hasher.update([0u8]);
        hasher.update(value.as_bytes());
    }
    let config_hash = crate::node::paths::to_hex(&hasher.finalize());
    format!("{node}@{uid}#{}", &config_hash[..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::crd::Backend;
    use std::collections::HashMap;

    #[test]
    fn cache_scope_changes_with_uid_and_config() {
        let node = DataNodeRef {
            name: "store".into(),
            namespace: "team".into(),
        };
        let cfg = HashMap::from([("bucket".to_string(), "a".to_string())]);
        let base = cache_scope(&node, "uid-1", &Backend::S3, &cfg);
        assert_eq!(base, cache_scope(&node, "uid-1", &Backend::S3, &cfg));
        assert_ne!(base, cache_scope(&node, "uid-2", &Backend::S3, &cfg));
        let other = HashMap::from([("bucket".to_string(), "b".to_string())]);
        assert_ne!(base, cache_scope(&node, "uid-1", &Backend::S3, &other));
        assert_ne!(base, cache_scope(&node, "uid-1", &Backend::GCS, &cfg));
    }
}
