use super::controller::{Context, Error};
use crate::resource::crd::{DataNode, DataPod, MountAccess};
use crate::resource::crd::{DataPodSpec, DataReplicaSet, DataReplicaSetStatus, Ref};
use k8s_openapi::api::core::v1::{
    Container, EnvVar, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PersistentVolumeClaimVolumeSource, Pod, PodSpec, Volume, VolumeMount,
    VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions};
use kube::{
    Resource,
    api::{Api, ResourceExt},
    runtime::{
        controller::Action,
        events::{Event, EventType},
        finalizer::{Event as Finalizer, finalizer},
    },
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tracing::*;

const DATASOURCE_FINALIZER: &str = "kubedal.arunaengine.org/datareplicaset";

#[instrument(skip(ctx, res), fields(trace_id))]
pub async fn reconcile_drs(res: Arc<DataReplicaSet>, ctx: Arc<Context>) -> Result<Action, Error> {
    let ns = res
        .namespace()
        .ok_or_else(|| Error::ReconcilerError(format!("{} has no namespace", res.name_any())))?;
    let dn: Api<DataReplicaSet> = Api::namespaced(ctx.client.clone(), &ns);

    info!("Reconciling Document \"{}\" in {}", res.name_any(), ns);
    finalizer(&dn, DATASOURCE_FINALIZER, res, |event| async {
        match event {
            Finalizer::Apply(doc) => doc.reconcile(ctx.clone()).await,
            Finalizer::Cleanup(doc) => doc.cleanup(ctx.clone()).await,
        }
    })
    .await
    .map_err(|e| Error::FinalizerError(Box::new(e)))
}

pub fn error_policy_drs(doc: Arc<DataReplicaSet>, error: &Error, _ctx: Arc<Context>) -> Action {
    warn!("reconcile failed: {:?}, {:?}", error, doc);
    Action::requeue(Duration::from_secs(5 * 60))
}

impl DataReplicaSet {
    // Reconcile (for non-finalizer related changes)
    async fn reconcile(&self, ctx: Arc<Context>) -> Result<Action, Error> {
        // Eval DataReplicaSet
        let client = ctx.client.clone();
        let drs_name = self.name_any();
        let drs_ns = self.namespace().ok_or_else(|| {
            Error::ReconcilerError(format!("DataReplicaSet {} is missing namespace", drs_name))
        })?; // .unwrap_or_default? Or match and patch with 'default'?
        let drs_replicas = self.spec.replicas as usize;
        // Replicas are found by owner, not by the selector: after a selector
        // change the existing replicas must still be found.
        let dp_api: Api<DataPod> = Api::namespaced(client.clone(), &drs_ns);
        let drs_data_pods = dp_api.list(&ListParams::default()).await?;

        let dn_api: Api<DataNode> = Api::namespaced(client.clone(), &drs_ns);
        let pvc_api = Api::<PersistentVolumeClaim>::namespaced(client.clone(), &drs_ns);
        let pod_api = Api::<Pod>::namespaced(client.clone(), &drs_ns);

        // Zero replicas need no source at all: remove every replica and the sync
        // pod, even if the source is gone or invalid. Replicas are found by owner,
        // so also those that no longer match a changed selector go.
        if drs_replicas == 0 {
            let uid = self.metadata.uid.clone().unwrap_or_default();
            let all_pvcs = pvc_api.list(&ListParams::default()).await?;
            let (pvcs, data_pods) =
                zero_replica_cleanup(self, &uid, &drs_data_pods.items, &all_pvcs.items);
            for name in pvcs {
                delete_owned(&pvc_api, &name, self).await?;
            }
            for name in data_pods {
                delete_owned(&dp_api, &name, self).await?;
            }
            delete_owned(&pod_api, &sync_pod_name(self), self).await?;
            self.patch_status(&ctx, true).await?;
            return Ok(Action::requeue(Duration::from_secs(5 * 60)));
        }

        // Fetch source DataPod and its DataNode
        // The node plugin only mounts DataPods of the pod's own namespace, so the
        // source must be in the namespace of the set.
        if let Some(message) = source_namespace_error(&drs_ns, &self.spec.source_pod) {
            warn!("DataReplicaSet {drs_name}: {message}");
            // An old sync pod must not keep running for an invalid source.
            delete_owned(&pod_api, &sync_pod_name(self), self).await?;
            // The status comes first; the Event is only a hint and may fail.
            self.patch_status(&ctx, false).await?;
            if let Err(err) = ctx
                .recorder
                .publish(
                    &Event {
                        type_: EventType::Warning,
                        reason: "InvalidSource".into(),
                        note: Some(message),
                        action: "Reconcile".into(),
                        secondary: None,
                    },
                    &self.object_ref(&()),
                )
                .await
            {
                warn!("DataReplicaSet {drs_name}: cannot record event: {err}");
            }
            return Ok(Action::requeue(Duration::from_secs(5 * 60)));
        }
        let source_data_pod = dp_api.get(&self.spec.source_pod.name).await?;
        // The DataPod reference names the DataNode, also in another namespace
        // (where no owner reference exists).
        let source_ref = source_node_ref(&source_data_pod, &drs_ns)?;
        let source_data_node = Api::<DataNode>::namespaced(
            client.clone(),
            source_ref.namespace.as_deref().unwrap_or(&drs_ns),
        )
        .get(&source_ref.name)
        .await?;

        // Create missing replica DataPods on DataNodes that hold no copy yet.
        //TODO: Scale down when there are more replicas than requested.
        //TODO: Update the sync pod when the set of replicas changes.
        let drs_uid = self.metadata.uid.clone().ok_or_else(|| {
            Error::ReconcilerError(format!("DataReplicaSet {drs_name} has no uid"))
        })?;
        let ours = owned_replicas(&drs_uid, drs_data_pods.items);
        let taken: Vec<String> = ours.iter().map(|dp| dp.name_any()).collect();
        let all_nodes = dn_api.list(&ListParams::default()).await?.items;

        // A read-only DataNode cannot hold a replica. Such replicas do not count;
        // replacements are created first, and the old ones go only after that.
        let (on_read_only, healthy): (Vec<DataPod>, Vec<DataPod>) = ours
            .into_iter()
            .partition(|dp| on_read_only_node(dp, &all_nodes, &drs_ns));
        // Scale down: replicas above the desired count go, the highest index first.
        let (mut replicas, excess) = split_excess(&drs_uid, healthy, drs_replicas);
        for dp in &excess {
            info!("Removing replica {} above the desired count", dp.name_any());
            delete_owned(&pvc_api, &pvc_name(self, dp), self).await?;
            delete_owned(&dp_api, &dp.name_any(), self).await?;
        }
        let need = drs_replicas.saturating_sub(replicas.len());
        if need > 0 {
            let used: Vec<String> = replicas
                .iter()
                .filter_map(|dp| dp.spec.data_node_ref.as_ref().map(|r| r.name.clone()))
                .collect();
            let mut available_data_nodes = all_nodes
                .into_iter()
                .filter(|node| node.metadata.uid != source_data_node.metadata.uid)
                .filter(|node| !node.spec.read_only)
                .filter(|node| !used.contains(&node.name_any()))
                .collect::<Vec<_>>();
            for index in free_replica_indexes(&drs_uid, &taken, need) {
                let Some(replica_data_node) = available_data_nodes.pop() else {
                    warn!("DataReplicaSet {drs_name}: no free DataNode for replica {index}");
                    break;
                };
                replicas
                    .push(create_replica_data_pod(index, self, &replica_data_node, &dp_api).await?);
            }
        }
        // The replacements count as synced once the sync pod runs with exactly
        // the current replicas.
        // The image is part of the hash, so a new image replaces the sync pod.
        let members = members_hash(
            &format!(
                "{}|{}",
                ctx.sync_image,
                member_id(&source_data_pod, &pvc_name(self, &source_data_pod))
            ),
            replicas.iter().map(|dp| member_id(dp, &pvc_name(self, dp))),
        );
        let synced = pod_api
            .get_opt(&sync_pod_name(self))
            .await?
            .is_some_and(|pod| sync_pod_is_running(&pod, &members));
        if old_replicas_can_go(replicas.len(), drs_replicas, synced) {
            for dp in &on_read_only {
                info!(
                    "Removing replica {} from a read-only DataNode",
                    dp.name_any()
                );
                delete_owned(&pvc_api, &pvc_name(self, dp), self).await?;
                delete_owned(&dp_api, &dp.name_any(), self).await?;
            }
        } else if !on_read_only.is_empty() {
            warn!(
                "DataReplicaSet {drs_name}: replicas on read-only DataNodes wait for a replacement"
            );
        }
        replicas.sort_by_key(|dp| dp.name_any());
        if replicas.is_empty() {
            // No replica: no sync pod either, also not an old one. With zero
            // replicas wanted, that is the desired state.
            delete_owned(&pod_api, &sync_pod_name(self), self).await?;
            self.patch_status(&ctx, drs_replicas == 0).await?;
            return Ok(Action::requeue(Duration::from_secs(5 * 60)));
        }

        // Make sure every PVC exists, also when an earlier reconcile failed halfway.
        let source_pvc = ensure_replica_pvc(
            &source_data_pod,
            &source_ref,
            self,
            MountAccess::FuseReadOnly,
            &pvc_api,
            &ctx,
        )
        .await?;
        let mut replica_resources = vec![];
        for replica in replicas {
            let node_ref = replica.spec.data_node_ref.clone().ok_or_else(|| {
                Error::ReconcilerError(format!(
                    "replica DataPod {} has no DataNode",
                    replica.name_any()
                ))
            })?;
            let pvc = ensure_replica_pvc(
                &replica,
                &node_ref,
                self,
                MountAccess::FuseReadWrite,
                &pvc_api,
                &ctx,
            )
            .await?;
            replica_resources.push((replica, pvc));
        }
        let replica_count = replica_resources.len();

        // Make sure the sync pod exists and mounts exactly the current PVCs.
        //TODO: Interval poll source mount for changes and sync to replica mounts
        let members = members_hash(
            &format!(
                "{}|{}",
                ctx.sync_image,
                member_id(&source_data_pod, &source_pvc.name_any())
            ),
            replica_resources
                .iter()
                .map(|(dp, pvc)| member_id(dp, &pvc.name_any())),
        );
        let pod = match pod_api.get_opt(&sync_pod_name(self)).await? {
            Some(pod) if sync_pod_is_current(&pod, &members) => pod,
            Some(pod) => {
                // The replicas changed (the pod mounts the wrong PVCs), or the pod
                // ended (evicted, crashed). Delete it; a later reconcile creates it
                // again.
                info!("Replacing the sync pod of replica set {drs_name}");
                delete_owned(&pod_api, &pod.name_any(), self).await?;
                self.patch_status(&ctx, false).await?;
                return Ok(Action::requeue(Duration::from_secs(10)));
            }
            None => {
                let pod = create_replica_pod(
                    self,
                    &ctx.sync_image,
                    &source_pvc,
                    replica_resources,
                    &members,
                    &pod_api,
                )
                .await?;
                info!("Created replica sync pod: {}", pod.name_any());
                pod
            }
        };

        let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
        self.patch_status(
            &ctx,
            replica_set_available(drs_replicas, replica_count, phase),
        )
        .await?;
        Ok(Action::requeue(Duration::from_secs(60)))
    }

    /// Record whether all requested replicas exist.
    async fn patch_status(&self, ctx: &Context, available: bool) -> Result<(), Error> {
        let ns = self
            .namespace()
            .ok_or_else(|| Error::ReconcilerError("DataReplicaSet has no namespace".into()))?;
        let api: Api<DataReplicaSet> = Api::namespaced(ctx.client.clone(), &ns);
        let patch = Patch::Merge(json!({ "status": DataReplicaSetStatus { available } }));
        api.patch_status(&self.name_any(), &PatchParams::default(), &patch)
            .await?;
        Ok(())
    }

    // Finalizer cleanup (the object was deleted, ensure nothing is orphaned)
    async fn cleanup(&self, ctx: Arc<Context>) -> Result<Action, Error> {
        // DataPods, PVCs and the sync pod carry an owner reference to this
        // DataReplicaSet, so Kubernetes garbage collection deletes them.

        let oref = self.object_ref(&());
        ctx.recorder
            .publish(
                &Event {
                    type_: EventType::Normal,
                    reason: "DeleteRequested".into(),
                    note: Some(format!("Delete `{}`", self.name_any())),
                    action: "Deleting".into(),
                    secondary: None,
                },
                &oref,
            )
            .await
            .map_err(Error::KubeError)?;
        Ok(Action::await_change())
    }
}

async fn create_replica_data_pod(
    index: usize,
    data_replica_set: &DataReplicaSet,
    data_node: &DataNode,
    data_pod_api: &Api<DataPod>,
) -> Result<DataPod, Error> {
    let drs_name = data_replica_set.name_any();
    let drs_uid = data_replica_set.metadata.uid.clone().ok_or_else(|| {
        Error::ReconcilerError(format!("DataReplicaSet {} is missing uid", drs_name))
    })?;
    let drs_ns = data_replica_set.namespace().ok_or_else(|| {
        Error::ReconcilerError(format!("DataReplicaSet {} is missing namespace", drs_name))
    })?;
    let drs_owner = data_replica_set.owner_ref(&()).ok_or_else(|| {
        Error::ReconcilerError(format!(
            "Failed to create owner ref from DataNode: {}",
            drs_name
        ))
    })?;

    // Create DataPod
    let replica_pod = DataPod {
        metadata: ObjectMeta {
            name: Some(replica_name(&drs_uid, index)),
            namespace: Some(drs_ns.clone()),
            owner_references: Some(vec![drs_owner.clone()]),
            // Without the template labels the selector never counts the replicas,
            // and every reconcile would create new ones.
            labels: Some(data_replica_set.spec.template.metadata.labels.clone()),
            ..Default::default()
        },
        spec: DataPodSpec {
            path: Some(replica_path(&drs_uid, index)),
            data_node_ref: Some(Ref {
                name: data_node.name_any(),
                namespace: data_node.namespace(),
            }),
            data_node_selector: None,
            request: data_replica_set.spec.template.spec.request.clone(),
        },
        status: None,
    };

    Ok(data_pod_api
        .create(&PostParams::default(), &replica_pod)
        .await?)
}

/// Get the PVC of a DataPod in this set, or create it if it is missing.
async fn ensure_replica_pvc(
    data_pod: &DataPod,
    data_node: &Ref,
    data_replica_set: &DataReplicaSet,
    mount_access: MountAccess,
    pvc_api: &Api<PersistentVolumeClaim>,
    ctx: &Context,
) -> Result<PersistentVolumeClaim, Error> {
    let storage_class = ctx.storage_class.as_str();
    let name = pvc_name(data_replica_set, data_pod);
    if let Some(pvc) = pvc_api.get_opt(&name).await? {
        // A claim with this name that is not ours must never become a sync
        // source or target.
        if let Some(problem) = claim_mismatch(&pvc, data_replica_set, data_pod, storage_class) {
            warn!("DataReplicaSet {}: {problem}", data_replica_set.name_any());
            data_replica_set.patch_status(ctx, false).await?;
            return Err(Error::ReconcilerError(problem));
        }
        return Ok(pvc);
    }

    let replica_owner = data_replica_set.owner_ref(&()).ok_or_else(|| {
        Error::ReconcilerError(format!(
            "Failed to create owner ref from DataReplicaSet: {}",
            data_replica_set.name_any()
        ))
    })?;

    let pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: data_replica_set.namespace(),
            owner_references: Some(vec![replica_owner.clone()]),
            annotations: Some(
                vec![
                    (
                        "kubedal.arunaengine.org/data-node-name".to_string(),
                        data_node.name.clone(),
                    ),
                    (
                        "kubedal.arunaengine.org/data-node-namespace".to_string(),
                        data_node
                            .namespace
                            .clone()
                            .or_else(|| data_pod.metadata.namespace.clone())
                            .unwrap_or_default(),
                    ),
                    (
                        "kubedal.arunaengine.org/data-pod-name".to_string(),
                        data_pod.name_any(),
                    ),
                    (
                        "kubedal.arunaengine.org/data-pod-namespace".to_string(),
                        data_pod.metadata.namespace.clone().unwrap_or_default(),
                    ),
                    (
                        "kubedal.arunaengine.org/mount".to_string(),
                        mount_access.to_string(),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec![String::from("ReadWriteOnce")]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(
                    vec![("storage".to_string(), Quantity("1Gi".to_string()))]
                        .into_iter()
                        .collect(),
                ),
                ..Default::default()
            }),
            storage_class_name: Some(storage_class.to_string()),
            ..Default::default()
        }),
        status: None,
    };

    Ok(pvc_api.create(&PostParams::default(), &pvc).await?)
}

async fn create_replica_pod(
    data_replica_set: &DataReplicaSet,
    image: &str,
    source_pvc: &PersistentVolumeClaim,
    replica_resources: Vec<(DataPod, PersistentVolumeClaim)>,
    members: &str,
    pod_api: &Api<Pod>,
) -> Result<Pod, Error> {
    let replica_owner = data_replica_set.owner_ref(&()).ok_or_else(|| {
        Error::ReconcilerError(format!(
            "Failed to create owner ref from DataReplicaSet: {}",
            data_replica_set.name_any()
        ))
    })?;

    // Fixed mount paths: a DataPod path like `/` or `/usr` must not hide the
    // files of the sync image.
    let source_mount = "/sync/source".to_string();
    let src_pvcsc = PersistentVolumeClaimVolumeSource {
        claim_name: source_pvc.name_any(),
        read_only: Some(true),
    };
    let mut volumes: Vec<Volume> = vec![Volume {
        name: "source".to_string(),
        persistent_volume_claim: Some(src_pvcsc),
        ..Default::default()
    }];
    let mut volume_mounts: Vec<VolumeMount> = vec![VolumeMount {
        name: "source".to_string(),
        mount_path: source_mount.clone(),
        ..Default::default()
    }];

    let mut replica_paths = vec![];
    for (i, (_, replica_pvc)) in replica_resources.iter().enumerate() {
        let replica_path = format!("/sync/replica-{i}");
        let replica_pvcsc = PersistentVolumeClaimVolumeSource {
            claim_name: replica_pvc.name_any(),
            read_only: Some(false),
        };
        volumes.push(Volume {
            name: format!("replica-{}", i),
            persistent_volume_claim: Some(replica_pvcsc),
            ..Default::default()
        });
        volume_mounts.push(VolumeMount {
            name: format!("replica-{}", i),
            mount_path: replica_path.clone(),
            ..Default::default()
        });

        replica_paths.push(replica_path);
    }

    // The kubedal image itself copies the data (`kubedal replicate`).
    let container = Container {
        name: "sync-replica".to_string(),
        image: Some(image.to_string()),
        image_pull_policy: Some("IfNotPresent".to_string()),
        args: Some(vec!["replicate".to_string()]),
        env: Some(vec![
            EnvVar {
                name: "source".to_string(),
                value: Some(source_mount),
                value_from: None,
            },
            EnvVar {
                name: "sleep_duration".to_string(),
                value: Some("1".to_string()),
                value_from: None,
            },
            EnvVar {
                name: "replicas".to_string(),
                value: Some(replica_paths.join(";")),
                value_from: None,
            },
        ]),
        volume_mounts: Some(volume_mounts),
        ..Default::default()
    };

    let pod = Pod {
        metadata: ObjectMeta {
            name: Some(sync_pod_name(data_replica_set)),
            namespace: data_replica_set.namespace(),
            owner_references: Some(vec![replica_owner]),
            labels: None, //TODO
            annotations: Some(
                [(MEMBERS_ANNOTATION.to_string(), members.to_string())]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![container],
            volumes: Some(volumes),
            ..Default::default()
        }),
        status: None,
    };

    Ok(pod_api.create(&PostParams::default(), &pod).await?)
}

/// Name of the PVC for a DataPod of this set. Fixed names let a later reconcile
/// find the PVCs and create the ones that are missing.
fn pvc_name(data_replica_set: &DataReplicaSet, data_pod: &DataPod) -> String {
    format!("{}-{}", data_replica_set.name_any(), data_pod.name_any())
}

/// Name of the replica DataPod with this index.
fn replica_name(drs_uid: &str, index: usize) -> String {
    format!("{drs_uid}-replica-{index}")
}

/// Folder of a replica in its DataNode. It holds the set UID, so two sets that
/// use the same DataNode never write into the same folder.
fn replica_path(drs_uid: &str, index: usize) -> String {
    format!("/{drs_uid}/replica-{index}")
}

/// The `count` smallest replica indexes whose names are not taken. Names, not
/// counts, decide this, so a deleted replica in the middle is created again.
fn free_replica_indexes(drs_uid: &str, taken: &[String], count: usize) -> Vec<usize> {
    (0..)
        .filter(|i| !taken.contains(&replica_name(drs_uid, *i)))
        .take(count)
        .collect()
}

/// Index of a replica DataPod of this set, from its name.
fn replica_index(drs_uid: &str, name: &str) -> Option<usize> {
    name.strip_prefix(&format!("{drs_uid}-replica-"))?
        .parse()
        .ok()
}

/// Keep the `desired` replicas with the lowest indexes; return them and the rest.
fn split_excess(
    drs_uid: &str,
    mut replicas: Vec<DataPod>,
    desired: usize,
) -> (Vec<DataPod>, Vec<DataPod>) {
    replicas.sort_by_key(|dp| replica_index(drs_uid, &dp.name_any()).unwrap_or(usize::MAX));
    let excess = replicas.split_off(desired.min(replicas.len()));
    (replicas, excess)
}

/// Replicas on read-only DataNodes are only removed once enough replacements
/// exist and the sync pod runs with them, so the set never has fewer copies.
/// With zero replicas wanted, nothing needs syncing.
fn old_replicas_can_go(healthy: usize, desired: usize, synced: bool) -> bool {
    healthy >= desired && (synced || desired == 0)
}

/// True if the sync pod runs and mounts exactly `members`.
fn sync_pod_is_running(pod: &Pod, members: &str) -> bool {
    let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
    phase == Some("Running") && sync_pod_is_current(pod, members)
}

/// Annotation on the sync pod with a hash of its image and the PVCs it mounts.
const MEMBERS_ANNOTATION: &str = "kubedal.arunaengine.org/replica-members";

/// Hash of the source PVC and the sorted replica PVC names.
fn members_hash(source: &str, replicas: impl Iterator<Item = String>) -> String {
    use sha2::{Digest, Sha256};
    let mut names: Vec<String> = replicas.collect();
    names.sort();
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    for name in names {
        hasher.update([0u8]);
        hasher.update(name.as_bytes());
    }
    crate::node::paths::to_hex(&hasher.finalize())[..16].to_string()
}

/// Delete an object. A missing object is fine; every other error (like a
/// missing RBAC permission) is returned, not ignored.
/// The objects in `items` that have an owner reference with `uid`.
fn owned_by<'a, K: Resource>(items: &'a [K], uid: &str) -> Vec<&'a K> {
    items
        .iter()
        .filter(|item| is_owned_by(*item, uid))
        .collect()
}

/// True if `item` has an owner reference with `uid` (never for an empty uid).
fn is_owned_by<K: Resource>(item: &K, uid: &str) -> bool {
    !uid.is_empty() && item.owner_references().iter().any(|o| o.uid == uid)
}

/// Why an existing claim with the expected name does not belong to this set
/// and DataPod, or None if it does.
fn claim_mismatch(
    pvc: &PersistentVolumeClaim,
    data_replica_set: &DataReplicaSet,
    data_pod: &DataPod,
    storage_class: &str,
) -> Option<String> {
    let name = pvc.name_any();
    let uid = data_replica_set.metadata.uid.as_deref().unwrap_or_default();
    if !is_owned_by(pvc, uid) {
        return Some(format!("PVC {name} exists but is not owned by this set"));
    }
    let pod = pvc
        .annotations()
        .get("kubedal.arunaengine.org/data-pod-name");
    if pod.map(String::as_str) != Some(data_pod.name_any().as_str()) {
        return Some(format!(
            "PVC {name} exists but is not for DataPod {}",
            data_pod.name_any()
        ));
    }
    let class = pvc
        .spec
        .as_ref()
        .and_then(|s| s.storage_class_name.as_deref());
    if class != Some(storage_class) {
        return Some(format!(
            "PVC {name} exists but does not use StorageClass {storage_class}"
        ));
    }
    None
}

/// The replica DataPods of the set with `drs_uid`: owned by it and named as its
/// replicas.
fn owned_replicas(drs_uid: &str, data_pods: Vec<DataPod>) -> Vec<DataPod> {
    let prefix = format!("{drs_uid}-replica-");
    data_pods
        .into_iter()
        .filter(|dp| is_owned_by(dp, drs_uid))
        .filter(|dp| dp.name_any().starts_with(&prefix))
        .collect()
}

/// The PVC and DataPod names that scaling to zero removes: the replicas the set
/// owns, their PVCs, and every other PVC the set owns (the one of its source).
fn zero_replica_cleanup(
    data_replica_set: &DataReplicaSet,
    uid: &str,
    data_pods: &[DataPod],
    pvcs: &[PersistentVolumeClaim],
) -> (Vec<String>, Vec<String>) {
    let replicas = owned_by(data_pods, uid);
    let mut pvc_names: Vec<String> = replicas
        .iter()
        .map(|dp| pvc_name(data_replica_set, dp))
        .collect();
    for pvc in owned_by(pvcs, uid) {
        if !pvc_names.contains(&pvc.name_any()) {
            pvc_names.push(pvc.name_any());
        }
    }
    let data_pod_names = replicas.iter().map(|dp| dp.name_any()).collect();
    (pvc_names, data_pod_names)
}

/// Delete the object `name` only if `owner` owns it. The delete carries the
/// object's UID as a precondition, so an object that was replaced meanwhile
/// (same name, new UID) is never deleted. A missing or foreign object is left.
async fn delete_owned<K>(api: &Api<K>, name: &str, owner: &DataReplicaSet) -> Result<(), Error>
where
    K: kube::Resource + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
{
    let Some(object) = api.get_opt(name).await? else {
        return Ok(());
    };
    let owner_uid = owner.metadata.uid.as_deref().unwrap_or_default();
    let Some(params) = owned_delete_params(&object, owner_uid) else {
        warn!(
            "DataReplicaSet {}: not deleting {name}, it is not owned by the set",
            owner.name_any()
        );
        return Ok(());
    };
    match api.delete(name, &params).await {
        Ok(_) => Ok(()),
        // Gone, or replaced by another object with the same name.
        Err(kube::Error::Api(status)) if status.code == 404 || status.code == 409 => Ok(()),
        Err(err) => Err(Error::KubeError(err)),
    }
}

/// Delete parameters that only match `object` itself, or None if the owner
/// with `owner_uid` does not own it.
fn owned_delete_params<K: Resource>(object: &K, owner_uid: &str) -> Option<DeleteParams> {
    if !is_owned_by(object, owner_uid) {
        return None;
    }
    Some(DeleteParams {
        preconditions: Some(Preconditions {
            uid: object.meta().uid.clone(),
            resource_version: None,
        }),
        ..Default::default()
    })
}

/// True if the replica lives on a DataNode (in namespace `ns`) that is read-only.
fn on_read_only_node(replica: &DataPod, nodes: &[DataNode], ns: &str) -> bool {
    let Some(node_ref) = replica.spec.data_node_ref.as_ref() else {
        return false;
    };
    let node_ns = node_ref.namespace.as_deref().unwrap_or(ns);
    nodes.iter().any(|n| {
        n.name_any() == node_ref.name
            && n.namespace().as_deref().unwrap_or(ns) == node_ns
            && n.spec.read_only
    })
}

/// An error message if the source DataPod reference points to another namespace.
fn source_namespace_error(drs_namespace: &str, source: &Ref) -> Option<String> {
    match source.namespace.as_deref() {
        Some(ns) if !ns.is_empty() && ns != drs_namespace => Some(format!(
            "sourceDataPodRef {ns}/{} must be in namespace {drs_namespace}, \
             the namespace of the DataReplicaSet",
            source.name
        )),
        _ => None,
    }
}

/// The DataNode of the source DataPod, with its namespace filled in.
fn source_node_ref(source: &DataPod, default_namespace: &str) -> Result<Ref, Error> {
    let node_ref = source.spec.data_node_ref.as_ref().ok_or_else(|| {
        Error::ReconcilerError(format!(
            "source DataPod {} has no DataNode yet",
            source.name_any()
        ))
    })?;
    Ok(Ref {
        name: node_ref.name.clone(),
        namespace: Some(
            node_ref
                .namespace
                .clone()
                .filter(|ns| !ns.is_empty())
                .unwrap_or_else(|| default_namespace.to_string()),
        ),
    })
}

/// What a sync pod mount depends on: the DataPod object (UID), its DataNode, its
/// path and the PVC. If any of these change, the sync pod must be recreated.
fn member_id(data_pod: &DataPod, pvc_name: &str) -> String {
    let node = data_pod
        .spec
        .data_node_ref
        .as_ref()
        .map(|r| format!("{}/{}", r.namespace.clone().unwrap_or_default(), r.name))
        .unwrap_or_default();
    format!(
        "{}|{}|{}|{}",
        data_pod.metadata.uid.clone().unwrap_or_default(),
        node,
        data_pod.spec.path.clone().unwrap_or_default(),
        pvc_name
    )
}

/// A sync pod is kept only while it mounts the current members and has not ended.
fn sync_pod_is_current(pod: &Pod, members: &str) -> bool {
    let phase = pod.status.as_ref().and_then(|s| s.phase.as_deref());
    let ended = matches!(phase, Some("Failed") | Some("Succeeded"));
    !ended && pod_members(pod) == Some(members)
}

/// The members hash of a sync pod, if it has one.
fn pod_members(pod: &Pod) -> Option<&str> {
    pod.metadata
        .annotations
        .as_ref()?
        .get(MEMBERS_ANNOTATION)
        .map(String::as_str)
}

/// Name of the sync pod of this set.
fn sync_pod_name(data_replica_set: &DataReplicaSet) -> String {
    format!("{}-sync", data_replica_set.name_any())
}

/// The set is available when all replicas exist and the sync pod runs.
fn replica_set_available(desired: usize, replicas: usize, sync_pod_phase: Option<&str>) -> bool {
    replicas >= desired && sync_pod_phase == Some("Running")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The controller deletes PVCs, DataPods and pods; the chart must allow it.
    #[test]
    fn controller_role_allows_the_deletes_it_does() {
        let rbac = include_str!("../../charts/kubedal/templates/rbac.yaml");
        let controller = rbac.split("-node").next().unwrap_or_default();
        for resource in [
            "persistentvolumeclaims",
            "datareplicasets/status",
            "\"pods\"",
        ] {
            let at = controller
                .find(resource)
                .unwrap_or_else(|| panic!("{resource} missing"));
            let verbs = controller[at..]
                .lines()
                .find(|l| l.contains("verbs:"))
                .unwrap();
            assert!(verbs.contains("\"delete\""), "{resource}: {verbs}");
        }
    }

    #[test]
    fn owned_objects_are_found_by_owner_uid_only() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
        let dp = |name: &str, owner: Option<&str>| {
            let mut dp = DataPod::new(name, DataPodSpec::default());
            dp.metadata.owner_references = owner.map(|uid| {
                vec![OwnerReference {
                    uid: uid.to_string(),
                    ..Default::default()
                }]
            });
            dp
        };
        // A replica whose labels no longer match the selector is still owned.
        let items = vec![
            dp("old-labels", Some("u")),
            dp("other-set", Some("v")),
            dp("free", None),
        ];
        let names: Vec<_> = owned_by(&items, "u").iter().map(|d| d.name_any()).collect();
        assert_eq!(names, vec!["old-labels"]);
        assert!(owned_by(&items, "").is_empty());
    }

    #[test]
    fn a_foreign_claim_with_the_expected_name_is_refused() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
        let mut drs = DataReplicaSet::new(
            "set",
            serde_json::from_value(json!({
                "replicas": 1,
                "selector": {"matchLabels": {}},
                "sourceDataPodRef": {"name": "source"},
                "template": {"metadata": {}, "spec": {}}
            }))
            .unwrap(),
        );
        drs.metadata.uid = Some("u".into());
        let source = DataPod::new("source", DataPodSpec::default());
        let claim = |owner: &str, pod: &str, class: &str| PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some("set-source".into()),
                owner_references: Some(vec![OwnerReference {
                    uid: owner.into(),
                    ..Default::default()
                }]),
                annotations: Some(
                    [(
                        "kubedal.arunaengine.org/data-pod-name".to_string(),
                        pod.to_string(),
                    )]
                    .into(),
                ),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                storage_class_name: Some(class.into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let check = |pvc| claim_mismatch(&pvc, &drs, &source, "kubedal");
        assert_eq!(check(claim("u", "source", "kubedal")), None);
        for (pvc, why) in [
            (claim("other", "source", "kubedal"), "not owned"),
            (claim("u", "elsewhere", "kubedal"), "not for DataPod"),
            (claim("u", "source", "fast"), "StorageClass"),
        ] {
            let problem = check(pvc).unwrap();
            assert!(problem.contains(why), "{problem}");
        }
    }

    #[test]
    fn only_owned_objects_are_deleted_and_only_themselves() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
        let meta = |uid: &str, owner: Option<&str>| ObjectMeta {
            name: Some("set-source".into()),
            uid: Some(uid.into()),
            owner_references: owner.map(|o| {
                vec![OwnerReference {
                    uid: o.into(),
                    ..Default::default()
                }]
            }),
            ..Default::default()
        };
        let pvc = |uid, owner| PersistentVolumeClaim {
            metadata: meta(uid, owner),
            ..Default::default()
        };
        let pod = |uid, owner| Pod {
            metadata: meta(uid, owner),
            ..Default::default()
        };
        // Foreign objects with the expected names are never deleted.
        assert!(owned_delete_params(&pvc("p1", None), "u").is_none());
        assert!(owned_delete_params(&pvc("p2", Some("other")), "u").is_none());
        assert!(owned_delete_params(&pod("s1", Some("other")), "u").is_none());
        // Owned ones are deleted only if their UID still matches.
        let params = owned_delete_params(&pod("s2", Some("u")), "u").unwrap();
        assert_eq!(params.preconditions.unwrap().uid.as_deref(), Some("s2"));
        let params = owned_delete_params(&pvc("p3", Some("u")), "u").unwrap();
        assert_eq!(params.preconditions.unwrap().uid.as_deref(), Some("p3"));
    }

    #[test]
    fn replicas_are_found_by_owner_after_a_selector_change() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
        let dp = |name: &str, owner: &str, label: &str| {
            let mut dp = DataPod::new(name, DataPodSpec::default());
            dp.metadata.labels = Some([("app".to_string(), label.to_string())].into());
            dp.metadata.owner_references = Some(vec![OwnerReference {
                uid: owner.into(),
                ..Default::default()
            }]);
            dp
        };
        let items = vec![
            dp("u-replica-0", "u", "old"),
            dp("u-replica-1", "u", "new"),
            // Not owned by the set, even with a replica name.
            dp("u-replica-2", "v", "new"),
            // Owned, but not a replica name.
            dp("helper", "u", "new"),
        ];
        let names: Vec<_> = owned_replicas("u", items)
            .iter()
            .map(|d| d.name_any())
            .collect();
        assert_eq!(names, vec!["u-replica-0", "u-replica-1"]);
    }

    #[test]
    fn scaling_to_zero_also_removes_the_source_pvc() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
        let drs: DataReplicaSet = serde_json::from_value(json!({
            "apiVersion": "kubedal.arunaengine.org/v1alpha1",
            "kind": "DataReplicaSet",
            "metadata": {"name": "set", "namespace": "ns", "uid": "u"},
            "spec": {
                "replicas": 0,
                "selector": {"matchLabels": {}},
                "sourceDataPodRef": {"name": "source"},
                "template": {"metadata": {}, "spec": {}}
            }
        }))
        .unwrap();
        let owned = Some(vec![OwnerReference {
            uid: "u".into(),
            ..Default::default()
        }]);
        let mut replica = DataPod::new("u-replica-0", DataPodSpec::default());
        replica.metadata.owner_references = owned.clone();
        // The source DataPod belongs to someone else, but its PVC to the set.
        let source = DataPod::new("source", DataPodSpec::default());
        let pvc = |name: &str, owner_references| PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(name.into()),
                owner_references,
                ..Default::default()
            },
            ..Default::default()
        };
        let pvcs = [
            pvc("set-u-replica-0", owned.clone()),
            pvc("set-source", owned),
            pvc("unrelated", None),
        ];
        let (pvc_names, data_pods) = zero_replica_cleanup(&drs, "u", &[replica, source], &pvcs);
        assert_eq!(pvc_names, vec!["set-u-replica-0", "set-source"]);
        assert_eq!(data_pods, vec!["u-replica-0"]);
    }

    /// The chart passes its StorageClass name, so DataReplicaSet PVCs use the
    /// class the chart created.
    #[test]
    fn the_chart_passes_its_storage_class_and_sync_image_to_the_controller() {
        let controller = include_str!("../../charts/kubedal/templates/controller.yaml");
        assert!(controller.contains("- --storage-class={{ .Values.storageClass.name }}"));
        assert!(controller.contains("- --sync-image={{ include \"kubedal.image\" . }}"));
    }

    #[test]
    fn scaling_down_removes_the_highest_replicas() {
        let dp = |name: &str| DataPod::new(name, DataPodSpec::default());
        let all = || vec![dp("u-replica-2"), dp("u-replica-0"), dp("u-replica-10")];
        let names = |v: &[DataPod]| v.iter().map(|d| d.name_any()).collect::<Vec<_>>();
        let (keep, excess) = split_excess("u", all(), 1);
        assert_eq!(names(&keep), vec!["u-replica-0"]);
        assert_eq!(names(&excess), vec!["u-replica-2", "u-replica-10"]);
        let (keep, excess) = split_excess("u", all(), 0);
        assert!(keep.is_empty());
        assert_eq!(excess.len(), 3);
        let (keep, excess) = split_excess("u", all(), 5);
        assert_eq!(keep.len(), 3);
        assert!(excess.is_empty());
    }

    #[test]
    fn replicas_on_read_only_data_nodes_are_found() {
        use crate::resource::crd::{Backend, DataNodeSpec};
        let node = |name: &str, read_only: bool| {
            let mut n = DataNode::new(
                name,
                DataNodeSpec {
                    backend: Backend::S3,
                    read_only,
                    secret_ref: None,
                    config_map_ref: None,
                    config: Default::default(),
                    limit: None,
                },
            );
            n.metadata.namespace = Some("team".into());
            n
        };
        let nodes = vec![node("ro", true), node("rw", false)];
        let replica = |node: &str| {
            DataPod::new(
                "r",
                DataPodSpec {
                    data_node_ref: Some(Ref {
                        name: node.into(),
                        namespace: Some("team".into()),
                    }),
                    ..Default::default()
                },
            )
        };
        assert!(on_read_only_node(&replica("ro"), &nodes, "team"));
        assert!(!on_read_only_node(&replica("rw"), &nodes, "team"));
        assert!(!on_read_only_node(&replica("missing"), &nodes, "team"));
    }

    #[test]
    fn source_data_pod_must_be_in_the_same_namespace() {
        let local = Ref {
            name: "src".into(),
            namespace: None,
        };
        assert!(source_namespace_error("team", &local).is_none());
        let same = Ref {
            name: "src".into(),
            namespace: Some("team".into()),
        };
        assert!(source_namespace_error("team", &same).is_none());
        let other = Ref {
            name: "src".into(),
            namespace: Some("other".into()),
        };
        assert!(source_namespace_error("team", &other).is_some());
    }

    #[test]
    fn source_data_node_comes_from_the_reference() {
        let mut dp = DataPod::new("src", DataPodSpec::default());
        assert!(source_node_ref(&dp, "team").is_err());
        dp.spec.data_node_ref = Some(Ref {
            name: "shared-store".into(),
            namespace: Some("shared".into()),
        });
        let r = source_node_ref(&dp, "team").unwrap();
        assert_eq!(r.namespace.as_deref(), Some("shared"));
        dp.spec.data_node_ref = Some(Ref {
            name: "store".into(),
            namespace: None,
        });
        let r = source_node_ref(&dp, "team").unwrap();
        assert_eq!(r.namespace.as_deref(), Some("team"));
    }

    #[test]
    fn members_change_when_a_replica_is_recreated_elsewhere() {
        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some("set-replica-0".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let replica = |uid: &str, node: &str| {
            let mut dp = DataPod::new(
                "u-replica-0",
                DataPodSpec {
                    path: Some("/u/replica-0".into()),
                    data_node_ref: Some(Ref {
                        name: node.into(),
                        namespace: Some("team".into()),
                    }),
                    ..Default::default()
                },
            );
            dp.metadata.uid = Some(uid.into());
            dp
        };
        // Same names, but the replica was deleted and created on another DataNode.
        let before = member_id(&replica("uid-1", "node-a"), &pvc.name_any());
        let after = member_id(&replica("uid-2", "node-b"), &pvc.name_any());
        assert_ne!(
            members_hash("src", [before].into_iter()),
            members_hash("src", [after].into_iter())
        );
    }

    #[test]
    fn ended_sync_pods_are_replaced() {
        use k8s_openapi::api::core::v1::PodStatus;
        let mut pod = Pod::default();
        pod.metadata.annotations = Some(
            [(MEMBERS_ANNOTATION.to_string(), "m".to_string())]
                .into_iter()
                .collect(),
        );
        for (phase, current) in [
            ("Running", true),
            ("Pending", true),
            ("Failed", false),
            ("Succeeded", false),
        ] {
            pod.status = Some(PodStatus {
                phase: Some(phase.into()),
                ..Default::default()
            });
            assert_eq!(sync_pod_is_current(&pod, "m"), current, "{phase}");
        }
        assert!(!sync_pod_is_current(&pod, "other"));
    }

    #[test]
    fn members_hash_changes_with_the_replicas() {
        let one = members_hash("src", ["a".to_string()].into_iter());
        let two = members_hash("src", ["b".to_string(), "a".to_string()].into_iter());
        assert_ne!(one, two);
        assert_eq!(
            two,
            members_hash("src", ["a".to_string(), "b".to_string()].into_iter())
        );
        let mut pod = Pod::default();
        assert_eq!(pod_members(&pod), None);
        pod.metadata.annotations = Some(
            [(MEMBERS_ANNOTATION.to_string(), one.clone())]
                .into_iter()
                .collect(),
        );
        assert_eq!(pod_members(&pod), Some(one.as_str()));
    }

    #[test]
    fn missing_replicas_are_found_by_name() {
        let dp = |name: &str| DataPod::new(name, DataPodSpec::default());
        // replica-0 was deleted while replica-1 still exists.
        let existing = [dp("u-replica-1"), dp("unrelated")];
        let taken: Vec<String> = existing.iter().map(|dp| dp.name_any()).collect();
        assert_eq!(free_replica_indexes("u", &taken, 1), vec![0]);
        assert_eq!(free_replica_indexes("u", &taken, 2), vec![0, 2]);
        assert!(free_replica_indexes("u", &taken, 0).is_empty());
        // Old replicas wait until replacements exist and are synced.
        assert!(!old_replicas_can_go(1, 2, true));
        assert!(!old_replicas_can_go(2, 2, false));
        assert!(old_replicas_can_go(2, 2, true));
        assert!(old_replicas_can_go(0, 0, false));
    }

    #[test]
    fn replica_folders_differ_between_sets() {
        assert_ne!(replica_path("set-a", 0), replica_path("set-b", 0));
        assert_eq!(replica_path("set-a", 1), "/set-a/replica-1");
    }

    #[test]
    fn available_needs_all_replicas_and_a_running_sync_pod() {
        assert!(replica_set_available(2, 2, Some("Running")));
        assert!(!replica_set_available(2, 1, Some("Running")));
        assert!(!replica_set_available(2, 2, Some("Pending")));
        assert!(!replica_set_available(2, 2, Some("Failed")));
        assert!(!replica_set_available(2, 2, None));
    }
}
