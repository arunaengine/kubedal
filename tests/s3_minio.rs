//! Cache, write-back and file-list volumes against a real S3 server, like MinIO
//! or another S3 compatible server.
//!
//! These tests only run when `KUBEDAL_TEST_S3_ENDPOINT` is set, for example:
//!
//! ```sh
//! docker run -d -p 9000:9000 -e RUSTFS_ACCESS_KEY=minioadmin \
//!     -e RUSTFS_SECRET_KEY=minioadmin rustfs/rustfs
//! curl --aws-sigv4 aws:amz:us-east-1:s3 --user minioadmin:minioadmin \
//!     -X PUT http://127.0.0.1:9000/kubedal-test
//! KUBEDAL_TEST_S3_ENDPOINT=http://127.0.0.1:9000 cargo test --test s3_minio
//! ```
//!
//! Optional: `KUBEDAL_TEST_S3_ACCESS_KEY`, `KUBEDAL_TEST_S3_SECRET_KEY` (default
//! `minioadmin`) and `KUBEDAL_TEST_S3_BUCKET` (default `kubedal-test`). The bucket
//! must exist.

use kubedal::node::cluster::{Cluster, EventKind, ResolvedNode};
use kubedal::node::paths::publish_id;
use kubedal::node::state::{DataNodeRef, PodRef};
use kubedal::node::testing::FakeMounter;
use kubedal::node::volume::{PublishRequest, TreeMode, VolumeManager, VolumeSource};
use kubedal::resource::crd::Backend;
use kubedal::util::opendal::build_operator;
use opendal::Operator;
use std::collections::HashMap;
use std::sync::Arc;
use tonic::Status;

struct S3Cluster {
    config: HashMap<String, String>,
}

#[tonic::async_trait]
impl Cluster for S3Cluster {
    async fn resolve(&self, node: &DataNodeRef, _pod: &PodRef) -> Result<ResolvedNode, Status> {
        let cfg = self.config.clone();
        let factory = Arc::new(move |root: &str| build_operator(&Backend::S3, cfg.clone(), root));
        Ok(ResolvedNode::new(node.clone(), None, false, factory))
    }

    async fn pod_event(&self, _pod: &PodRef, _kind: EventKind, _reason: &str, _message: &str) {}
}

fn s3_config() -> Option<HashMap<String, String>> {
    let endpoint = std::env::var("KUBEDAL_TEST_S3_ENDPOINT").ok()?;
    let get = |key: &str, default: &str| std::env::var(key).unwrap_or_else(|_| default.into());
    // Each test run uses its own root, so runs never see each other's files.
    let root = format!("/run-{}/", uuid::Uuid::new_v4().simple());
    Some(HashMap::from([
        ("endpoint".into(), endpoint),
        ("region".into(), "us-east-1".into()),
        (
            "bucket".into(),
            get("KUBEDAL_TEST_S3_BUCKET", "kubedal-test"),
        ),
        (
            "access_key_id".into(),
            get("KUBEDAL_TEST_S3_ACCESS_KEY", "minioadmin"),
        ),
        (
            "secret_access_key".into(),
            get("KUBEDAL_TEST_S3_SECRET_KEY", "minioadmin"),
        ),
        ("root".into(), root),
    ]))
}

async fn check_bucket(cfg: &HashMap<String, String>) {
    let op = build_operator(&Backend::S3, cfg.clone(), "").unwrap();
    if let Err(err) = op.list("").await {
        panic!(
            "bucket {} at {} is not usable ({err}); create it before running the S3 tests",
            cfg["bucket"], cfg["endpoint"]
        );
    }
}

fn pod() -> PodRef {
    PodRef {
        name: "task".into(),
        namespace: "team".into(),
        uid: "uid".into(),
        service_account: "default".into(),
    }
}

fn node() -> DataNodeRef {
    DataNodeRef {
        name: "minio".into(),
        namespace: "team".into(),
    }
}

async fn setup() -> Option<(tempfile::TempDir, Operator, VolumeManager)> {
    let cfg = s3_config()?;
    check_bucket(&cfg).await;
    let op = build_operator(&Backend::S3, cfg.clone(), "").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let manager = VolumeManager::new(
        dir.path().join("node"),
        Arc::new(S3Cluster { config: cfg }),
        Arc::new(FakeMounter::default()),
    )
    .unwrap();
    Some((dir, op, manager))
}

#[tokio::test]
async fn cache_download_and_write_back() {
    let Some((dir, op, manager)) = setup().await else {
        eprintln!("KUBEDAL_TEST_S3_ENDPOINT is not set, skipping");
        return;
    };
    op.write("in/a.txt", "hello").await.unwrap();
    op.write("in/sub/b.txt", vec![1u8; 9 * 1024 * 1024])
        .await
        .unwrap();
    op.create_dir("in/empty/").await.unwrap();

    let target = dir.path().join("target");
    manager
        .publish(PublishRequest {
            volume_id: "vol".into(),
            target_path: target.clone(),
            read_only: false,
            pod: pod(),
            source: VolumeSource::Tree {
                node: node(),
                path: "in".into(),
                mode: TreeMode::Cache,
                writable: true,
            },
            data_pod_uid: None,
        })
        .await
        .unwrap();
    let id = publish_id("vol", &target.to_string_lossy());
    let data = manager.data_dir(&id);
    assert_eq!(
        std::fs::read_to_string(data.join("a.txt")).unwrap(),
        "hello"
    );
    assert_eq!(
        std::fs::metadata(data.join("sub/b.txt")).unwrap().len(),
        9 * 1024 * 1024
    );
    assert!(data.join("empty").is_dir());

    std::fs::write(data.join("out.txt"), "result").unwrap();
    manager.unpublish("vol", &target).await.unwrap();
    assert_eq!(
        op.read("in/out.txt").await.unwrap().to_vec(),
        b"result".to_vec()
    );
    op.delete_with("").recursive(true).await.unwrap();
}

#[tokio::test]
async fn file_list_with_upload() {
    let Some((dir, op, manager)) = setup().await else {
        eprintln!("KUBEDAL_TEST_S3_ENDPOINT is not set, skipping");
        return;
    };
    op.write("objects/tool", "#!/bin/sh\necho hi\n")
        .await
        .unwrap();
    op.write(
        "jobs/1.json",
        r#"{"version": 1,
            "files": [{"path": "bin/tool", "source": "objects/tool", "executable": true}],
            "directories": ["out"],
            "upload": {"dataNode": "minio", "prefix": "results/1", "path": "out"}}"#,
    )
    .await
    .unwrap();

    let target = dir.path().join("target");
    manager
        .publish(PublishRequest {
            volume_id: "vol".into(),
            target_path: target.clone(),
            read_only: false,
            pod: pod(),
            source: VolumeSource::FileList {
                node: node(),
                manifest: "jobs/1.json".into(),
            },
            data_pod_uid: None,
        })
        .await
        .unwrap();
    let id = publish_id("vol", &target.to_string_lossy());
    std::fs::write(manager.data_dir(&id).join("out/r.txt"), "done").unwrap();
    manager.unpublish("vol", &target).await.unwrap();
    assert_eq!(
        op.read("results/1/r.txt").await.unwrap().to_vec(),
        b"done".to_vec()
    );
    op.delete_with("").recursive(true).await.unwrap();
}

#[tokio::test]
async fn fuse_closed_file_rename_on_s3() {
    use fuse3::path::prelude::*;
    use kubedal::util::fuse_fs::Filesystem;
    use std::ffi::OsStr;
    let Some(cfg) = s3_config() else {
        eprintln!("KUBEDAL_TEST_S3_ENDPOINT is not set, skipping");
        return;
    };
    check_bucket(&cfg).await;
    let op = build_operator(&Backend::S3, cfg, "").unwrap();
    let spool = tempfile::tempdir().unwrap();
    let fs = Filesystem::new(op.clone(), 0, 0, spool.path().to_path_buf());
    let req = Request {
        unique: 1,
        uid: 0,
        gid: 0,
        pid: 0,
    };
    let flags = (libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC) as u32;
    let fh = fs
        .create(req, OsStr::new("/"), OsStr::new("out.tmp"), 0o644, flags)
        .await
        .unwrap()
        .fh;
    let p = Some(OsStr::new("/out.tmp"));
    fs.write(req, p, fh, 0, b"result", 0, 0).await.unwrap();
    fs.release(req, p, fh, 0, 0, false).await.unwrap();
    fs.rename(
        req,
        OsStr::new("/"),
        OsStr::new("out.tmp"),
        OsStr::new("/"),
        OsStr::new("out"),
    )
    .await
    .unwrap();
    assert!(!op.exists("out.tmp").await.unwrap());
    assert_eq!(op.read("out").await.unwrap().to_vec(), b"result");
    op.delete_with("").recursive(true).await.unwrap();
}

#[tokio::test]
async fn fuse_reader_does_not_see_a_replaced_object() {
    use fuse3::path::prelude::*;
    use kubedal::util::fuse_fs::Filesystem;
    use std::ffi::OsStr;
    let Some(cfg) = s3_config() else {
        eprintln!("KUBEDAL_TEST_S3_ENDPOINT is not set, skipping");
        return;
    };
    check_bucket(&cfg).await;
    let op = build_operator(&Backend::S3, cfg, "").unwrap();
    op.write("f.txt", "old").await.unwrap();
    let spool = tempfile::tempdir().unwrap();
    let fs = Filesystem::new(op.clone(), 0, 0, spool.path().to_path_buf());
    let req = Request {
        unique: 1,
        uid: 0,
        gid: 0,
        pid: 0,
    };
    let fh = fs
        .open(req, OsStr::new("/f.txt"), libc::O_RDONLY as u32)
        .await
        .unwrap()
        .fh;
    // Someone replaces the object; the open reader must not get the new bytes.
    op.write("f.txt", "new").await.unwrap();
    let err = fs
        .read(req, Some(OsStr::new("/f.txt")), fh, 0, 100)
        .await
        .unwrap_err();
    assert_eq!(err, fuse3::Errno::from(libc::ESTALE));
    op.delete_with("").recursive(true).await.unwrap();
}
