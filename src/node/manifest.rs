//! The manifest of a file-list volume.
//!
//! A manifest is a JSON object stored in a DataNode. It lists the files to place in
//! the volume, empty directories to create, and optionally where to upload the
//! volume content when the pod ends. Example:
//!
//! ```json
//! {
//!   "version": 1,
//!   "files": [
//!     { "path": "inputs/reads.fq", "source": "objects/reads.fq",
//!       "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
//!       "size": 4, "executable": false }
//!   ],
//!   "directories": ["outputs"],
//!   "upload": { "dataNode": "results", "prefix": "runs/42/", "path": "outputs" }
//! }
//! ```

use crate::node::paths::{self, PathError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use tonic::Status;

/// The only manifest version this kubedal understands.
pub const MANIFEST_VERSION: u32 = 1;

/// Manifests larger than this are rejected.
pub const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(
    title = "kubedal file-list manifest",
    description = "Files to place in a kubedal file-list volume, directories to create, \
                   and where to upload the volume content when the pod ends."
)]
pub struct Manifest {
    /// Format version. Must be 1.
    pub version: u32,
    /// Files to place in the volume.
    #[serde(default)]
    pub files: Vec<ManifestFile>,
    /// Directories to create in the volume, for example for outputs.
    #[serde(default)]
    pub directories: Vec<String>,
    /// Where to upload the volume content when the pod ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<ManifestUpload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "One file to place in the volume.")]
pub struct ManifestFile {
    /// Path of the file inside the volume.
    pub path: String,
    /// Path of the object inside the DataNode of the volume.
    pub source: String,
    /// Expected SHA-256 of the content, as 64 hex characters. Files with a digest are
    /// verified and shared through the node content cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Expected size in bytes. Used to check the DataNode limit before downloading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Make the file executable (mode 0755 instead of 0644).
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(description = "Where to upload the volume content when the pod ends.")]
pub struct ManifestUpload {
    /// Name of the DataNode to upload to.
    pub data_node: String,
    /// Namespace of that DataNode. Defaults to the namespace of the pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_node_namespace: Option<String>,
    /// Prefix inside the DataNode.
    #[serde(default)]
    pub prefix: String,
    /// Folder inside the volume to upload. Empty means the whole volume.
    #[serde(default)]
    pub path: String,
}

/// A checked file entry with paths in normal form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub path: String,
    pub source: String,
    pub sha256: Option<String>,
    pub size: Option<u64>,
    pub executable: bool,
}

/// A checked manifest, ready to be placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePlan {
    pub files: Vec<PlannedFile>,
    pub directories: Vec<String>,
    pub upload: Option<PlannedUpload>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedUpload {
    pub data_node: String,
    pub data_node_namespace: Option<String>,
    /// Prefix in the form of `paths::normalize_prefix`.
    pub prefix: String,
    /// Checked relative folder, or "" for the whole volume.
    pub path: String,
}

impl FilePlan {
    /// Sum of the sizes that the manifest declares.
    pub fn declared_size(&self) -> u64 {
        self.files.iter().filter_map(|f| f.size).sum()
    }
}

fn invalid(message: String) -> Status {
    Status::invalid_argument(format!("invalid manifest: {message}"))
}

fn check_path(what: &str, value: &str) -> Result<String, Status> {
    paths::safe_relative(value).map_err(|e: PathError| invalid(format!("{what}: {e}")))
}

fn check_digest(value: &str) -> Result<String, Status> {
    let digest = value.to_ascii_lowercase();
    if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid(format!(
            "sha256 {value:?} must be 64 hexadecimal characters"
        )));
    }
    Ok(digest)
}

/// Parse and check a manifest.
pub fn parse(data: &[u8]) -> Result<FilePlan, Status> {
    // The error must not repeat object content: a wrong manifest path could point
    // to a secret. Only the kind of error and its position are reported.
    let manifest: Manifest = serde_json::from_slice(data).map_err(|e| {
        invalid(format!(
            "not a valid version 1 manifest ({:?} error at line {}, column {})",
            e.classify(),
            e.line(),
            e.column()
        ))
    })?;
    plan(manifest)
}

/// Check a manifest and bring all paths into normal form.
pub fn plan(manifest: Manifest) -> Result<FilePlan, Status> {
    if manifest.version != MANIFEST_VERSION {
        return Err(invalid(format!(
            "version {} is not supported, use version {MANIFEST_VERSION}",
            manifest.version
        )));
    }

    // Every path in the volume is either a file or a directory, never both.
    let mut kinds: BTreeMap<String, bool> = BTreeMap::new(); // true = file
    let add_dir_chain = |path: &str, kinds: &mut BTreeMap<String, bool>| {
        let mut current = String::new();
        for part in path.split('/') {
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(part);
            if kinds.get(&current) == Some(&true) {
                return Err(invalid(format!(
                    "{current:?} is a file and a directory at the same time"
                )));
            }
            kinds.insert(current.clone(), false);
        }
        Ok(())
    };

    let mut files = Vec::with_capacity(manifest.files.len());
    let mut seen = HashSet::new();
    for file in manifest.files {
        let path = check_path("file path", &file.path)?;
        let source = check_path("file source", file.source.trim_start_matches('/'))?;
        if !seen.insert(path.clone()) {
            return Err(invalid(format!("file {path:?} is listed twice")));
        }
        if let Some((parent, _)) = path.rsplit_once('/') {
            add_dir_chain(parent, &mut kinds)?;
        }
        if kinds.get(&path) == Some(&false) {
            return Err(invalid(format!(
                "{path:?} is a file and a directory at the same time"
            )));
        }
        kinds.insert(path.clone(), true);
        files.push(PlannedFile {
            path,
            source,
            sha256: file.sha256.as_deref().map(check_digest).transpose()?,
            size: file.size,
            executable: file.executable,
        });
    }

    let mut directories = Vec::with_capacity(manifest.directories.len());
    for dir in manifest.directories {
        let dir = check_path("directory", &dir)?;
        add_dir_chain(&dir, &mut kinds)?;
        directories.push(dir);
    }

    let upload = match manifest.upload {
        None => None,
        Some(upload) => {
            if upload.data_node.trim().is_empty() {
                return Err(invalid("upload.dataNode must not be empty".into()));
            }
            let prefix = paths::normalize_prefix(&upload.prefix)
                .map_err(|e| invalid(format!("upload prefix: {e}")))?;
            let path = if upload.path.trim_matches('/').is_empty() {
                String::new()
            } else {
                check_path("upload path", &upload.path)?
            };
            Some(PlannedUpload {
                data_node: upload.data_node,
                data_node_namespace: upload.data_node_namespace.filter(|n| !n.is_empty()),
                prefix,
                path,
            })
        }
    };

    Ok(FilePlan {
        files,
        directories,
        upload,
    })
}

/// The JSON schema of the manifest format, as pretty printed JSON.
pub fn json_schema() -> String {
    let schema = schemars::schema_for!(Manifest);
    let mut text = serde_json::to_string_pretty(&schema).unwrap_or_default();
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema file in the repository must match the code.
    /// Run with `UPDATE_SCHEMA=1` to rewrite it.
    #[test]
    fn schema_file_is_current() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/schema/manifest-v1.schema.json"
        );
        let current = json_schema();
        if std::env::var_os("UPDATE_SCHEMA").is_some() {
            std::fs::write(path, &current).unwrap();
        }
        let stored = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            stored, current,
            "run UPDATE_SCHEMA=1 cargo test to refresh {path}"
        );
    }

    const DIGEST: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    fn parse_str(s: &str) -> Result<FilePlan, Status> {
        parse(s.as_bytes())
    }

    #[test]
    fn parses_full_manifest() {
        let plan = parse_str(&format!(
            r#"{{
                "version": 1,
                "files": [
                    {{"path": "inputs/a.txt", "source": "/objects/a", "sha256": "{}", "size": 4}},
                    {{"path": "bin/tool", "source": "tools/tool", "executable": true}}
                ],
                "directories": ["outputs", "tmp/work"],
                "upload": {{"dataNode": "results", "dataNodeNamespace": "shared",
                            "prefix": "/runs/42", "path": "outputs/"}}
            }}"#,
            DIGEST.to_uppercase()
        ))
        .unwrap();
        assert_eq!(plan.files.len(), 2);
        assert_eq!(plan.files[0].source, "objects/a");
        assert_eq!(plan.files[0].sha256.as_deref(), Some(DIGEST));
        assert!(plan.files[1].executable);
        assert_eq!(plan.directories, vec!["outputs", "tmp/work"]);
        assert_eq!(plan.declared_size(), 4);
        let upload = plan.upload.unwrap();
        assert_eq!(upload.prefix, "runs/42/");
        assert_eq!(upload.path, "outputs");
        assert_eq!(upload.data_node_namespace.as_deref(), Some("shared"));
    }

    #[test]
    fn minimal_manifest_is_valid() {
        let plan = parse_str(r#"{"version": 1}"#).unwrap();
        assert!(plan.files.is_empty());
        assert!(plan.upload.is_none());
    }

    #[test]
    fn parse_errors_do_not_repeat_content() {
        for data in [
            r#"{"version": 1, "secret-key": "hunter2"}"#,
            r#"{"version": "hunter2"}"#,
            r#"{"files": [{"path": 5, "source": "hunter2"}], "version": 1}"#,
        ] {
            let err = parse(data.as_bytes()).unwrap_err();
            assert!(!err.message().contains("hunter2"), "{}", err.message());
            assert!(!err.message().contains("secret-key"), "{}", err.message());
        }
    }

    #[test]
    fn rejects_wrong_version_and_unknown_fields() {
        assert!(parse_str(r#"{"version": 2}"#).is_err());
        assert!(parse_str(r#"{"files": []}"#).is_err());
        assert!(parse_str(r#"{"version": 1, "extra": true}"#).is_err());
        assert!(
            parse_str(r#"{"version": 1, "files": [{"path": "a", "source": "b", "mode": 1}]}"#)
                .is_err()
        );
        assert!(parse_str("not json").is_err());
    }

    #[test]
    fn rejects_escaping_paths() {
        for bad in [
            r#"{"version": 1, "files": [{"path": "../a", "source": "b"}]}"#,
            r#"{"version": 1, "files": [{"path": "/etc/a", "source": "b"}]}"#,
            r#"{"version": 1, "files": [{"path": "a", "source": "x/../../b"}]}"#,
            r#"{"version": 1, "directories": [".."]}"#,
            r#"{"version": 1, "upload": {"dataNode": "d", "path": "../x"}}"#,
            r#"{"version": 1, "upload": {"dataNode": "d", "prefix": "a/../../b"}}"#,
        ] {
            let err = parse_str(bad).unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{bad}");
        }
    }

    #[test]
    fn rejects_conflicts() {
        // Same file twice.
        assert!(
            parse_str(
                r#"{"version": 1, "files": [{"path": "a", "source": "x"}, {"path": "./a", "source": "y"}]}"#
            )
            .is_err()
        );
        // A file that is also a parent directory.
        assert!(
            parse_str(
                r#"{"version": 1, "files": [{"path": "a", "source": "x"}, {"path": "a/b", "source": "y"}]}"#
            )
            .is_err()
        );
        assert!(
            parse_str(
                r#"{"version": 1, "files": [{"path": "a/b", "source": "y"}, {"path": "a", "source": "x"}]}"#
            )
            .is_err()
        );
        // A directory with the name of a file.
        assert!(
            parse_str(
                r#"{"version": 1, "files": [{"path": "out", "source": "x"}], "directories": ["out/x"]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_bad_digest_and_empty_upload_node() {
        assert!(
            parse_str(
                r#"{"version": 1, "files": [{"path": "a", "source": "b", "sha256": "abc"}]}"#
            )
            .is_err()
        );
        assert!(parse_str(r#"{"version": 1, "upload": {"dataNode": " "}}"#).is_err());
    }
}
