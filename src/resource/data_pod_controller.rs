use crate::resource::crd::{DataNode, DataPod, DataPodSpec, DataPodStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::core::PartialObjectMetaExt;
use kube::{
    Resource,
    api::{Api, Patch, PatchParams, ResourceExt},
    runtime::{
        controller::Action,
        events::{Event, EventType},
        finalizer::{Event as Finalizer, finalizer},
    },
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tracing::*;

const SYNC_FINALIZER: &str = "kubedal.arunaengine.org/datapod";

use super::controller::{Context, Error};

#[instrument(skip(ctx, res), fields(trace_id))]
pub async fn reconcile_dp(res: Arc<DataPod>, ctx: Arc<Context>) -> Result<Action, Error> {
    let ns = res.namespace().unwrap(); // res is namespace scoped
    let dp: Api<DataPod> = Api::namespaced(ctx.client.clone(), &ns);

    info!("Reconciling DataPod \"{}\" in {}", res.name_any(), ns);
    finalizer(&dp, SYNC_FINALIZER, res, |event| async {
        match event {
            Finalizer::Apply(doc) => doc.reconcile(ctx.clone()).await,
            Finalizer::Cleanup(doc) => doc.cleanup(ctx.clone()).await,
        }
    })
    .await
    .map_err(|e| Error::FinalizerError(Box::new(e)))
}

pub fn error_policy_dp(doc: Arc<DataPod>, error: &Error, _ctx: Arc<Context>) -> Action {
    warn!("reconcile failed: {:?}, {:?}", error, doc);
    Action::requeue(Duration::from_secs(5 * 60))
}

impl DataPod {
    // Reconcile (for non-finalizer related changes)
    async fn reconcile(&self, ctx: Arc<Context>) -> Result<Action, Error> {
        // If the status is already initialized, do nothing
        let client = ctx.client.clone();
        let ns = self
            .namespace()
            .ok_or_else(|| Error::ReconcilerError("Missing namespace".into()))?;

        // Fetch DataPod
        let name = self.name_any();
        let data_pod_api: Api<DataPod> = Api::namespaced(client.clone(), &ns);

        // Set path to DataPod uid if None, empty or "/"
        let (new_path, generated) = if self
            .spec
            .path
            .as_ref()
            .map_or(true, |path| path.is_empty() || path == "/")
        {
            (
                format!(
                    "/{}",
                    self.uid().expect("DataPods uid is missing.").as_str()
                ),
                true,
            )
        } else {
            (self.spec.path.clone().unwrap(), false) // Cannot fail as the other branch collects all the other cases.
        };

        let path_patch = Patch::Apply(json!({
            "apiVersion": DataPod::api_version(&()),
            "kind": DataPod::kind(&()),
            "spec": DataPodSpec {
                path: Some(new_path),
                ..Default::default()
            }
        }));
        data_pod_api
            .patch(&name, &PatchParams::apply("cntrlr").force(), &path_patch)
            .await
            .inspect_err(|e| error!("Failed to patch path: {:?}", e))
            .map_err(Error::KubeError)?;

        //TODO: Set owner reference and datNodeRef depending on ref/selector
        if let Some(node_ref) = &self.spec.data_node_ref {
            let data_node_api: Api<DataNode> = Api::namespaced(client.clone(), &ns);
            let data_node = data_node_api
                .get(&node_ref.name)
                .await
                .map_err(Error::KubeError)?;

            let owner = OwnerReference {
                api_version: DataNode::api_version(&()).to_string(),
                block_owner_deletion: None,
                controller: None,
                kind: DataNode::kind(&()).to_string(),
                name: data_node.name_any(),
                uid: data_node.metadata.uid.unwrap(),
            };

            let metadata_patch = Patch::Apply(
                ObjectMeta {
                    owner_references: Some(vec![owner]),
                    ..Default::default()
                }
                .into_request_partial::<DataPod>(),
            );
            data_pod_api
                .patch_metadata(&name, &PatchParams::apply("cntrlr"), &metadata_patch)
                .await?;
            info!("PartialMetadataPatch for {} succeeded.", &name)
        } else if let Some(selector) = &self.spec.data_node_selector {
            //TODO:
            //  - Fetch DataNodes with selector
            //  - Take first result and set as owner (?)
        } else {
            return Err(Error::ReconcilerError(format!("DataPod {} has no DataNode reference", &name)));
        }

        // always overwrite status object with what we saw
        let new_status = Patch::Apply(json!({
            "apiVersion": DataPod::api_version(&()),
            "kind": DataPod::kind(&()),
            "status": DataPodStatus {
                available: true,
                generated_path: generated,
            }
        }));

        //  - Set generated_path if changed
        trace!("Patching status for {}", name);
        let ps = PatchParams::apply("cntrlr").force();
        let new_data_pod = data_pod_api
            .patch_status(&name, &ps, &new_status)
            .await
            .inspect_err(|e| error!("Failed to patch status: {:?}", e))
            .map_err(Error::KubeError)?;

        if self.status != new_data_pod.status {
            ctx.recorder
                .publish(
                    &Event {
                        type_: EventType::Normal,
                        reason: format!(
                            "Status changed: {:?}",
                            new_data_pod
                                .status
                                .as_ref()
                                .map(|s| s.generated_path.clone())
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

        // If no events were received, check back every 5 minutes
        Ok(Action::requeue(Duration::from_secs(5 * 60)))
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
