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
    /// Storage backend scheme (s3, azblob, gcs, etc.)
    pub backend: Backend,

    /// Access mode for the resource (default: Cached)
    #[serde(default = "default_access_mode")]
    pub access_mode: AccessMode,

    /// Mount mode for the resource
    #[serde(default)]
    pub mount: MountMode,

    /// Credentials for accessing the backend
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<Credentials>,

    /// Additional config options for the backend
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub config: HashMap<String, String>,
}

/// Credentials configuration for accessing storage backends
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Credentials {
    /// Secret reference for credentials
    #[serde(rename = "secretRef")]
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

/// Mount mode for the resource
#[derive(Default, Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub enum MountMode {
    /// Mount the resource as a cached local directory
    #[default]
    Cached,
    /// Mount the resource via fuse
    Fuse,
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
