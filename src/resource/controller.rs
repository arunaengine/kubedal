use crate::resource::crd::{DataNode, DataPod, DataReplicaSet};
use crate::resource::data_node_controller::{error_policy_dn, reconcile_dn};
use crate::resource::data_pod_controller::{error_policy_dp, reconcile_dp};
use futures::StreamExt;
use kube::{
    api::{Api, ListParams},
    client::Client,
    runtime::{controller::Controller, events::Recorder, watcher::Config},
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

    #[error("Tonic error: {0}")]
    TonicError(#[from] tonic::Status),
}

/// Initialize the controller and shared state (given the crd is installed)
pub async fn run(client: Client) {
    let data_node_api = Api::<DataNode>::all(client.clone());
    if let Err(e) = data_node_api.list(&ListParams::default().limit(1)).await {
        error!("CRD DataNode is not queryable; {e:?}. Is the CRD installed?");
        info!("Installation: cargo run --bin crdgen | kubectl apply -f -");
        std::process::exit(1);
    }

    let data_pod_api = Api::<DataPod>::all(client.clone());
    if let Err(e) = data_pod_api.list(&ListParams::default().limit(1)).await {
        error!("CRD DataPod is not queryable; {e:?}. Is the CRD installed?");
        info!("Installation: cargo run --bin crdgen | kubectl apply -f -");
        std::process::exit(1);
    }

    let drs_api = Api::<DataReplicaSet>::all(client.clone());
    if let Err(e) = drs_api.list(&ListParams::default().limit(1)).await {
        error!("CRD DataReplicaSet is not queryable; {e:?}. Is the CRD installed?");
        info!("Installation: cargo run --bin crdgen | kubectl apply -f -");
        std::process::exit(1);
    }

    let state = Arc::new(Context {
        client: client.clone(),
        recorder: Recorder::new(client.clone(), "kubedal.arunaengine.org".into()),
    });

    let state_clone = state.clone();
    let data_node_controller_handle = tokio::spawn(async move {
        Controller::new(data_node_api, Config::default().any_semantic())
            .shutdown_on_signal()
            .run(reconcile_dn, error_policy_dn, state_clone)
            .for_each(|_| futures::future::ready(()))
    });

    let data_pod_controller_handle = tokio::spawn(async move {
        Controller::new(data_pod_api, Config::default().any_semantic())
            .shutdown_on_signal()
            .run(reconcile_dp, error_policy_dp, state.clone())
            .for_each(|_| futures::future::ready(()))
    });

    _ = tokio::join!(data_node_controller_handle, data_pod_controller_handle);
}
