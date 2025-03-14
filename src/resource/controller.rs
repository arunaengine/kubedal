use super::{
    datasource_controller::{error_policy_ds, reconcile_ds},
    sync_controller::{error_policy_sy, reconcile_sy},
};
use crate::resource::crd::{Datasource, Sync};
use futures::StreamExt;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::{
    api::{Api, ListParams},
    client::Client,
    runtime::{
        controller::Controller,
        events::Recorder,
        watcher::{self, Config},
    },
};
use std::sync::Arc;
use tracing::*;

// Context for our reconciler
#[derive(Clone)]
pub struct Context {
    /// Kubernetes client
    pub client: Client,
    /// Event recorder
    pub recorder: Recorder,
}

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

/// Initialize the controller and shared state (given the crd is installed)
pub async fn run(client: Client) {
    let ds_api = Api::<Datasource>::all(client.clone());
    if let Err(e) = ds_api.list(&ListParams::default().limit(1)).await {
        error!("CRD Datasource is not queryable; {e:?}. Is the CRD installed?");
        info!("Installation: cargo run --bin crdgen | kubectl apply -f -");
        std::process::exit(1);
    }

    let sync_api = Api::<Sync>::all(client.clone());
    if let Err(e) = sync_api.list(&ListParams::default().limit(1)).await {
        error!("CRD Sync is not queryable; {e:?}. Is the CRD installed?");
        info!("Installation: cargo run --bin crdgen | kubectl apply -f -");
        std::process::exit(1);
    }

    let pod_api = Api::<Pod>::all(client.clone());
    let pvc_api = Api::<PersistentVolumeClaim>::all(client.clone());

    let state = Arc::new(Context {
        client: client.clone(),
        recorder: Recorder::new(client.clone(), "kubedal.arunaengine.org".into()),
    });

    let ds_controller = Controller::new(ds_api, Config::default().any_semantic())
        .shutdown_on_signal()
        .run(reconcile_ds, error_policy_ds, state.clone())
        .for_each(|_| futures::future::ready(())); // We don't care about the result (for now)

    let sync_controller = Controller::new(sync_api, Config::default().any_semantic())
        .owns(pod_api, watcher::Config::default())
        .owns(pvc_api, watcher::Config::default())
        .shutdown_on_signal()
        .run(reconcile_sy, error_policy_sy, state.clone())
        .for_each(|_| futures::future::ready(())); // We don't care about the result (for now)

    tokio::join!(ds_controller, sync_controller);
}
