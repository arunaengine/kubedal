use crate::resource::crd::{Backend, DataNode};
use k8s_openapi::api::core::v1::Secret;
use kube::{Api, Client};
use opendal::layers::{LoggingLayer, RetryLayer};
use opendal::{Builder, Operator, services};
use std::collections::HashMap;
use tonic::Status;

pub async fn get_operator(client: &Client, node: &DataNode) -> Result<Operator, Status> {
    let mut cfg = node.spec.config.clone();

    let secret = match node.spec.secret_ref.as_ref() {
        None => None,
        Some(cred) => {
            let secret_name = &cred.name;
            let secret_ns = cred
                .namespace
                .as_ref()
                .or(node.metadata.namespace.as_ref())
                .ok_or_else(|| Status::invalid_argument("Secret namespace not provided"))?;
            let secret_api: Api<Secret> = Api::namespaced(client.clone(), secret_ns);
            let secret = secret_api
                .get(secret_name)
                .await
                .map_err(|_| Status::not_found("Secret not found"))?;
            Some(secret)
        }
    };
    if let Some(secret) = secret {
        if let Some(data) = secret.data {
            for (k, v) in data.into_iter() {
                cfg.insert(
                    k,
                    std::str::from_utf8(&v.0)
                        .map_err(|e| {
                            Status::internal(format!("Failed to deserialize secret: {}", e))
                        })?
                        .to_string(),
                );
            }
        }
    };

    let op = match node.spec.backend {
        Backend::S3 => init_service::<services::S3>(cfg)?,
        Backend::HTTP => init_service::<services::Http>(cfg)?,
    };

    Ok(op)
}

pub fn init_service<B: Builder>(cfg: HashMap<String, String>) -> Result<Operator, Status> {
    let op = Operator::from_iter::<B>(cfg)
        .map_err(|e| Status::internal(format!("Failed to init openDAL operator: {}", e)))?
        .layer(LoggingLayer::default())
        .layer(RetryLayer::new())
        .finish();

    Ok(op)
}
