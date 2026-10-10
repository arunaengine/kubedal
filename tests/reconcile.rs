//! Startup reconcile after a restart of the node plugin.

mod common;

use common::{Env, tree_request, write};
use kubedal::node::cluster::EventKind;
use kubedal::node::paths::publish_id;
use kubedal::node::state::Phase;
use kubedal::node::testing::FakeMount;
use kubedal::node::volume::TreeMode;

#[tokio::test]
async fn stale_fuse_mount_is_unmounted_and_reported() {
    let mut env = Env::new();
    env.add_store(None, false);
    let target = env.target("p1");
    env.manager
        .publish(tree_request(
            "vol-1",
            &target,
            "data",
            TreeMode::Fuse,
            false,
        ))
        .await
        .unwrap();

    env.mounter.kill_fuse_sessions();
    env.restart();
    let report = env.manager.reconcile().await.unwrap();
    assert_eq!(report.stale_fuse_unmounted, 1);
    assert!(env.mounter.get(&target).is_none());
    let events = env.cluster.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::Warning);
    assert_eq!(events[0].reason, "FuseMountLost");

    // The record stays, so the coming unpublish call works as usual.
    env.manager.unpublish("vol-1", &target).await.unwrap();
}

#[tokio::test]
async fn cache_mounts_are_kept() {
    let mut env = Env::new();
    env.add_store(None, false);
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
    env.restart();
    let report = env.manager.reconcile().await.unwrap();
    assert_eq!(report.kept, 1);
    assert!(matches!(
        env.mounter.get(&target),
        Some(FakeMount::Bind { .. })
    ));
}

#[tokio::test]
async fn interrupted_publish_is_cleaned_up() {
    let mut env = Env::new();
    env.add_store(None, false);
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
    // Pretend the plugin crashed in the middle of the download.
    let mut record = env.manager.state().load(&id).unwrap().unwrap();
    record.phase = Phase::Preparing;
    env.manager.state().save(&id, &record).unwrap();

    env.restart();
    let report = env.manager.reconcile().await.unwrap();
    assert_eq!(report.discarded, 1);
    assert!(!env.manager.volume_dir(&id).exists());
    assert!(env.manager.state().load(&id).unwrap().is_none());
    assert!(env.mounter.get(&target).is_none());
}

#[tokio::test]
async fn data_waiting_for_upload_is_never_removed() {
    let mut env = Env::new();
    env.add_store(None, false);
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
    // The pod and its target directory vanished without an unpublish call.
    env.mounter.clear();
    std::fs::remove_dir_all(&target).unwrap();

    env.restart();
    let report = env.manager.reconcile().await.unwrap();
    assert_eq!(report.pending_upload, 1);
    assert!(env.manager.data_dir(&id).join("out.txt").exists());
}
