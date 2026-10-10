//! Write-back of read-write cache volumes.

mod common;

use common::{Env, tree_request, write};
use kubedal::node::cluster::EventKind;
use kubedal::node::paths::publish_id;
use kubedal::node::state::Phase;
use kubedal::node::testing::FakeNode;
use kubedal::node::volume::TreeMode;
use tonic::Code;

#[tokio::test]
async fn new_and_changed_files_are_uploaded() {
    let env = Env::new();
    let bucket = env.add_store(None, false);
    let target = env.target("p1");
    env.manager
        .publish(tree_request(
            "vol-1",
            &target,
            "data",
            TreeMode::Cache,
            true,
        ))
        .await
        .unwrap();
    let id = publish_id("vol-1", &target.to_string_lossy());
    let data = env.manager.data_dir(&id);

    // The pod changes one file, adds a file and an empty folder.
    write(&data.join("sub/b.txt"), "changed by the pod");
    write(&data.join("results/out.txt"), "result");
    std::fs::create_dir_all(data.join("logs")).unwrap();
    // Someone else changes an unchanged file in storage meanwhile.
    write(&bucket.join("data/a.txt"), "changed remotely");

    env.manager.unpublish("vol-1", &target).await.unwrap();

    let read = |p: &str| std::fs::read_to_string(bucket.join(p)).unwrap();
    assert_eq!(read("data/sub/b.txt"), "changed by the pod");
    assert_eq!(read("data/results/out.txt"), "result");
    assert!(bucket.join("data/logs").is_dir());
    // Unchanged files are not uploaded again, so the remote change survives.
    assert_eq!(read("data/a.txt"), "changed remotely");

    assert!(!env.manager.volume_dir(&id).exists());
    assert!(env.manager.state().load(&id).unwrap().is_none());
    let events = env.cluster.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::Normal);
    assert_eq!(events[0].reason, "VolumeUploaded");
}

#[tokio::test]
async fn failed_upload_keeps_data_and_is_retried() {
    let env = Env::new();
    let bucket = env.add_store(None, false);
    let target = env.target("p1");
    let req = tree_request("vol-1", &target, "data", TreeMode::Cache, true);
    env.manager.publish(req.clone()).await.unwrap();
    let id = publish_id("vol-1", &target.to_string_lossy());
    write(&env.manager.data_dir(&id).join("out.txt"), "precious");

    // The DataNode is gone, so the upload fails.
    env.cluster.remove_node("team", "store");
    let err = env.manager.unpublish("vol-1", &target).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    assert!(env.manager.data_dir(&id).join("out.txt").exists());
    let record = env.manager.state().load(&id).unwrap().unwrap();
    assert_eq!(record.phase, Phase::Unpublishing);
    let events = env.cluster.events();
    assert_eq!(events.last().unwrap().kind, EventKind::Warning);
    assert_eq!(events.last().unwrap().reason, "VolumeUploadFailed");

    // A new publish must not reuse or delete the pending data.
    let err = env.manager.publish(req).await.unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition);

    // The DataNode is back, kubelet retries the unpublish.
    env.cluster.add_node(
        "team",
        "store",
        FakeNode {
            root: bucket.clone(),
            limit: None,
            read_only: false,
        },
    );
    env.manager.unpublish("vol-1", &target).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(bucket.join("data/out.txt")).unwrap(),
        "precious"
    );
    assert!(!env.manager.volume_dir(&id).exists());
}

#[tokio::test]
async fn upload_survives_a_restart_between_retries() {
    let mut env = Env::new();
    let bucket = env.add_store(None, false);
    let target = env.target("p1");
    env.manager
        .publish(tree_request(
            "vol-1",
            &target,
            "data",
            TreeMode::Cache,
            true,
        ))
        .await
        .unwrap();
    let id = publish_id("vol-1", &target.to_string_lossy());
    write(&env.manager.data_dir(&id).join("out.txt"), "precious");
    env.cluster.remove_node("team", "store");
    assert!(env.manager.unpublish("vol-1", &target).await.is_err());

    env.restart();
    env.cluster.add_node(
        "team",
        "store",
        FakeNode {
            root: bucket.clone(),
            limit: None,
            read_only: false,
        },
    );
    env.manager.unpublish("vol-1", &target).await.unwrap();
    assert!(bucket.join("data/out.txt").exists());
}

#[tokio::test]
async fn read_only_volume_uploads_nothing() {
    let env = Env::new();
    let bucket = env.add_store(None, false);
    let target = env.target("p1");
    env.manager
        .publish(tree_request(
            "vol-1",
            &target,
            "data",
            TreeMode::Cache,
            false,
        ))
        .await
        .unwrap();
    let id = publish_id("vol-1", &target.to_string_lossy());
    write(&env.manager.data_dir(&id).join("sneaky.txt"), "x");
    env.manager.unpublish("vol-1", &target).await.unwrap();
    assert!(!bucket.join("data/sneaky.txt").exists());
    assert!(env.cluster.events().is_empty());
}

#[tokio::test]
async fn fuse_spool_files_are_uploaded_on_unpublish() {
    let env = Env::new();
    let bucket = env.add_store(None, false);
    let target = env.target("p1");
    env.manager
        .publish(tree_request("vol-1", &target, "data", TreeMode::Fuse, true))
        .await
        .unwrap();
    let id = publish_id("vol-1", &target.to_string_lossy());
    // Content that the FUSE file system could not upload, for example before
    // the driver restarted.
    let spool = env.manager.spool_dir(&id);
    write(&spool.join("1.data"), "from the spool");
    write(&spool.join("1.json"), r#"{"key": "out/late.txt"}"#);

    env.manager.unpublish("vol-1", &target).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(bucket.join("data/out/late.txt")).unwrap(),
        "from the spool"
    );
    assert!(!env.manager.volume_dir(&id).exists());
}
