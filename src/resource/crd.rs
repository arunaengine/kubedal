use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub enum MountAccess {
    CacheReadWrite,
    CacheReadOnly,
    FuseReadWrite,
    FuseReadOnly,
}

impl fmt::Display for MountAccess {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MountAccess::CacheReadWrite => write!(f, "cache-read-write"),
            MountAccess::CacheReadOnly => write!(f, "cache-read-only"),
            MountAccess::FuseReadWrite => write!(f, "fuse-read-write"),
            MountAccess::FuseReadOnly => write!(f, "fuse-read-only"),
        }
    }
}

/// Datanode is a custom resource for defining data location, it is similar to a K8s Node
/// but for data sources. It can be used to define a data source and its access configuration.
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kubedal.arunaengine.org",
    version = "v1alpha1",
    kind = "DataNode",
    shortname = "dn",
    status = "DataNodeStatus",
    namespaced
)]
pub struct DataNodeSpec {
    /// Storage backend scheme (s3, azblob, gcs, etc.)
    pub backend: Backend,

    /// Is the resource read-only
    pub read_only: bool,

    /// Secrets / credentials for accessing the backend
    /// Tokens, keys, etc. can be stored in a Kubernetes secret
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "secretRef")]
    pub secret_ref: Option<Ref>,

    /// Additional config options for the backend
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub config: HashMap<String, String>,

    /// Maximum storage capacity
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<Quantity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct DataNodeStatus {
    pub available: bool,
    pub used: Quantity,
}

#[derive(CustomResource, Default, Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[kube(
    group = "kubedal.arunaengine.org",
    version = "v1alpha1",
    kind = "DataPod",
    shortname = "dp",
    status = "DataPodStatus",
    namespaced
)]
#[kube(derive = "PartialEq")]
pub struct DataPodSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(rename = "dataNodeRef")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_node_ref: Option<Ref>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "dataNodeSelector")]
    pub data_node_selector: Option<BTreeMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<Quantity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct DataPodStatus {
    pub available: bool,
    pub generated_path: bool,
}

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kubedal.arunaengine.org",
    version = "v1alpha1",
    kind = "DataReplicaSet",
    shortname = "drs",
    status = "DataReplicaSetStatus",
    namespaced
)]
pub struct DataReplicaSetSpec {
    pub replicas: u32,
    pub selector: MatchLabels,
    #[serde(rename = "sourceDataPodRef")]
    pub source_pod: Ref,
    pub template: DataReplicaSetSpecTemplate,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DataReplicaSetSpecTemplate {
    pub metadata: LabelsMetadata,
    pub spec: DataPodSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LabelsMetadata {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct DataReplicaSetStatus {
    pub available: bool,
}

/// Reference to a Kubernetes resource
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Ref {
    /// Name of the resource
    pub name: String,

    /// Namespace of the resource (optional, defaults to resource namespace)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Resource access mode
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub enum Backend {
    /// S3 compatible storage
    S3,
    /// HTTP storage, read-only
    HTTP,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct MatchLabels {
    #[serde(rename = "matchLabels")]
    pub labels: BTreeMap<String, String>,
}
