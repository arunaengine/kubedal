//! Cache, write-back and FUSE volumes against emulators of Google Cloud Storage
//! (fake-gcs-server) and Azure Blob Storage (Azurite).
//!
//! The GCS tests only run when `KUBEDAL_TEST_GCS_ENDPOINT` is set, and the Azure
//! Blob tests only when `KUBEDAL_TEST_AZBLOB_ENDPOINT` is set, for example:
//!
//! ```sh
//! docker run -d -p 4443:4443 fsouza/fake-gcs-server:1.56.1 \
//!     -scheme http -port 4443 -public-host 127.0.0.1:4443
//! curl -X POST -H 'Content-Type: application/json' -d '{"name": "kubedal-test"}' \
//!     http://127.0.0.1:4443/storage/v1/b
//! docker run -d -p 10000:10000 mcr.microsoft.com/azure-storage/azurite \
//!     azurite-blob --blobHost 0.0.0.0
//! python3 test/azurite_container.py
//! KUBEDAL_TEST_GCS_ENDPOINT=http://127.0.0.1:4443 \
//!     KUBEDAL_TEST_AZBLOB_ENDPOINT=http://127.0.0.1:10000/devstoreaccount1 \
//!     cargo test --test gcs_azblob
//! ```
//!
//! The bucket and the container are named `kubedal-test` and must exist. The
//! Azure Blob tests use the well-known Azurite development account.

use fuse3::path::prelude::*;
use kubedal::node::cluster::{Cluster, EventKind, ResolvedNode};
use kubedal::node::paths::publish_id;
use kubedal::node::state::{DataNodeRef, PodRef};
use kubedal::node::testing::FakeMounter;
use kubedal::node::volume::{PublishRequest, TreeMode, VolumeManager, VolumeSource};
use kubedal::resource::crd::Backend;
use kubedal::util::fuse_fs::Filesystem;
use kubedal::util::opendal::build_operator;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::Arc;
use tonic::Status;

struct EmulatorCluster {
    backend: Backend,
    config: HashMap<String, String>,
}

#[tonic::async_trait]
impl Cluster for EmulatorCluster {
    async fn resolve(&self, node: &DataNodeRef, _pod: &PodRef) -> Result<ResolvedNode, Status> {
        let (backend, cfg) = (self.backend.clone(), self.config.clone());
        let factory = Arc::new(move |root: &str| build_operator(&backend, cfg.clone(), root));
        Ok(ResolvedNode::new(node.clone(), None, false, factory))
    }

    async fn pod_event(&self, _pod: &PodRef, _kind: EventKind, _reason: &str, _message: &str) {}
}

/// Each test run uses its own root, so runs never see each other's files.
fn run_root() -> (String, String) {
    (
        "root".into(),
        format!("/run-{}/", uuid::Uuid::new_v4().simple()),
    )
}

fn gcs_config() -> Option<HashMap<String, String>> {
    let endpoint = std::env::var("KUBEDAL_TEST_GCS_ENDPOINT").ok()?;
    Some(HashMap::from([
        ("endpoint".into(), endpoint),
        ("bucket".into(), "kubedal-test".into()),
        // The emulator takes requests without credentials.
        ("allow_anonymous".into(), "true".into()),
        ("disable_vm_metadata".into(), "true".into()),
        ("disable_config_load".into(), "true".into()),
        run_root(),
    ]))
}

fn azblob_config() -> Option<HashMap<String, String>> {
    let endpoint = std::env::var("KUBEDAL_TEST_AZBLOB_ENDPOINT").ok()?;
    Some(HashMap::from([
        ("endpoint".into(), endpoint),
        ("container".into(), "kubedal-test".into()),
        ("account_name".into(), "devstoreaccount1".into()),
        (
            "account_key".into(),
            "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw=="
                .into(),
        ),
        run_root(),
    ]))
}

fn pod() -> PodRef {
    PodRef {
        name: "task".into(),
        namespace: "team".into(),
        uid: "uid".into(),
        service_account: "default".into(),
    }
}

fn req() -> Request {
    Request {
        unique: 1,
        uid: 0,
        gid: 0,
        pid: 0,
    }
}

/// Remove the files of this run one by one. fake-gcs-server has no batch
/// delete, which a recursive delete uses.
async fn remove_all(op: &opendal::Operator) {
    for entry in op.list_with("").recursive(true).await.unwrap() {
        op.delete(entry.path()).await.unwrap();
    }
}

/// Download a folder into a cache volume, write a file and upload it again.
async fn cache_download_and_write_back(backend: Backend, cfg: HashMap<String, String>) {
    let op = build_operator(&backend, cfg.clone(), "").unwrap();
    op.write("in/a.txt", "hello").await.unwrap();
    op.write("in/sub/b.txt", vec![1u8; 3 * 1024 * 1024])
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let manager = VolumeManager::new(
        dir.path().join("node"),
        Arc::new(EmulatorCluster {
            backend,
            config: cfg,
        }),
        Arc::new(FakeMounter::default()),
    )
    .unwrap();
    let target = dir.path().join("target");
    manager
        .publish(PublishRequest {
            volume_id: "vol".into(),
            target_path: target.clone(),
            read_only: false,
            pod: pod(),
            source: VolumeSource::Tree {
                node: DataNodeRef {
                    name: "store".into(),
                    namespace: "team".into(),
                },
                path: "in".into(),
                mode: TreeMode::Cache,
                writable: true,
            },
            data_pod_uid: None,
        })
        .await
        .unwrap();
    let data = manager.data_dir(&publish_id("vol", &target.to_string_lossy()));
    assert_eq!(
        std::fs::read_to_string(data.join("a.txt")).unwrap(),
        "hello"
    );
    assert_eq!(
        std::fs::metadata(data.join("sub/b.txt")).unwrap().len(),
        3 * 1024 * 1024
    );

    std::fs::write(data.join("out.txt"), "result").unwrap();
    manager.unpublish("vol", &target).await.unwrap();
    assert_eq!(op.read("in/out.txt").await.unwrap().to_vec(), b"result");
    remove_all(&op).await;
}

/// Write, list, rename and remove files through the FUSE file system.
async fn fuse_write_list_rename(backend: Backend, cfg: HashMap<String, String>) {
    use futures::StreamExt;
    let op = build_operator(&backend, cfg, "").unwrap();
    let spool = tempfile::tempdir().unwrap();
    let fs = Filesystem::new(op.clone(), 0, 0, spool.path().to_path_buf());
    let root = OsStr::new("/");
    let flags = (libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC) as u32;
    let fh = fs
        .create(req(), root, OsStr::new("out.tmp"), 0o644, flags)
        .await
        .unwrap()
        .fh;
    let p = Some(OsStr::new("/out.tmp"));
    fs.write(req(), p, fh, 0, b"result", 0, 0).await.unwrap();
    fs.release(req(), p, fh, 0, 0, false).await.unwrap();
    assert_eq!(op.read("out.tmp").await.unwrap().to_vec(), b"result");

    fs.rename(req(), root, OsStr::new("out.tmp"), root, OsStr::new("out"))
        .await
        .unwrap();
    assert!(!op.exists("out.tmp").await.unwrap());
    assert_eq!(op.read("out").await.unwrap().to_vec(), b"result");

    let reply = fs.readdir(req(), root, 0, 0).await.unwrap();
    let names: Vec<_> = reply
        .entries
        .map(|e| e.unwrap().name.into_string().unwrap())
        .collect()
        .await;
    assert_eq!(names, [".", "..", "out"]);

    fs.unlink(req(), root, OsStr::new("out")).await.unwrap();
    assert!(!op.exists("out").await.unwrap());
    remove_all(&op).await;
}

#[tokio::test]
async fn gcs_cache_download_and_write_back() {
    let Some(cfg) = gcs_config() else {
        eprintln!("KUBEDAL_TEST_GCS_ENDPOINT is not set, skipping");
        return;
    };
    cache_download_and_write_back(Backend::GCS, cfg).await;
}

#[tokio::test]
async fn gcs_fuse_write_list_rename() {
    let Some(cfg) = gcs_config() else {
        eprintln!("KUBEDAL_TEST_GCS_ENDPOINT is not set, skipping");
        return;
    };
    fuse_write_list_rename(Backend::GCS, cfg).await;
}

#[tokio::test]
async fn azblob_cache_download_and_write_back() {
    let Some(cfg) = azblob_config() else {
        eprintln!("KUBEDAL_TEST_AZBLOB_ENDPOINT is not set, skipping");
        return;
    };
    cache_download_and_write_back(Backend::Azblob, cfg).await;
}

#[tokio::test]
async fn azblob_fuse_write_list_rename() {
    let Some(cfg) = azblob_config() else {
        eprintln!("KUBEDAL_TEST_AZBLOB_ENDPOINT is not set, skipping");
        return;
    };
    fuse_write_list_rename(Backend::Azblob, cfg).await;
}
