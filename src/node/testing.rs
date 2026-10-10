//! Fakes for [`Cluster`] and [`Mounter`], so volume code can be tested without
//! Kubernetes and without root. Used by unit and integration tests.

use crate::node::cluster::{Cluster, EventKind, ResolvedNode};
use crate::node::mounter::Mounter;
use crate::node::state::{DataNodeRef, PodRef};
use opendal::Operator;
use opendal::services::Fs;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tonic::Status;

/// A DataNode backed by a local directory.
#[derive(Debug, Clone)]
pub struct FakeNode {
    pub root: PathBuf,
    pub limit: Option<u64>,
    pub read_only: bool,
}

/// An Event that was recorded on a pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedEvent {
    pub pod: String,
    pub kind: EventKind,
    pub reason: String,
    pub message: String,
}

/// A cluster with DataNodes on the local file system.
///
/// Like the real cluster, a pod may only use DataNodes in its own namespace,
/// unless the DataNode was shared with [`FakeCluster::allow`].
#[derive(Default)]
pub struct FakeCluster {
    nodes: Mutex<HashMap<DataNodeRef, FakeNode>>,
    allowed: Mutex<Vec<(String, DataNodeRef)>>,
    events: Mutex<Vec<RecordedEvent>>,
}

pub fn fs_operator(root: &Path) -> Result<Operator, Status> {
    let root = root.to_string_lossy().to_string();
    Operator::new(Fs::default().root(&root))
        .map_err(|e| Status::internal(format!("fs operator: {e}")))
}

impl FakeCluster {
    pub fn add_node(&self, namespace: &str, name: &str, node: FakeNode) {
        let key = DataNodeRef {
            name: name.into(),
            namespace: namespace.into(),
        };
        self.nodes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key, node);
    }

    pub fn remove_node(&self, namespace: &str, name: &str) {
        let key = DataNodeRef {
            name: name.into(),
            namespace: namespace.into(),
        };
        self.nodes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key);
    }

    /// Let pods in `pod_namespace` use a DataNode in another namespace.
    pub fn allow(&self, pod_namespace: &str, node: DataNodeRef) {
        self.allowed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((pod_namespace.into(), node));
    }

    pub fn events(&self) -> Vec<RecordedEvent> {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

#[tonic::async_trait]
impl Cluster for FakeCluster {
    async fn resolve(&self, node: &DataNodeRef, pod: &PodRef) -> Result<ResolvedNode, Status> {
        let allowed = node.namespace == pod.namespace
            || self
                .allowed
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .any(|(ns, n)| ns == &pod.namespace && n == node);
        if !allowed {
            return Err(Status::permission_denied(format!(
                "pod may not use DataNode {node}"
            )));
        }
        let fake = self
            .nodes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(node)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("DataNode {node} not found")))?;
        let root = fake.root.clone();
        let factory = Arc::new(move |sub: &str| {
            let dir = root.join(sub.trim_matches('/'));
            std::fs::create_dir_all(&dir)
                .map_err(|e| Status::internal(format!("create fake root: {e}")))?;
            fs_operator(&dir)
        });
        Ok(ResolvedNode::new(
            node.clone(),
            fake.limit,
            fake.read_only,
            factory,
        ))
    }

    async fn pod_event(&self, pod: &PodRef, kind: EventKind, reason: &str, message: &str) {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(RecordedEvent {
                pod: format!("{}/{}", pod.namespace, pod.name),
                kind,
                reason: reason.into(),
                message: message.into(),
            });
    }
}

/// What the fake mounter has at a target path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeMount {
    Bind { source: PathBuf, read_only: bool },
    Fuse { read_only: bool },
}

/// Records mounts in memory instead of calling the kernel.
#[derive(Default)]
pub struct FakeMounter {
    mounts: Mutex<HashMap<PathBuf, FakeMount>>,
    /// Targets whose FUSE process "died", for example by a restart.
    dead_fuse: Mutex<Vec<PathBuf>>,
}

impl FakeMounter {
    pub fn mounts(&self) -> HashMap<PathBuf, FakeMount> {
        self.mounts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn get(&self, target: &Path) -> Option<FakeMount> {
        self.mounts().get(target).cloned()
    }

    /// Forget all FUSE sessions but keep the mounts, like after a restart.
    pub fn kill_fuse_sessions(&self) {
        let mounts = self.mounts();
        let mut dead = self.dead_fuse.lock().unwrap_or_else(|p| p.into_inner());
        for (target, mount) in mounts {
            if matches!(mount, FakeMount::Fuse { .. }) {
                dead.push(target);
            }
        }
    }

    /// Forget all mounts, like after a node reboot.
    pub fn clear(&self) {
        self.mounts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    fn insert(&self, target: &Path, mount: FakeMount) -> Result<(), Status> {
        if !target.is_dir() {
            return Err(Status::internal(format!(
                "mount target {target:?} does not exist"
            )));
        }
        self.mounts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(target.to_path_buf(), mount);
        self.dead_fuse
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|t| t != target);
        Ok(())
    }
}

#[tonic::async_trait]
impl Mounter for FakeMounter {
    async fn bind(&self, source: &Path, target: &Path, read_only: bool) -> Result<(), Status> {
        self.insert(
            target,
            FakeMount::Bind {
                source: source.to_path_buf(),
                read_only,
            },
        )
    }

    async fn mount_fuse(
        &self,
        _op: Operator,
        target: &Path,
        read_only: bool,
    ) -> Result<(), Status> {
        self.insert(target, FakeMount::Fuse { read_only })
    }

    async fn unmount(&self, target: &Path) -> Result<(), Status> {
        self.mounts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(target);
        self.dead_fuse
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|t| t != target);
        Ok(())
    }

    fn is_mounted(&self, target: &Path) -> bool {
        self.get(target).is_some()
    }

    async fn has_fuse_session(&self, target: &Path) -> bool {
        matches!(self.get(target), Some(FakeMount::Fuse { .. }))
            && !self
                .dead_fuse
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .any(|t| t == target)
    }
}
