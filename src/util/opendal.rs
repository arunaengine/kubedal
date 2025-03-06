use crate::resource::crd::Backend;
use opendal::layers::{LoggingLayer, RetryLayer};
use opendal::{Builder, Operator, services};
use std::collections::HashMap;
use tonic::Status;

pub fn get_operator(backend: Backend, cfg: HashMap<String, String>) -> Result<Operator, Status> {
    let op = match backend {
        Backend::S3 => init_service::<services::S3>(cfg)?,
        Backend::HTTP => init_service::<services::Http>(cfg)?,
        //_ => return Err(Status::invalid_argument("Backend not supported")),
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
