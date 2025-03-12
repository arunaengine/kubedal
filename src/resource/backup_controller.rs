use super::controller::{Context, Error};
use crate::resource::crd::{Backup, BackupStatus};
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

const BACKUP_FINALIZER: &str = "kubedal.arunaengine.org/backup";

#[instrument(skip(ctx, res), fields(trace_id))]
pub async fn reconcile_ba(res: Arc<Backup>, ctx: Arc<Context>) -> Result<Action, Error> {
    let ns = res.namespace().unwrap(); // res is namespace scoped
    let ds: Api<Backup> = Api::namespaced(ctx.client.clone(), &ns);

    info!(
        "Reconciling Backup operation \"{}\" in {}",
        res.name_any(),
        ns
    );
    finalizer(&ds, BACKUP_FINALIZER, res, |event| async {
        match event {
            Finalizer::Apply(doc) => doc.reconcile(ctx.clone()).await,
            Finalizer::Cleanup(doc) => doc.cleanup(ctx.clone()).await,
        }
    })
    .await
    .map_err(|e| Error::FinalizerError(Box::new(e)))
}

pub fn error_policy_ba(doc: Arc<Backup>, error: &Error, _ctx: Arc<Context>) -> Action {
    warn!("reconcile failed: {:?}, {:?}", error, doc);
    Action::requeue(Duration::from_secs(5 * 60))
}

impl Backup {
    // Reconcile (for non-finalizer related changes)
    async fn reconcile(&self, ctx: Arc<Context>) -> Result<Action, Error> {
        if self.status.is_some() {
            return Ok(Action::requeue(Duration::from_secs(30 * 60)));
        }

        // If the status is already initialized, do nothing
        let client = ctx.client.clone();
        let ns = self
            .namespace()
            .ok_or_else(|| Error::ReconcilerError("Missing namespace".into()))?;

        let name = self.name_any();
        let docs: Api<Backup> = Api::namespaced(client, &ns);

        // always overwrite status object with what we saw
        let new_status = Patch::Apply(json!({
            "status": BackupStatus {
                status: "Initialized".to_string(),
            }
        }));

        trace!("Patching status for {}", name);
        let ps = PatchParams::apply("cntrlr").force();
        let _o = docs
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

        // If no events were received, check back every 30 minutes
        Ok(Action::requeue(Duration::from_secs(30 * 60)))
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
