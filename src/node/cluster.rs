//! What the volume code needs from Kubernetes, behind a trait so tests can fake it.

use crate::node::state::{DataNodeRef, PodRef};
use opendal::Operator;
use std::sync::Arc;
use tonic::Status;

/// Builds an operator for a root path inside a DataNode.
pub type OperatorFactory = Arc<dyn Fn(&str) -> Result<Operator, Status> + Send + Sync>;

/// A DataNode that a pod is allowed to use, ready for data access.
///
/// Credentials stay inside the operator factory and are never written to disk.
#[derive(Clone)]
pub struct ResolvedNode {
    pub node: DataNodeRef,
    /// Maximum bytes a volume may hold, from the DataNode `limit`.
    pub limit: Option<u64>,
    /// The DataNode does not allow writes.
    pub read_only: bool,
    factory: OperatorFactory,
    cache_scope: String,
    identity: String,
}

impl ResolvedNode {
    pub fn new(
        node: DataNodeRef,
        limit: Option<u64>,
        read_only: bool,
        factory: OperatorFactory,
    ) -> Self {
        let cache_scope = node.to_string();
        let identity = node.to_string();
        Self {
            node,
            limit,
            read_only,
            factory,
            cache_scope,
            identity,
        }
    }

    /// Set the identity of the DataNode object: its UID and a hash of its
    /// backend and non-secret configuration. Output is only written back to a
    /// DataNode with the identity it had when the volume was published.
    pub fn with_identity(mut self, identity: String) -> Self {
        self.identity = identity;
        self
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Fail if the DataNode is not the one a volume was published with.
    pub fn require_identity(&self, expected: Option<&str>) -> Result<(), Status> {
        match expected {
            Some(expected) if expected != self.identity => {
                Err(Status::failed_precondition(format!(
                    "DataNode {} was replaced or reconfigured since the volume was published; \
                     the output is kept on the node",
                    self.node
                )))
            }
            _ => Ok(()),
        }
    }

    /// Set the content cache scope. It must change whenever the DataNode could
    /// point to other data: a new object with the same name (new UID) or a
    /// changed backend configuration.
    pub fn with_cache_scope(mut self, scope: String) -> Self {
        self.cache_scope = scope;
        self
    }

    /// Content cache entries are only shared between volumes with the same scope.
    pub fn cache_scope(&self) -> &str {
        &self.cache_scope
    }

    /// An operator whose root is `root` inside the DataNode. "" is the DataNode root.
    pub fn operator(&self, root: &str) -> Result<Operator, Status> {
        (self.factory)(root)
    }

    /// Fail if the DataNode is read-only.
    pub fn require_writable(&self) -> Result<(), Status> {
        if self.read_only {
            return Err(Status::permission_denied(format!(
                "DataNode {} is read-only and cannot be used for writing",
                self.node
            )));
        }
        Ok(())
    }
}

/// Kind of a Kubernetes Event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Normal,
    Warning,
}

// async_trait adds `#[must_use]` to methods that return a Future.
#[allow(clippy::double_must_use)]
#[tonic::async_trait]
pub trait Cluster: Send + Sync {
    /// Look up a DataNode and check that the pod may use it.
    async fn resolve(&self, node: &DataNodeRef, pod: &PodRef) -> Result<ResolvedNode, Status>;

    /// Record an Event on a pod. Failures are logged, not returned.
    async fn pod_event(&self, pod: &PodRef, kind: EventKind, reason: &str, message: &str);
}
