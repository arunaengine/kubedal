use crate::resource::crd::{Backend, DataNode, Ref};
use crate::util::error::kube_status;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::{Api, Client, ResourceExt};
use opendal::layers::{LoggingLayer, RetryLayer};
use opendal::{Builder, Operator, services};
use std::collections::HashMap;
use tonic::Status;

/// True if OpenDAL uses `key` exactly as it is, so two different keys never
/// reach the same object. Such a key has no leading "/", no white space at its
/// ends, and no empty, "." or ".." parts (a trailing "/" for a folder is fine,
/// but the key without it must be exact too). The empty key is the root.
pub fn is_exact_key(key: &str) -> bool {
    if key.is_empty() {
        return true;
    }
    let file = key.strip_suffix('/').unwrap_or(key);
    let exact = |k: &str| opendal::raw::normalize_path(k) == k;
    let parts_ok = file
        .split('/')
        .all(|part| !part.is_empty() && part != "." && part != "..");
    !key.starts_with('/') && parts_ok && exact(key) && exact(file)
}

/// A user-given storage path with at most one leading "/" removed, if it is an
/// exact key (see [`is_exact_key`]). Nothing else is trimmed or changed.
pub fn exact_user_key(path: &str) -> Option<&str> {
    let key = path.strip_prefix('/').unwrap_or(path);
    is_exact_key(key).then_some(key)
}

/// Build an operator for the root of a DataNode.
pub async fn get_operator(client: &Client, node: &DataNode) -> Result<Operator, Status> {
    let cfg = data_node_config(client, node).await?;
    build_operator(&node.spec.backend, cfg, "")
}

/// The OpenDAL configuration of a DataNode: the values of its ConfigMap, then
/// of its secret, then its `config` section. A later source wins.
///
/// The ConfigMap and the secret must live in the namespace of the DataNode.
/// Otherwise anybody who may create a DataNode could make kubedal read secrets
/// from other namespaces.
pub async fn data_node_config(
    client: &Client,
    node: &DataNode,
) -> Result<HashMap<String, String>, Status> {
    let node_ns = node
        .namespace()
        .ok_or_else(|| Status::invalid_argument("DataNode has no namespace"))?;
    let check_namespace = |kind: &str, reference: &Ref| match reference.namespace.as_ref() {
        Some(ns) if ns != &node_ns => Err(Status::permission_denied(format!(
            "DataNode {node_ns}/{} refers to a {kind} in namespace {ns}; \
             the {kind} must be in the namespace of the DataNode",
            node.name_any()
        ))),
        _ => Ok(()),
    };

    let mut config_map = HashMap::new();
    if let Some(map_ref) = node.spec.config_map_ref.as_ref() {
        check_namespace("ConfigMap", map_ref)?;
        let map = Api::<ConfigMap>::namespaced(client.clone(), &node_ns)
            .get(&map_ref.name)
            .await
            .map_err(|e| kube_status(e, &format!("ConfigMap {node_ns}/{}", map_ref.name)))?;
        config_map.extend(map.data.unwrap_or_default());
    }

    let mut secret_values = HashMap::new();
    if let Some(secret_ref) = node.spec.secret_ref.as_ref() {
        check_namespace("secret", secret_ref)?;
        let secret = Api::<Secret>::namespaced(client.clone(), &node_ns)
            .get(&secret_ref.name)
            .await
            .map_err(|e| kube_status(e, &format!("Secret {node_ns}/{}", secret_ref.name)))?;
        for (key, value) in secret.data.unwrap_or_default() {
            let value = String::from_utf8(value.0).map_err(|_| {
                Status::invalid_argument(format!(
                    "value of key {key} in secret {} is not UTF-8",
                    secret_ref.name
                ))
            })?;
            secret_values.insert(key, value);
        }
    }
    Ok(merge_config(config_map, secret_values, &node.spec.config))
}

/// Merge the configuration sources of a DataNode. Secret values override
/// ConfigMap values, and the `config` section overrides both.
pub fn merge_config(
    config_map: HashMap<String, String>,
    secret: HashMap<String, String>,
    spec: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut cfg = config_map;
    cfg.extend(secret);
    cfg.extend(spec.clone());
    cfg
}

/// Join a sub path to the `root` option of a backend configuration.
fn with_root(mut cfg: HashMap<String, String>, sub_root: &str) -> HashMap<String, String> {
    let sub_root = sub_root.trim_matches('/');
    if !sub_root.is_empty() {
        let base = cfg
            .get("root")
            .map(|r| r.trim_end_matches('/'))
            .unwrap_or("");
        let root = format!("{base}/{sub_root}/");
        cfg.insert("root".to_string(), root);
    }
    cfg
}

/// Build an operator for a backend, rooted at `sub_root` inside the configured root.
pub fn build_operator(
    backend: &Backend,
    cfg: HashMap<String, String>,
    sub_root: &str,
) -> Result<Operator, Status> {
    let cfg = with_root(cfg, sub_root);
    match backend {
        Backend::S3 => init_service::<services::S3>(cfg),
        Backend::HTTP => init_service::<services::Http>(cfg),
        Backend::GCS => init_service::<services::Gcs>(cfg),
        Backend::Azblob => init_service::<services::Azblob>(cfg),
    }
}

pub fn init_service<B: Builder>(cfg: HashMap<String, String>) -> Result<Operator, Status> {
    let op = Operator::from_iter::<B>(cfg)
        .map_err(|e| {
            Status::invalid_argument(format!("invalid DataNode storage configuration: {e}"))
        })?
        .layer(LoggingLayer::default())
        .layer(RetryLayer::new());

    Ok(op)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_joined() {
        let cfg = HashMap::from([("root".to_string(), "/base/".to_string())]);
        assert_eq!(with_root(cfg.clone(), "")["root"], "/base/");
        assert_eq!(with_root(cfg, "/data/x/")["root"], "/base/data/x/");
        assert_eq!(with_root(HashMap::new(), "data")["root"], "/data/");
    }

    #[test]
    fn config_section_beats_secret_beats_config_map() {
        let map = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let cfg = merge_config(
            map(&[("bucket", "shared"), ("region", "eu"), ("endpoint", "a")]),
            map(&[("endpoint", "b"), ("access_key_id", "AKIA")]),
            &map(&[("bucket", "mine")]),
        );
        assert_eq!(cfg["bucket"], "mine");
        assert_eq!(cfg["endpoint"], "b");
        assert_eq!(cfg["region"], "eu");
        assert_eq!(cfg["access_key_id"], "AKIA");
    }

    #[test]
    fn invalid_config_is_invalid_argument() {
        // S3 needs a bucket.
        let err = build_operator(&Backend::S3, HashMap::new(), "").unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn s3_operator_builds() {
        let cfg = HashMap::from([
            ("bucket".to_string(), "b".to_string()),
            ("region".to_string(), "us-east-1".to_string()),
            ("endpoint".to_string(), "http://localhost:9000".to_string()),
        ]);
        let op = build_operator(&Backend::S3, cfg, "data").unwrap();
        assert_eq!(op.info().root(), "/data/");
    }
}
