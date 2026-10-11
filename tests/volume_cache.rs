//! Cache and FUSE volume tests with a local file system backend.

mod common;

use common::{Env, pod, store, tree_request};
use kubedal::node::mounter::Mounter;
use kubedal::node::paths::publish_id;
use kubedal::node::state::Phase;
use kubedal::node::testing::FakeMount;
use kubedal::node::volume::TreeMode;
use tonic::Code;

#[tokio::test]
async fn cache_read_only_publish_and_unpublish() {
    let env = Env::new();
    env.add_store(None, false);
    let target = env.target("p1");
    let req = tree_request("vol-1", &target, "/data", TreeMode::Cache, false);

    env.manager.publish(req.clone()).await.unwrap();

    let id = publish_id("vol-1", &target.to_string_lossy());
    let data = env.manager.data_dir(&id);
    assert_eq!(
        std::fs::read_to_string(data.join("a.txt")).unwrap(),
        "hello"
    );
    assert_eq!(
        std::fs::read_to_string(data.join("sub/b.txt")).unwrap(),
        "world"
    );
    assert!(data.join("empty").is_dir());
    assert!(!data.join("c.txt").exists());
    assert_eq!(
        env.mounter.get(&target),
        Some(FakeMount::Bind {
            source: data.clone(),
            read_only: true
        })
    );
    let record = env.manager.state().load(&id).unwrap().unwrap();
    assert_eq!(record.phase, Phase::Published);
    assert!(record.upload.is_none());

    // A second publish call with the same arguments succeeds without work.
    env.manager.publish(req).await.unwrap();

    env.manager.unpublish("vol-1", &target).await.unwrap();
    assert!(env.mounter.get(&target).is_none());
    assert!(!env.manager.volume_dir(&id).exists());
    assert!(env.manager.state().load(&id).unwrap().is_none());
    assert!(!target.exists());

    // Unpublish is idempotent.
    env.manager.unpublish("vol-1", &target).await.unwrap();
}

#[tokio::test]
async fn csi_readonly_flag_overrides_writable_mode() {
    let env = Env::new();
    env.add_store(None, false);
    let target = env.target("p1");
    let mut req = tree_request("vol-1", &target, "data", TreeMode::Cache, true);
    req.read_only = true;
    env.manager.publish(req).await.unwrap();
    assert!(matches!(
        env.mounter.get(&target),
        Some(FakeMount::Bind {
            read_only: true,
            ..
        })
    ));
}

#[tokio::test]
async fn limit_is_enforced_and_nothing_is_left_behind() {
    let env = Env::new();
    env.add_store(Some(6), false);
    let target = env.target("p1");
    let err = env
        .manager
        .publish(tree_request(
            "vol-1",
            &target,
            "data",
            TreeMode::Cache,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::ResourceExhausted);

    let id = publish_id("vol-1", &target.to_string_lossy());
    assert!(!env.manager.volume_dir(&id).exists());
    assert!(env.manager.state().load(&id).unwrap().is_none());
    assert!(env.mounter.get(&target).is_none());

    // 10 bytes fit into a limit of 10 bytes.
    let env = Env::new();
    env.add_store(Some(10), false);
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
}

#[tokio::test]
async fn writable_volume_on_read_only_data_node_is_denied() {
    let env = Env::new();
    env.add_store(None, true);
    let target = env.target("p1");
    let err = env
        .manager
        .publish(tree_request(
            "vol-1",
            &target,
            "data",
            TreeMode::Cache,
            true,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);

    // Reading is still fine.
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
}

#[tokio::test]
async fn pods_in_other_namespaces_need_permission() {
    let env = Env::new();
    env.add_store(None, false);
    let target = env.target("p1");
    let mut req = tree_request("vol-1", &target, "data", TreeMode::Cache, false);
    req.pod = pod("intruder");
    let err = env.manager.publish(req.clone()).await.unwrap_err();
    assert_eq!(err.code(), Code::PermissionDenied);

    env.cluster.allow("intruder", store());
    env.manager.publish(req).await.unwrap();
}

#[tokio::test]
async fn missing_data_node_is_not_found() {
    let env = Env::new();
    let target = env.target("p1");
    let err = env
        .manager
        .publish(tree_request(
            "vol-1",
            &target,
            "data",
            TreeMode::Cache,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn path_escaping_the_data_node_is_rejected() {
    let env = Env::new();
    env.add_store(None, false);
    let target = env.target("p1");
    let err = env
        .manager
        .publish(tree_request(
            "vol-1",
            &target,
            "../x",
            TreeMode::Cache,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn cache_volume_is_bound_again_after_reboot() {
    let env = Env::new();
    env.add_store(None, false);
    let target = env.target("p1");
    let req = tree_request("vol-1", &target, "data", TreeMode::Cache, true);
    env.manager.publish(req.clone()).await.unwrap();
    let id = publish_id("vol-1", &target.to_string_lossy());
    common::write(&env.manager.data_dir(&id).join("new.txt"), "kept");

    // After a reboot all mounts are gone, the data directory is still there.
    env.mounter.clear();
    env.manager.publish(req).await.unwrap();
    assert!(matches!(
        env.mounter.get(&target),
        Some(FakeMount::Bind {
            read_only: false,
            ..
        })
    ));
    assert!(env.manager.data_dir(&id).join("new.txt").exists());
}

#[tokio::test]
async fn fuse_volume_is_mounted_again_after_restart() {
    let mut env = Env::new();
    env.add_store(None, false);
    let target = env.target("p1");
    let req = tree_request("vol-1", &target, "data", TreeMode::Fuse, false);
    env.manager.publish(req.clone()).await.unwrap();
    assert_eq!(
        env.mounter.get(&target),
        Some(FakeMount::Fuse { read_only: true })
    );

    env.mounter.kill_fuse_sessions();
    env.restart();
    env.manager.publish(req).await.unwrap();
    assert!(env.mounter.has_fuse_session(&target).await);

    env.manager.unpublish("vol-1", &target).await.unwrap();
    assert!(env.mounter.get(&target).is_none());
}

#[tokio::test]
async fn publish_with_other_settings_fails() {
    let env = Env::new();
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
    let err = env
        .manager
        .publish(tree_request(
            "vol-1",
            &target,
            "data",
            TreeMode::Fuse,
            false,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::AlreadyExists);
}

#[tokio::test]
async fn paths_that_storage_would_change_never_mount() {
    let env = Env::new();
    env.add_store(None, false);
    for (path, mode) in [
        ("//", TreeMode::Fuse),
        ("data//sub", TreeMode::Fuse),
        ("data/./sub", TreeMode::Cache),
    ] {
        let target = env.target("p1");
        let err = env
            .manager
            .publish(tree_request("vol-1", &target, path, mode, true))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::InvalidArgument, "{path:?}");
        assert!(env.mounter.get(&target).is_none(), "{path:?}");
    }
}

#[tokio::test]
async fn publish_with_another_source_fails() {
    let env = Env::new();
    env.add_store(None, false);
    let target = env.target("p1");
    let request = |path: &str, uid: &str| {
        let mut req = tree_request("vol-1", &target, path, TreeMode::Cache, false);
        req.data_pod_uid = Some(uid.into());
        req
    };
    env.manager.publish(request("data", "dp-1")).await.unwrap();
    // The same request again is fine.
    env.manager.publish(request("data", "dp-1")).await.unwrap();
    // Another path, or another DataPod with the same path, is refused.
    let err = env
        .manager
        .publish(request("other", "dp-1"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::AlreadyExists);
    let err = env
        .manager
        .publish(request("data", "dp-2"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::AlreadyExists);
}

#[tokio::test]
async fn unpublish_of_unknown_volume_succeeds() {
    let env = Env::new();
    let target = env.target("p1");
    std::fs::create_dir_all(&target).unwrap();
    env.manager.unpublish("nothing", &target).await.unwrap();
    assert!(!target.exists());
}

#[tokio::test]
async fn cancelled_publish_still_finishes_its_work() {
    let env = Env::new();
    env.add_store(None, false);
    let target = env.target("p1");
    let manager = std::sync::Arc::new(env.manager);
    let req = tree_request("vol-1", &target, "data", TreeMode::Cache, false);

    // The caller gives up at once, like a gRPC call that kubelet cancels.
    let _ = tokio::time::timeout(
        std::time::Duration::ZERO,
        manager.publish_detached(req.clone()),
    )
    .await;

    // The work goes on to the end, so the volume ends up published.
    let id = publish_id("vol-1", &target.to_string_lossy());
    for _ in 0..200 {
        if manager
            .state()
            .load(&id)
            .unwrap()
            .is_some_and(|r| r.phase == Phase::Published)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let record = manager.state().load(&id).unwrap().unwrap();
    assert_eq!(record.phase, Phase::Published);
    manager.publish_detached(req).await.unwrap();
}
