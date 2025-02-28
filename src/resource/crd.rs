use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Resource is our main custom resource
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kubedal.arunaengine.org",
    version = "v1alpha1",
    kind = "Resource",
    shortname = "res",
    namespaced
)]
pub struct ResourceSpec {
    /// Storage backend type (s3, azblob, gcs, etc.)
    pub backend: Backend,

    /// Bucket or container name
    pub bucket: String,

    /// Path/prefix within the bucket
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Access mode for the resource (default: Cached)
    #[serde(default = "default_access_mode")]
    pub access_mode: AccessMode,

    /// Optional endpoint for the backend
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// Optional region for the backend
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// Credentials for accessing the backend
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<Credentials>,

    /// Additional options for the backend
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub options: HashMap<String, String>,
}

/// Credentials configuration for accessing storage backends
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Credentials {
    /// Secret reference for credentials
    pub secret_ref: SecretRef,
}

/// Reference to a Kubernetes secret
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SecretRef {
    /// Name of the secret
    pub name: String,

    /// Namespace of the secret (optional, defaults to resource namespace)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Resource access mode
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub enum AccessMode {
    /// Read Write access (currently needs FUSE)
    ReadWriteOnce,
    /// Read only access from multiple nodes
    ReadOnlyMany,
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

/// Default access mode is Cached
fn default_access_mode() -> AccessMode {
    AccessMode::ReadOnlyMany
}

/// Status of the Resource
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct ResourceStatus {
    // List of all active volume bindings
    pub bindings: Vec<Binding>,
}

/// Volume binding currently in use
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct Binding {
    pub volume_id: String,
}
