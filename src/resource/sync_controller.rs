use crate::resource::crd::{Sync, SyncPodStatus, SyncStatus};
use k8s_openapi::{
    api::{
        batch::v1::{Job, JobStatus},
        core::v1::{
            Container, PersistentVolumeClaim, PersistentVolumeClaimSpec,
            PersistentVolumeClaimVolumeSource, PodSpec, Volume, VolumeMount,
            VolumeResourceRequirements,
        },
    },
    apimachinery::pkg::api::resource::Quantity,
};
use kube::{
    Client, Resource,
    api::{Api, ObjectMeta, Patch, PatchParams, PostParams, ResourceExt},
    runtime::{
        controller::Action,
        events::{Event, EventType},
        finalizer::{Event as Finalizer, finalizer},
    },
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tracing::*;

const SYNC_FINALIZER: &str = "kubedal.arunaengine.org/sync";

use super::controller::{Context, Error};

#[instrument(skip(ctx, res), fields(trace_id))]
pub async fn reconcile_sy(res: Arc<Sync>, ctx: Arc<Context>) -> Result<Action, Error> {
    let ns = res.namespace().unwrap(); // res is namespace scoped
    let ds: Api<Sync> = Api::namespaced(ctx.client.clone(), &ns);

    info!(
        "Reconciling Sync operation \"{}\" in {}",
        res.name_any(),
        ns
    );
    finalizer(&ds, SYNC_FINALIZER, res, |event| async {
        match event {
            Finalizer::Apply(doc) => doc.reconcile(ctx.clone()).await,
            Finalizer::Cleanup(doc) => doc.cleanup(ctx.clone()).await,
        }
    })
    .await
    .map_err(|e| Error::FinalizerError(Box::new(e)))
}

pub fn error_policy_sy(doc: Arc<Sync>, error: &Error, _ctx: Arc<Context>) -> Action {
    warn!("reconcile failed: {:?}, {:?}", error, doc);
    Action::requeue(Duration::from_secs(5 * 60))
}

impl Sync {
    // Reconcile (for non-finalizer related changes)
    async fn reconcile(&self, ctx: Arc<Context>) -> Result<Action, Error> {
        // If the status is already initialized, do nothing
        let client = ctx.client.clone();
        let ns = self
            .namespace()
            .ok_or_else(|| Error::ReconcilerError("Missing namespace".into()))?;

        let name = self.name_any();
        let docs: Api<Sync> = Api::namespaced(client.clone(), &ns);

        let status_update = match self.status {
            Some(ref status) => {
                let job_api = Api::<Job>::namespaced(client.clone(), &ns);
                let job = job_api
                    .get(&status.pod_name)
                    .await
                    .map_err(Error::KubeError)?;

                let sync_status = job_status_to_sync_status(&job.status);

                Patch::Apply(json!({
                    "apiVersion": Sync::api_version(&()),
                    "kind": Sync::kind(&()),
                    "status": SyncStatus {
                        pod_name: status.pod_name.clone(),
                        sync_status,
                    }
                }))
            }
            None => {
                let (source, destination) = create_pvcs(client.clone(), self).await?;
                let job = create_job(client, self, source, destination).await?;

                // always overwrite status object with what we saw
                Patch::Apply(json!({
                    "apiVersion": Sync::api_version(&()),
                    "kind": Sync::kind(&()),
                    "status": SyncStatus {
                        pod_name: job.metadata.name.clone().unwrap(),
                        sync_status: SyncPodStatus::Pending,
                    }
                }))
            }
        };

        trace!("Patching status for {}", name);
        let ps = PatchParams::apply("cntrlr").force();
        let new_sync = docs
            .patch_status(&name, &ps, &status_update)
            .await
            .inspect_err(|e| error!("Failed to patch status: {:?}", e))
            .map_err(Error::KubeError)?;

        if self.status != new_sync.status {
            ctx.recorder
                .publish(
                    &Event {
                        type_: EventType::Normal,
                        reason: format!(
                            "Status{:?}",
                            new_sync
                                .status
                                .as_ref()
                                .map(|s| s.sync_status.clone())
                                .unwrap_or_default()
                        ),
                        action: "ReconciledSync".into(),
                        note: None,
                        secondary: None,
                    },
                    &self.object_ref(&()),
                )
                .await
                .map_err(Error::KubeError)?;
        }

        if let Some(SyncStatus {
            sync_status: SyncPodStatus::Completed,
            ..
        }) = new_sync.status.as_ref()
        {
            Ok(Action::await_change())
        } else {
            Ok(Action::requeue(Duration::from_secs(15)))
        }
    }

    // Finalizer cleanup (the object was deleted, ensure nothing is orphaned)
    async fn cleanup(&self, ctx: Arc<Context>) -> Result<Action, Error> {
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

async fn create_pvcs(
    client: Client,
    sync: &Sync,
) -> Result<(PersistentVolumeClaim, PersistentVolumeClaim), Error> {
    let ns = sync
        .namespace()
        .ok_or_else(|| Error::ReconcilerError("Missing namespace".into()))?;
    let pvc_api = Api::<PersistentVolumeClaim>::namespaced(client.clone(), &ns);

    let sync_owner = sync.owner_ref(&()).unwrap();

    let source_pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(format!("source{}", sync.uid().unwrap_or(sync.name_any()))),
            namespace: Some(ns.clone()),
            owner_references: Some(vec![sync_owner.clone()]),
            annotations: Some(
                vec![
                    (
                        "kubedal.arunaengine.org/datasource".to_string(),
                        sync.spec.source.name.clone(),
                    ),
                    (
                        "kubedal.arunaengine.org/namespace".to_string(),
                        sync.spec.source.namespace.clone().unwrap_or(ns.clone()),
                    ),
                    (
                        "kubedal.arunaengine.org/mount".to_string(),
                        "fuse".to_string(),
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
        ..Default::default()
    };

    let destination_pvc = PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(format!("target{}", sync.uid().unwrap_or(sync.name_any()))),
            namespace: Some(ns.clone()),
            owner_references: Some(vec![sync_owner]),
            annotations: Some(
                vec![
                    (
                        "kubedal.arunaengine.org/datasource".to_string(),
                        sync.spec.destination.name.clone(),
                    ),
                    (
                        "kubedal.arunaengine.org/namespace".to_string(),
                        sync.spec
                            .destination
                            .namespace
                            .clone()
                            .unwrap_or(ns.clone()),
                    ),
                    (
                        "kubedal.arunaengine.org/mount".to_string(),
                        "fuse".to_string(),
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
        ..Default::default()
    };
    let source = pvc_api
        .create(&PostParams::default(), &source_pvc)
        .await
        .map_err(Error::KubeError)?;
    let destination = pvc_api
        .create(&PostParams::default(), &destination_pvc)
        .await
        .map_err(Error::KubeError)?;
    Ok((source, destination))
}

async fn create_job(
    client: Client,
    sync: &Sync,
    source: PersistentVolumeClaim,
    destination: PersistentVolumeClaim,
) -> Result<Job, Error> {
    let ns = sync
        .namespace()
        .ok_or_else(|| Error::ReconcilerError("Missing namespace".into()))?;
    let job_api = Api::<Job>::namespaced(client.clone(), &ns);

    let sync_owner = sync.owner_ref(&()).unwrap();

    let job = Job {
        metadata: ObjectMeta {
            name: Some(format!(
                "sync-{}{}",
                sync.uid().unwrap_or(sync.name_any()),
                sync.metadata.resource_version.clone().unwrap_or_default()
            )),
            namespace: Some(ns.clone()),
            owner_references: Some(vec![sync_owner.clone()]),
            ..Default::default()
        },
        spec: Some(k8s_openapi::api::batch::v1::JobSpec {

            template: k8s_openapi::api::core::v1::PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    name: Some(format!(
                        "sync-{}{}",
                        sync.uid().unwrap_or(sync.name_any()),
                        sync.metadata.resource_version.clone().unwrap_or_default()
                    )),
                    owner_references: Some(vec![sync_owner.clone()]),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    restart_policy: Some("OnFailure".to_string()),
                    containers: vec![Container {
                        name: "sync".to_string(),
                        image: Some("alpine:3.20".to_string()),
                        command: Some(vec!["sh".to_string(), "-c".to_string()]),
                        args: Some(vec![format!("cp -a /source/. /destination/")]),
                        volume_mounts: Some(vec![
                            VolumeMount {
                                name: "source".to_string(),
                                mount_path: "/source".to_string(),
                                ..Default::default()
                            },
                            VolumeMount {
                                name: "destination".to_string(),
                                mount_path: "/destination".to_string(),
                                ..Default::default()
                            },
                        ]),
                        ..Default::default()
                    }],
                    volumes: Some(vec![
                        Volume {
                            name: "source".to_string(),
                            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                                claim_name: source.metadata.name.clone().unwrap(),
                                read_only: Some(true),
                            }),
                            ..Default::default()
                        },
                        Volume {
                            name: "destination".to_string(),
                            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                                claim_name: destination.metadata.name.clone().unwrap(),
                                read_only: None,
                            }),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    job_api
        .create(&PostParams::default(), &job)
        .await
        .map_err(Error::KubeError)
}

/// Maps a Kubernetes Job status to a SyncPodStatus
pub fn job_status_to_sync_status(job_status: &Option<JobStatus>) -> SyncPodStatus {
    match job_status {
        None => SyncPodStatus::Pending,
        Some(status) => {
            // Check if the job has failed
            if let Some(failed) = status.failed {
                if failed > 0 {
                    return SyncPodStatus::Failed;
                }
            }

            // Check if the job has completed successfully
            if let Some(succeeded) = status.succeeded {
                if succeeded > 0 {
                    return SyncPodStatus::Completed;
                }
            }

            // Check if the job has active pods
            if let Some(active) = status.active {
                if active > 0 {
                    return SyncPodStatus::Running;
                }
            }

            // Check if the job has a start time
            if status.start_time.is_some() {
                return SyncPodStatus::Running;
            }

            // Default to Pending if we can't determine status
            SyncPodStatus::Pending
        }
    }
}
