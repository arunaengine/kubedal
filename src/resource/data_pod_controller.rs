use crate::resource::crd::{DataPod, DataPodStatus};
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
        let data_pod = data_pod_api.get(&name).await.map_err(Error::KubeError)?;

        // always overwrite status object with what we saw
        let new_status = Patch::Apply(json!({
            "apiVersion": DataPod::api_version(&()),
            "kind": DataPod::kind(&()),
            "status": DataPodStatus {
                available: true,
                generated_path: data_pod.spec.path.is_some(),
            }
        }));

        //TODO: Patch resource
        //  - Set owner reference and datNodeRef depending on ref/selector
        //  - Set path if changed (and generated_path if necessary)

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

        // If no events were received, check back every 1 minute
        Ok(Action::requeue(Duration::from_secs(1 * 60)))
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
