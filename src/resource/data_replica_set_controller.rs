use super::controller::{Context, Error};
use crate::resource::crd::{DataNode, DataPod, MountAccess};
use crate::resource::crd::{DataPodSpec, DataReplicaSet, Ref};
use k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaim, PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource,
    Pod, PodSpec, Volume, VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{ListParams, PostParams};
use kube::{
    Resource,
    api::{Api, ResourceExt},
    runtime::{
        controller::Action,
        events::{Event, EventType},
        finalizer::{Event as Finalizer, finalizer},
    },
};
use rand::Rng;
use rand::distr::Alphanumeric;
use std::{sync::Arc, time::Duration};
use tracing::*;

const DATASOURCE_FINALIZER: &str = "kubedal.arunaengine.org/datareplicaset";

#[instrument(skip(ctx, res), fields(trace_id))]
pub async fn reconcile_drs(res: Arc<DataReplicaSet>, ctx: Arc<Context>) -> Result<Action, Error> {
    let ns = res.namespace().unwrap(); // res is namespace scoped
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
        // If the status is already initialized, do nothing
        if self.status.is_some() {
            return Ok(Action::requeue(Duration::from_secs(30 * 60)));
        }

        // Eval DataReplicaSet
        let client = ctx.client.clone();
        let drs_name = self.name_any();
        let drs_ns = self.namespace().ok_or_else(|| {
            Error::ReconcilerError(format!("DataReplicaSet {} is missing namespace", drs_name))
        })?; // .unwrap_or_default? Or match and patch with 'default'?
        let drs_replicas = self.spec.replicas as usize;
        let selector = self
            .spec
            .selector
            .labels
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join(",");

        // Eval number of existing DataPod replicas
        let dp_api: Api<DataPod> = Api::namespaced(client.clone(), &drs_ns);
        let drs_data_pods = dp_api
            .list(&ListParams::default().labels(&selector))
            .await?;

        // Create source DataPod replicas with refs to other DataNodes
        if drs_data_pods.items.len() == drs_replicas {
            //TODO: Check if metadata/spec has changed and patch if necessary (?)

            // Everything is fine ... Check back in some minutes
            return Ok(Action::requeue(Duration::from_secs(5 * 60)));
        } else if drs_data_pods.items.len() < drs_replicas {
            let dn_api: Api<DataNode> = Api::namespaced(client.clone(), &drs_ns);

            // Fetch source DataPod and its DataNode
            let source_data_pod = dp_api.get(&self.spec.source_pod.name).await?;
            let source_data_node = dn_api
                .get(
                    &source_data_pod
                        .owner_references()
                        .first()
                        .ok_or_else(|| {
                            Error::ReconcilerError(format!(
                                "DataPod {} is missing owner ref",
                                source_data_pod.name_any()
                            ))
                        })?
                        .name,
                )
                .await?;

            // Fetch other available DataNodes
            let mut available_data_nodes = dn_api
                .list(&ListParams::default())
                .await?
                .into_iter()
                .filter(|node| node.metadata.uid != source_data_node.metadata.uid)
                .collect::<Vec<_>>();

            // Do ... stuff
            let pvc_api = Api::<PersistentVolumeClaim>::namespaced(client.clone(), &drs_ns);
            let mut replica_resources = vec![];
            for index in drs_data_pods.items.len()..drs_replicas {
                match available_data_nodes.pop() {
                    Some(replica_data_node) => {
                        let replica_data_pod =
                            create_replica_data_pod(index, &self, &replica_data_node, &dp_api)
                                .await?;
                        let replica_pvc = create_replica_pvc(
                            &replica_data_pod,
                            &replica_data_node,
                            &self,
                            MountAccess::FuseReadWrite,
                            &pvc_api,
                        )
                        .await?;
                        replica_resources.push((replica_data_pod, replica_pvc));
                    }
                    None => break,
                }
            }

            let source_pvc = create_replica_pvc(
                &source_data_pod,
                &source_data_node,
                &self,
                MountAccess::FuseReadOnly,
                &pvc_api,
            )
            .await?;

            //Create Pod with read-only mount to source DataPod and read-write mounts to replica DataPods
            //TODO: Interval poll source mount for changes and sync to replica mounts
            let pod_api = Api::<Pod>::namespaced(client.clone(), &drs_ns);
            let pod = create_replica_pod(
                &self,
                &source_data_pod,
                &source_pvc,
                replica_resources,
                &pod_api,
            )
            .await?;
            info!("Successfully created replica pod: {}", pod.name_any())
        }

        /* //TODO: DataReplicaSetStatus ?
        trace!("Patching status for {}", name);
        let ps = PatchParams::apply("cntrlr").force();
        let _o = drs_api
            .patch_status(&name, &ps, &new_status)
            .await
            .inspect_err(|e| error!("Failed to patch status: {:?}", e))
            .map_err(Error::KubeError)?;

        ctx.recorder
            .publish(
                &Event {
                    type_: EventType::Normal,
                    reason: "Initialized".into(),
                    note: Some(format!("Init `{}`", self.name_any())),
                    action: "Initialized".into(),
                    secondary: None,
                },
                &self.object_ref(&()),
            )
            .await
            .map_err(Error::KubeError)?;
        */

        // Patch status and reschedule reconcile in 5 minutes
        return Ok(Action::requeue(Duration::from_secs(5 * 60)));
    }

    // Finalizer cleanup (the object was deleted, ensure nothing is orphaned)
    async fn cleanup(&self, ctx: Arc<Context>) -> Result<Action, Error> {
        //TODO: Delete all pods spawned from this replica set -> spec.selector
        // ... or does ownerRef its magic?

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
            name: Some(format!("{}-replica-{}", drs_uid, index)),
            namespace: Some(drs_ns.clone()),
            owner_references: Some(vec![drs_owner.clone()]),
            ..Default::default()
        },
        spec: DataPodSpec {
            path: Some(format!("/replica-{}", index)),
            data_node_ref: Some(Ref {
                name: data_node.name_any(),
                namespace: data_node.namespace(),
            }),
            data_node_selector: None,
            request: None, //TODO: ???
        },
        status: None,
    };

    Ok(data_pod_api
        .create(&PostParams::default(), &replica_pod)
        .await?)
}

async fn create_replica_pvc(
    data_pod: &DataPod,
    data_node: &DataNode,
    data_replica_set: &DataReplicaSet,
    mount_access: MountAccess,
    pvc_api: &Api<PersistentVolumeClaim>,
) -> Result<PersistentVolumeClaim, Error> {
    // Create PVC for source mount
    let rand_suffix = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(8)
        .map(char::from)
        .collect::<String>()
        .to_lowercase();

    let replica_owner = data_replica_set.owner_ref(&()).ok_or_else(|| {
        Error::ReconcilerError(format!(
            "Failed to create owner ref from DataReplicaSet: {}",
            data_replica_set.name_any()
        ))
    })?;

    let pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(format!(
                "pvc-{}-{}",
                data_replica_set
                    .metadata
                    .uid
                    .clone()
                    .unwrap_or(data_replica_set.name_any()),
                rand_suffix
            )),
            namespace: data_replica_set.namespace(),
            owner_references: Some(vec![replica_owner.clone()]),
            annotations: Some(
                vec![
                    (
                        "kubedal.arunaengine.org/data-node-name".to_string(),
                        data_node.name_any(),
                    ),
                    (
                        "kubedal.arunaengine.org/data-node-namespace".to_string(),
                        data_node.metadata.namespace.clone().unwrap_or_default(),
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
            storage_class_name: Some("kubedal".to_string()),
            ..Default::default()
        }),
        status: None,
    };

    Ok(pvc_api.create(&PostParams::default(), &pvc).await?)
}

async fn create_replica_pod(
    data_replica_set: &DataReplicaSet,
    source_data_pod: &DataPod,
    source_pvc: &PersistentVolumeClaim,
    replica_resources: Vec<(DataPod, PersistentVolumeClaim)>,
    pod_api: &Api<Pod>,
) -> Result<Pod, Error> {
    let rand_suffix = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(8)
        .map(char::from)
        .collect::<String>()
        .to_lowercase();

    let replica_owner = data_replica_set.owner_ref(&()).ok_or_else(|| {
        Error::ReconcilerError(format!(
            "Failed to create owner ref from DataReplicaSet: {}",
            data_replica_set.name_any()
        ))
    })?;

    // Add source volume
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
        mount_path: source_data_pod.spec.path.clone().ok_or_else(|| {
            Error::ReconcilerError(format!(
                "DataPod {} is missing path.",
                source_data_pod.name_any()
            ))
        })?,
        ..Default::default()
    }];

    for (i, (replica_pod, replica_pvc)) in replica_resources.iter().enumerate() {
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
            mount_path: replica_pod.spec.path.clone().ok_or_else(|| {
                Error::ReconcilerError(format!(
                    "Replica DataPod {} is missing path.",
                    source_data_pod.name_any()
                ))
            })?,
            ..Default::default()
        });
    }

    let container = Container {
        name: "sync-replica".to_string(),
        image: Some("alpine:3.21".to_string()),
        command: Some(vec!["sleep".to_string(), "infinity".to_string()]), //TODO: Sync src mount to replica mounts
        volume_mounts: Some(volume_mounts),
        ..Default::default()
    };

    let pod = Pod {
        metadata: ObjectMeta {
            name: Some(format!("{}-{}", data_replica_set.name_any(), rand_suffix)),
            namespace: data_replica_set.namespace(),
            owner_references: Some(vec![replica_owner]),
            labels: None,      //TODO
            annotations: None, //TODO
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
