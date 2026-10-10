//! Shared setup for integration tests.
#![allow(dead_code)]

use kubedal::node::state::{DataNodeRef, PodRef};
use kubedal::node::testing::{FakeCluster, FakeMounter, FakeNode};
use kubedal::node::volume::{PublishRequest, TreeMode, VolumeManager, VolumeSource};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

pub struct Env {
    pub dir: TempDir,
    pub cluster: Arc<FakeCluster>,
    pub mounter: Arc<FakeMounter>,
    pub manager: VolumeManager,
}

impl Env {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cluster = Arc::new(FakeCluster::default());
        let mounter = Arc::new(FakeMounter::default());
        let manager =
            VolumeManager::new(dir.path().join("node"), cluster.clone(), mounter.clone()).unwrap();
        Self {
            dir,
            cluster,
            mounter,
            manager,
        }
    }

    /// Start a new manager on the same data directory, like after a restart.
    pub fn restart(&mut self) {
        self.manager = VolumeManager::new(
            self.dir.path().join("node"),
            self.cluster.clone(),
            self.mounter.clone(),
        )
        .unwrap();
    }

    /// Add a DataNode "store" in namespace "team" with some files. Returns its root.
    pub fn add_store(&self, limit: Option<u64>, read_only: bool) -> PathBuf {
        let root = self.dir.path().join("bucket");
        write(&root.join("data/a.txt"), "hello");
        write(&root.join("data/sub/b.txt"), "world");
        std::fs::create_dir_all(root.join("data/empty")).unwrap();
        write(&root.join("other/c.txt"), "other");
        self.cluster.add_node(
            "team",
            "store",
            FakeNode {
                root: root.clone(),
                limit,
                read_only,
            },
        );
        root
    }

    pub fn target(&self, name: &str) -> PathBuf {
        let parent = self.dir.path().join("pods").join(name);
        std::fs::create_dir_all(&parent).unwrap();
        parent.join("mount")
    }
}

pub fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

pub fn store() -> DataNodeRef {
    DataNodeRef {
        name: "store".into(),
        namespace: "team".into(),
    }
}

pub fn pod(namespace: &str) -> PodRef {
    PodRef {
        name: "task".into(),
        namespace: namespace.into(),
        uid: "uid-1".into(),
        service_account: "default".into(),
    }
}

pub fn tree_request(
    volume_id: &str,
    target: &Path,
    path: &str,
    mode: TreeMode,
    writable: bool,
) -> PublishRequest {
    PublishRequest {
        volume_id: volume_id.into(),
        target_path: target.to_path_buf(),
        read_only: false,
        pod: pod("team"),
        source: VolumeSource::Tree {
            node: store(),
            path: path.into(),
            mode,
            writable,
        },
    }
}
