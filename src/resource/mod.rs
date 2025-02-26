use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Represents our custom "Resource" CRD
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub metadata: ResourceMetadata,
    pub spec: ResourceSpec,
    #[serde(default)]
    pub status: ResourceStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceMetadata {
    pub name: String,
    pub namespace: String,
    #[serde(default)]
    pub annotations: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSpec {
    /// The storage backend type (s3, azblob, gcs, etc.)
    pub backend: String,
    
    /// Endpoint for the backend (e.g. S3 endpoint)
    #[serde(default)]
    pub endpoint: Option<String>,
    
    /// Region for the backend (e.g. S3 region)
    #[serde(default)]
    pub region: Option<String>,
    
    /// Bucket name
    #[serde(default)]
    pub bucket: Option<String>,
    
    /// Path/prefix within the bucket
    #[serde(default)]
    pub path: Option<String>,
    
    /// Secret reference for credentials
    #[serde(default)]
    pub secret_ref: Option<SecretRef>,
    
    /// Access mode (cached or direct)
    #[serde(default = "default_access_mode")]
    pub access_mode: AccessMode,
    
    /// Additional options for the backend
    #[serde(default)]
    pub options: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretRef {
    pub name: String,
    pub namespace: String,
    
    /// Key in the secret for access key/id
    #[serde(default = "default_access_key")]
    pub access_key: String,
    
    /// Key in the secret for secret key
    #[serde(default = "default_secret_key")]
    pub secret_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AccessMode {
    /// Download and cache data locally
    Cached,
    /// Mount via FUSE directly
    Direct,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ResourceStatus {
    /// Last time the resource was successfully accessed
    pub last_accessed: Option<String>,
    
    /// Current state of the resource
    pub state: ResourceState,
    
    /// Any error message if in error state
    pub error: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub enum ResourceState {
    #[default]
    Pending,
    Ready,
    Error,
}

fn default_access_mode() -> AccessMode {
    AccessMode::Cached
}

fn default_access_key() -> String {
    "access_key".to_string()
}

fn default_secret_key() -> String {
    "secret_key".to_string()
}

/// Helper to create an OpenDAL operator from a Resource
pub fn create_opendal_operator(resource: &Resource) -> Result<(), String> {
    // In a real implementation, this would:
    // 1. Fetch credentials from the referenced secret if specified
    // 2. Create an OpenDAL Operator with the appropriate backend
    // 3. Return the operator for use in volume operations
    
    // For our dummy implementation, we'll just validate and log
    match resource.spec.backend.as_str() {
        "s3" => {
            if resource.spec.bucket.is_none() {
                return Err("S3 backend requires a bucket".to_string());
            }
            log::info!(
                "Would create S3 operator for bucket: {}, path: {}", 
                resource.spec.bucket.as_ref().unwrap(),
                resource.spec.path.as_ref().unwrap_or(&"".to_string())
            );
        },
        "azblob" => {
            if resource.spec.bucket.is_none() {
                return Err("Azure Blob backend requires a container".to_string());
            }
            log::info!(
                "Would create Azure Blob operator for container: {}, path: {}", 
                resource.spec.bucket.as_ref().unwrap(),
                resource.spec.path.as_ref().unwrap_or(&"".to_string())
            );
        },
        _ => {
            return Err(format!("Unsupported backend: {}", resource.spec.backend));
        }
    }
    
    Ok(())
}
