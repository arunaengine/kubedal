use crate::resource::crd::{Datasource, DatasourceStatus};
use futures::StreamExt;
use kube::{
    Resource,
    api::{Api, ListParams, Patch, PatchParams, ResourceExt},
    client::Client,
    runtime::{
        controller::{Action, Controller},
        events::{Event, EventType, Recorder},
        finalizer::{Event as Finalizer, finalizer},
        watcher::Config,
    },
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tracing::*;

// Context for our reconciler
#[derive(Clone)]
pub struct Context {
    /// Kubernetes client
    pub client: Client,
    /// Event recorder
    pub recorder: Recorder,
}

const RESOURCE_FINALIZER: &str = "kubedal.arunaengine.org/resource";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Kube Error: {0}")]
    KubeError(#[from] kube::Error),

    #[error("Reconciler Error: {0}")]
    ReconcilerError(String),

    #[error("Missing Secret: {0}")]
    MissingSecret(String),

    #[error("Secret Access Error: {0}")]
    SecretAccessError(String),

    #[error("Finalizer Error: {0}")]
    FinalizerError(#[source] Box<kube::runtime::finalizer::Error<Error>>),
}

#[instrument(skip(ctx, res), fields(trace_id))]
async fn reconcile(res: Arc<Datasource>, ctx: Arc<Context>) -> Result<Action, Error> {
    let ns = res.namespace().unwrap(); // res is namespace scoped
    let ress: Api<Datasource> = Api::namespaced(ctx.client.clone(), &ns);

    info!("Reconciling Document \"{}\" in {}", res.name_any(), ns);
    finalizer(&ress, RESOURCE_FINALIZER, res, |event| async {
        match event {
            Finalizer::Apply(doc) => doc.reconcile(ctx.clone()).await,
            Finalizer::Cleanup(doc) => doc.cleanup(ctx.clone()).await,
        }
    })
    .await
    .map_err(|e| Error::FinalizerError(Box::new(e)))
}

fn error_policy(doc: Arc<Datasource>, error: &Error, _ctx: Arc<Context>) -> Action {
    warn!("reconcile failed: {:?}, {:?}", error, doc);
    Action::requeue(Duration::from_secs(5 * 60))
}

impl Datasource {
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
        let docs: Api<Datasource> = Api::namespaced(client, &ns);

        // always overwrite status object with what we saw
        let new_status = Patch::Apply(json!({
            "status": DatasourceStatus {
                bindings: self.status.as_ref().map_or_else(std::vec::Vec::new, |s| s.bindings.clone()),
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

/// Initialize the controller and shared state (given the crd is installed)
pub async fn run(client: Client) {
    let docs = Api::<Datasource>::all(client.clone());
    if let Err(e) = docs.list(&ListParams::default().limit(1)).await {
        error!("CRD is not queryable; {e:?}. Is the CRD installed?");
        info!("Installation: cargo run --bin crdgen | kubectl apply -f -");
        std::process::exit(1);
    }

    let state = Arc::new(Context {
        client: client.clone(),
        recorder: Recorder::new(client.clone(), "kubedal.arunaengine.org".into()),
    });

    Controller::new(docs, Config::default().any_semantic())
        .shutdown_on_signal()
        .run(reconcile, error_policy, state)
        .filter_map(|x| async move { std::result::Result::ok(x) })
        .for_each(|_| futures::future::ready(()))
        .await;
}
