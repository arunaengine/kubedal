//! Startup reconcile after a restart of the node plugin.

mod common;

use common::{Env, tree_request, write};
use kubedal::node::cluster::EventKind;
use kubedal::node::mounter::Mounter;
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

fn add_pending_spool(env: &Env, id: &str, key: &str, content: &str) {
    let spool = env.manager.spool_dir(id);
    write(&spool.join("1.data"), content);
    write(&spool.join("1.json"), &format!(r#"{{"key": "{key}"}}"#));
}

#[tokio::test]
async fn failed_fuse_replay_keeps_the_record_and_the_spool() {
    let mut env = Env::new();
    let bucket = env.add_store(None, false);
    let target = env.target("p1");
    let req = tree_request("vol-1", &target, "data", TreeMode::Fuse, true);
    env.manager.publish(req.clone()).await.unwrap();
    let id = publish_id("vol-1", &target.to_string_lossy());
    add_pending_spool(&env, &id, "late.txt", "pending");

    // The driver restarts, and the storage cannot be reached during recovery.
    env.mounter.kill_fuse_sessions();
    env.restart();
    env.manager.reconcile().await.unwrap();
    env.cluster.set_unreachable("team", "store", true);
    assert!(env.manager.publish(req.clone()).await.is_err());
    let record = env.manager.state().load(&id).unwrap().unwrap();
    assert_eq!(record.phase, Phase::Published);
    assert!(env.manager.spool_dir(&id).join("1.json").exists());

    // A second restart during recovery keeps everything as well.
    env.restart();
    let report = env.manager.reconcile().await.unwrap();
    assert_eq!(report.discarded, 0);
    assert!(env.manager.spool_dir(&id).join("1.json").exists());

    // Storage works again: the spool is uploaded, then the volume is mounted.
    env.cluster.set_unreachable("team", "store", false);
    env.manager.publish(req).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(bucket.join("data/late.txt")).unwrap(),
        "pending"
    );
    assert!(env.mounter.has_fuse_session(&target).await);
}

#[tokio::test]
async fn pending_spool_files_are_never_discarded() {
    let env = Env::new();
    env.add_store(None, false);
    let target = env.target("p1");
    env.manager
        .publish(tree_request("vol-1", &target, "data", TreeMode::Fuse, true))
        .await
        .unwrap();
    let id = publish_id("vol-1", &target.to_string_lossy());
    add_pending_spool(&env, &id, "late.txt", "pending");
    // Pretend a publish was interrupted; reconcile would normally discard it.
    let mut record = env.manager.state().load(&id).unwrap().unwrap();
    record.phase = Phase::Preparing;
    env.manager.state().save(&id, &record).unwrap();
    let report = env.manager.reconcile().await.unwrap();
    assert_eq!(report.failed, 1);
    assert!(env.manager.spool_dir(&id).join("1.json").exists());
}
