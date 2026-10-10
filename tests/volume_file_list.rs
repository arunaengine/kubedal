//! File-list volumes: files from a manifest, the content cache and uploads.

mod common;

use common::{Env, pod, store, write};
use kubedal::node::cluster::EventKind;
use kubedal::node::paths::publish_id;
use kubedal::node::state::DataNodeRef;
use kubedal::node::testing::{FakeMount, FakeNode};
use kubedal::node::volume::{PublishRequest, VolumeSource};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tonic::Code;

const HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
const WORLD: &str = "486ea46224d1bb4fb680f34f7c9ad96a8f24ec88be73ea8e5a6c65260e9cb8a7";

fn request(volume_id: &str, target: &Path, manifest: &str) -> PublishRequest {
    PublishRequest {
        volume_id: volume_id.into(),
        target_path: target.to_path_buf(),
        read_only: false,
        pod: pod("team"),
        source: VolumeSource::FileList {
            node: store(),
            manifest: manifest.into(),
        },
    }
}

/// Add a "results" DataNode in the namespace "team". Returns its root.
fn add_results(env: &Env) -> PathBuf {
    let root = env.dir.path().join("results");
    std::fs::create_dir_all(&root).unwrap();
    env.cluster.add_node(
        "team",
        "results",
        FakeNode {
            root: root.clone(),
            limit: None,
            read_only: false,
        },
    );
    root
}

fn manifest_json() -> String {
    format!(
        r#"{{
            "version": 1,
            "files": [
                {{"path": "in/hello.txt", "source": "data/a.txt", "sha256": "{HELLO}", "size": 5}},
                {{"path": "in/deep/world.txt", "source": "data/sub/b.txt", "sha256": "{WORLD}"}},
                {{"path": "bin/run.sh", "source": "other/c.txt", "executable": true}}
            ],
            "directories": ["out", "tmp"],
            "upload": {{"dataNode": "results", "prefix": "runs/1", "path": "out"}}
        }}"#
    )
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn files_are_placed_and_outputs_uploaded() {
    let env = Env::new();
    let bucket = env.add_store(None, false);
    let results = add_results(&env);
    write(&bucket.join("jobs/1.json"), &manifest_json());
    let target = env.target("p1");

    env.manager
        .publish(request("vol-1", &target, "jobs/1.json"))
        .await
        .unwrap();
    let id = publish_id("vol-1", &target.to_string_lossy());
    let data = env.manager.data_dir(&id);
    assert_eq!(
        std::fs::read_to_string(data.join("in/hello.txt")).unwrap(),
        "hello"
    );
    assert_eq!(
        std::fs::read_to_string(data.join("in/deep/world.txt")).unwrap(),
        "world"
    );
    assert_eq!(mode(&data.join("in/hello.txt")), 0o644);
    assert_eq!(mode(&data.join("bin/run.sh")), 0o755);
    assert!(data.join("out").is_dir() && data.join("tmp").is_dir());
    assert!(matches!(
        env.mounter.get(&target),
        Some(FakeMount::Bind {
            read_only: false,
            ..
        })
    ));

    // The pod writes results and scribbles on an input.
    write(&data.join("out/result.txt"), "42");
    write(&data.join("in/hello.txt"), "scribbled");
    env.manager.unpublish("vol-1", &target).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(results.join("runs/1/result.txt")).unwrap(),
        "42"
    );
    // Only the "out" folder is uploaded.
    assert!(!results.join("runs/1/in").exists());
    let events = env.cluster.events();
    assert_eq!(events.last().unwrap().reason, "VolumeUploaded");

    // The cache still holds the original content.
    let cached = env.manager.content_cache().path("team/store", HELLO);
    assert_eq!(std::fs::read_to_string(cached).unwrap(), "hello");
}

#[tokio::test]
async fn shared_files_are_downloaded_once_per_node() {
    let env = Env::new();
    let bucket = env.add_store(None, false);
    add_results(&env);
    write(&bucket.join("jobs/1.json"), &manifest_json());

    let first = env.target("p1");
    env.manager
        .publish(request("vol-1", &first, "jobs/1.json"))
        .await
        .unwrap();

    // If the second volume read from storage, it would see this change.
    write(&bucket.join("data/a.txt"), "HELLO");
    let second = env.target("p2");
    env.manager
        .publish(request("vol-2", &second, "jobs/1.json"))
        .await
        .unwrap();
    let id = publish_id("vol-2", &second.to_string_lossy());
    assert_eq!(
        std::fs::read_to_string(env.manager.data_dir(&id).join("in/hello.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn digest_mismatch_fails_and_cleans_up() {
    let env = Env::new();
    let bucket = env.add_store(None, false);
    write(
        &bucket.join("jobs/bad.json"),
        &format!(
            r#"{{"version": 1, "files": [{{"path": "x", "source": "data/a.txt", "sha256": "{WORLD}"}}]}}"#
        ),
    );
    let target = env.target("p1");
    let err = env
        .manager
        .publish(request("vol-1", &target, "jobs/bad.json"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::DataLoss);
    let id = publish_id("vol-1", &target.to_string_lossy());
    assert!(!env.manager.volume_dir(&id).exists());
    assert!(env.manager.state().load(&id).unwrap().is_none());
    assert!(
        !env.manager
            .content_cache()
            .path("team/store", WORLD)
            .exists()
    );
}

#[tokio::test]
async fn missing_manifest_is_not_found() {
    let env = Env::new();
    env.add_store(None, false);
    let target = env.target("p1");
    let err = env
        .manager
        .publish(request("vol-1", &target, "jobs/none.json"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn upload_target_is_checked_before_download() {
    let env = Env::new();
    let bucket = env.add_store(None, false);
    // The upload goes to a DataNode in a namespace the pod may not use.
    let other = env.dir.path().join("other-ns");
    env.cluster.add_node(
        "elsewhere",
        "results",
        FakeNode {
            root: other,
            limit: None,
            read_only: false,
        },
    );
    write(
        &bucket.join("jobs/1.json"),
        r#"{"version": 1, "upload": {"dataNode": "results", "dataNodeNamespace": "elsewhere"}}"#,
    );
    let target = env.target("p1");
    let err = env
        .manager
        .publish(request("vol-1", &target, "jobs/1.json"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);

    env.cluster.allow(
        "team",
        DataNodeRef {
            name: "results".into(),
            namespace: "elsewhere".into(),
        },
    );
    env.manager
        .publish(request("vol-1", &target, "jobs/1.json"))
        .await
        .unwrap();
}

#[tokio::test]
async fn read_only_volume_with_upload_is_rejected() {
    let env = Env::new();
    let bucket = env.add_store(None, false);
    add_results(&env);
    write(&bucket.join("jobs/1.json"), &manifest_json());
    let target = env.target("p1");
    let mut req = request("vol-1", &target, "jobs/1.json");
    req.read_only = true;
    let err = env.manager.publish(req).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn declared_sizes_are_checked_against_the_limit() {
    let env = Env::new();
    let bucket = env.add_store(Some(4), false);
    write(
        &bucket.join("jobs/1.json"),
        &format!(
            r#"{{"version": 1, "files": [{{"path": "x", "source": "data/a.txt", "sha256": "{HELLO}", "size": 5}}]}}"#
        ),
    );
    let target = env.target("p1");
    let err = env
        .manager
        .publish(request("vol-1", &target, "jobs/1.json"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted);
    // Nothing was downloaded into the cache.
    assert!(
        !env.manager
            .content_cache()
            .path("team/store", HELLO)
            .exists()
    );
}

#[tokio::test]
async fn invalid_manifest_is_rejected() {
    let env = Env::new();
    let bucket = env.add_store(None, false);
    write(
        &bucket.join("jobs/1.json"),
        r#"{"version": 1, "files": [{"path": "../../etc/x", "source": "data/a.txt"}]}"#,
    );
    let target = env.target("p1");
    let err = env
        .manager
        .publish(request("vol-1", &target, "jobs/1.json"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(
        env.cluster
            .events()
            .iter()
            .all(|e| e.kind != EventKind::Normal)
    );
}

#[tokio::test]
async fn cache_hits_need_the_same_data_node() {
    let env = Env::new();
    let bucket = env.add_store(None, false);
    write(&bucket.join("jobs/1.json"), &manifest_json());
    add_results(&env);
    env.manager
        .publish(request("vol-1", &env.target("p1"), "jobs/1.json"))
        .await
        .unwrap();

    // Another namespace with its own DataNode knows the digest of a file in "store",
    // but its source does not exist. It must not get the cached bytes.
    let other = env.dir.path().join("other-bucket");
    write(
        &other.join("jobs/steal.json"),
        &format!(
            r#"{{"version": 1, "files": [{{"path": "x", "source": "missing", "sha256": "{HELLO}"}}]}}"#
        ),
    );
    env.cluster.add_node(
        "other",
        "store",
        FakeNode {
            root: other,
            limit: None,
            read_only: false,
        },
    );
    let mut req = request("vol-2", &env.target("p2"), "jobs/steal.json");
    req.pod = pod("other");
    req.source = VolumeSource::FileList {
        node: DataNodeRef {
            name: "store".into(),
            namespace: "other".into(),
        },
        manifest: "jobs/steal.json".into(),
    };
    let err = env.manager.publish(req).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn failed_publish_still_prunes_the_cache() {
    let mut env = Env::new();
    env.manager = kubedal::node::volume::VolumeManager::new(
        env.dir.path().join("node"),
        env.cluster.clone(),
        env.mounter.clone(),
    )
    .unwrap()
    .with_content_cache_limit(0);
    let bucket = env.add_store(None, false);
    // The first file is fine and enters the cache, the second has a wrong digest.
    write(
        &bucket.join("jobs/1.json"),
        &format!(
            r#"{{"version": 1, "files": [
                {{"path": "a", "source": "data/a.txt", "sha256": "{HELLO}"}},
                {{"path": "b", "source": "data/a.txt", "sha256": "{WORLD}"}}
            ]}}"#
        ),
    );
    let err = env
        .manager
        .publish(request("vol-1", &env.target("p1"), "jobs/1.json"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::DataLoss);
    assert!(
        !env.manager
            .content_cache()
            .path("team/store", HELLO)
            .exists()
    );
}
