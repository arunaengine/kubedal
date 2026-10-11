//! Read the CSI volume context of a NodePublishVolume call.
//!
//! Two kinds of volumes exist:
//! - Inline (ephemeral) volumes, declared directly in the pod. Their attributes use
//!   short keys like `dataNode` and `path`.
//! - Persistent volumes, created from a PVC. Their context comes from the PVC
//!   annotations (keys with the `kubedal.arunaengine.org/` prefix).

use crate::node::state::{DataNodeRef, PodRef};
use crate::node::volume::{TreeMode, VolumeSource};
use std::collections::HashMap;
use tonic::Status;

/// Inline volume attribute: name of the DataNode.
pub const ATTR_DATA_NODE: &str = "dataNode";
/// Inline volume attribute: namespace of the DataNode (default: the pod namespace).
pub const ATTR_DATA_NODE_NAMESPACE: &str = "dataNodeNamespace";
/// Inline volume attribute: path (prefix) inside the DataNode.
pub const ATTR_PATH: &str = "path";
/// Inline volume attribute: mount mode, see [`parse_mount`].
pub const ATTR_MOUNT: &str = "mount";
/// Inline volume attribute: path of a file-list manifest inside the DataNode.
pub const ATTR_MANIFEST: &str = "manifest";

/// PVC annotation keys, also used in the context of persistent volumes.
pub const ANN_DATA_NODE_NAME: &str = "kubedal.arunaengine.org/data-node-name";
pub const ANN_DATA_NODE_NAMESPACE: &str = "kubedal.arunaengine.org/data-node-namespace";
pub const ANN_DATA_POD_NAME: &str = "kubedal.arunaengine.org/data-pod-name";
pub const ANN_DATA_POD_NAMESPACE: &str = "kubedal.arunaengine.org/data-pod-namespace";
pub const ANN_MOUNT: &str = "kubedal.arunaengine.org/mount";

const POD_NAME: &str = "csi.storage.k8s.io/pod.name";
const POD_NAMESPACE: &str = "csi.storage.k8s.io/pod.namespace";
const POD_UID: &str = "csi.storage.k8s.io/pod.uid";
const POD_SERVICE_ACCOUNT: &str = "csi.storage.k8s.io/serviceAccount.name";
const EPHEMERAL: &str = "csi.storage.k8s.io/ephemeral";

/// What the volume context asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextSource {
    /// The source is fully known.
    Direct(VolumeSource),
    /// A persistent volume that names a DataPod. The DataPod gives the DataNode and path.
    DataPod {
        name: String,
        namespace: String,
        /// DataNode from the PVC annotations, used if the DataPod has none.
        node: Option<DataNodeRef>,
        mode: TreeMode,
        writable: bool,
    },
}

/// Parse a mount mode like `cache-read-only` into a mode and a writable flag.
pub fn parse_mount(value: &str) -> Result<(TreeMode, bool), Status> {
    match value {
        "" | "cache" | "cache-read-only" => Ok((TreeMode::Cache, false)),
        "cache-read-write" => Ok((TreeMode::Cache, true)),
        "fuse" | "fuse-read-only" => Ok((TreeMode::Fuse, false)),
        "fuse-read-write" => Ok((TreeMode::Fuse, true)),
        other => Err(Status::invalid_argument(format!(
            "unsupported mount mode {other:?}; use cache-read-only, cache-read-write, \
             fuse-read-only or fuse-read-write"
        ))),
    }
}

/// Read the pod that the volume is for. Needs `podInfoOnMount: true` in the CSIDriver.
pub fn pod_from_context(ctx: &HashMap<String, String>) -> Result<PodRef, Status> {
    let get =
        |key: &str| {
            ctx.get(key).filter(|v| !v.is_empty()).cloned().ok_or_else(|| {
            Status::invalid_argument(format!(
                "volume context is missing {key}; is podInfoOnMount enabled in the CSIDriver?"
            ))
        })
        };
    Ok(PodRef {
        name: get(POD_NAME)?,
        namespace: get(POD_NAMESPACE)?,
        uid: get(POD_UID)?,
        service_account: get(POD_SERVICE_ACCOUNT)?,
    })
}

pub fn is_ephemeral(ctx: &HashMap<String, String>) -> bool {
    ctx.get(EPHEMERAL).is_some_and(|v| v == "true")
}

/// Work out the volume source from the context.
pub fn source_from_context(
    ctx: &HashMap<String, String>,
    pod: &PodRef,
) -> Result<ContextSource, Status> {
    if ctx.contains_key(ATTR_DATA_NODE) || is_ephemeral(ctx) {
        return inline_source(ctx, pod);
    }
    persistent_source(ctx, pod)
}

fn non_empty<'a>(ctx: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    ctx.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// A storage path attribute exactly as given: it is not trimmed, so it can
/// never name another object than the one written. Empty means not set.
fn exact_attribute<'a>(
    ctx: &'a HashMap<String, String>,
    key: &str,
) -> Result<Option<&'a str>, Status> {
    let Some(value) = ctx.get(key).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if crate::util::opendal::exact_user_key(value).is_none() {
        return Err(Status::invalid_argument(format!(
            "volume attribute {key} {value:?} would name another storage path \
             (white space at the ends, empty, \".\" or \"..\" parts)"
        )));
    }
    Ok(Some(value))
}

fn inline_source(ctx: &HashMap<String, String>, pod: &PodRef) -> Result<ContextSource, Status> {
    let name = non_empty(ctx, ATTR_DATA_NODE).ok_or_else(|| {
        Status::invalid_argument(format!(
            "inline volume needs the volume attribute {ATTR_DATA_NODE}"
        ))
    })?;
    let node = DataNodeRef {
        name: name.to_string(),
        namespace: non_empty(ctx, ATTR_DATA_NODE_NAMESPACE)
            .unwrap_or(&pod.namespace)
            .to_string(),
    };

    if let Some(manifest) = exact_attribute(ctx, ATTR_MANIFEST)? {
        return Ok(ContextSource::Direct(VolumeSource::FileList {
            node,
            manifest: manifest.to_string(),
        }));
    }

    let (mode, writable) = parse_mount(non_empty(ctx, ATTR_MOUNT).unwrap_or(""))?;
    Ok(ContextSource::Direct(VolumeSource::Tree {
        node,
        path: exact_attribute(ctx, ATTR_PATH)?.unwrap_or("/").to_string(),
        mode,
        writable,
    }))
}

fn persistent_source(ctx: &HashMap<String, String>, pod: &PodRef) -> Result<ContextSource, Status> {
    let (mode, writable) = parse_mount(non_empty(ctx, ANN_MOUNT).unwrap_or(""))?;
    let node = non_empty(ctx, ANN_DATA_NODE_NAME).map(|name| DataNodeRef {
        name: name.to_string(),
        namespace: non_empty(ctx, ANN_DATA_NODE_NAMESPACE)
            .unwrap_or(&pod.namespace)
            .to_string(),
    });

    if let Some(name) = non_empty(ctx, ANN_DATA_POD_NAME) {
        return Ok(ContextSource::DataPod {
            name: name.to_string(),
            namespace: non_empty(ctx, ANN_DATA_POD_NAMESPACE)
                .unwrap_or(&pod.namespace)
                .to_string(),
            node,
            mode,
            writable,
        });
    }

    match node {
        Some(node) => Ok(ContextSource::Direct(VolumeSource::Tree {
            node,
            path: "/".into(),
            mode,
            writable,
        })),
        None => Err(Status::invalid_argument(format!(
            "volume context names neither a DataPod ({ANN_DATA_POD_NAME}) nor a DataNode \
             ({ANN_DATA_NODE_NAME} or {ATTR_DATA_NODE})"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod_ctx(extra: &[(&str, &str)]) -> HashMap<String, String> {
        let mut ctx: HashMap<String, String> = [
            (POD_NAME, "task"),
            (POD_NAMESPACE, "team"),
            (POD_UID, "uid-1"),
            (POD_SERVICE_ACCOUNT, "default"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in extra {
            ctx.insert(k.to_string(), v.to_string());
        }
        ctx
    }

    #[test]
    fn storage_paths_are_taken_exactly_as_given() {
        for (attr, value) in [
            (ATTR_PATH, "x "),
            (ATTR_PATH, "a//b"),
            (ATTR_PATH, "a/./b"),
            (ATTR_MANIFEST, "m /"),
            (ATTR_MANIFEST, " m"),
        ] {
            let ctx = pod_ctx(&[
                (EPHEMERAL, "true"),
                (ATTR_DATA_NODE, "store"),
                (attr, value),
            ]);
            let pod = pod_from_context(&ctx).unwrap();
            let err = source_from_context(&ctx, &pod).unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument, "{attr}={value:?}");
        }
        // One leading "/" is fine.
        let ctx = pod_ctx(&[
            (EPHEMERAL, "true"),
            (ATTR_DATA_NODE, "store"),
            (ATTR_PATH, "/a b"),
        ]);
        let pod = pod_from_context(&ctx).unwrap();
        assert!(source_from_context(&ctx, &pod).is_ok());
    }

    #[test]
    fn reads_pod_info() {
        let pod = pod_from_context(&pod_ctx(&[])).unwrap();
        assert_eq!(pod.name, "task");
        assert_eq!(pod.namespace, "team");
        let err = pod_from_context(&HashMap::new()).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn inline_defaults_to_cache_read_only_in_pod_namespace() {
        let ctx = pod_ctx(&[(EPHEMERAL, "true"), (ATTR_DATA_NODE, "store")]);
        let pod = pod_from_context(&ctx).unwrap();
        let source = source_from_context(&ctx, &pod).unwrap();
        assert_eq!(
            source,
            ContextSource::Direct(VolumeSource::Tree {
                node: DataNodeRef {
                    name: "store".into(),
                    namespace: "team".into()
                },
                path: "/".into(),
                mode: TreeMode::Cache,
                writable: false,
            })
        );
    }

    #[test]
    fn inline_reads_all_attributes() {
        let ctx = pod_ctx(&[
            (EPHEMERAL, "true"),
            (ATTR_DATA_NODE, "store"),
            (ATTR_DATA_NODE_NAMESPACE, "shared"),
            (ATTR_PATH, "inputs/run-1"),
            (ATTR_MOUNT, "fuse-read-write"),
        ]);
        let pod = pod_from_context(&ctx).unwrap();
        let ContextSource::Direct(VolumeSource::Tree {
            node,
            path,
            mode,
            writable,
        }) = source_from_context(&ctx, &pod).unwrap()
        else {
            panic!("expected a tree source");
        };
        assert_eq!(node.namespace, "shared");
        assert_eq!(path, "inputs/run-1");
        assert_eq!(mode, TreeMode::Fuse);
        assert!(writable);
    }

    #[test]
    fn inline_manifest_gives_file_list() {
        let ctx = pod_ctx(&[
            (EPHEMERAL, "true"),
            (ATTR_DATA_NODE, "store"),
            (ATTR_MANIFEST, "jobs/1/manifest.json"),
        ]);
        let pod = pod_from_context(&ctx).unwrap();
        assert!(matches!(
            source_from_context(&ctx, &pod).unwrap(),
            ContextSource::Direct(VolumeSource::FileList { manifest, .. }) if manifest == "jobs/1/manifest.json"
        ));
    }

    #[test]
    fn inline_without_data_node_fails() {
        let ctx = pod_ctx(&[(EPHEMERAL, "true")]);
        let pod = pod_from_context(&ctx).unwrap();
        let err = source_from_context(&ctx, &pod).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn persistent_with_data_pod() {
        let ctx = pod_ctx(&[
            (ANN_DATA_POD_NAME, "dp"),
            (ANN_DATA_NODE_NAME, "dn"),
            (ANN_MOUNT, "cache-read-write"),
        ]);
        let pod = pod_from_context(&ctx).unwrap();
        assert_eq!(
            source_from_context(&ctx, &pod).unwrap(),
            ContextSource::DataPod {
                name: "dp".into(),
                namespace: "team".into(),
                node: Some(DataNodeRef {
                    name: "dn".into(),
                    namespace: "team".into()
                }),
                mode: TreeMode::Cache,
                writable: true,
            }
        );
    }

    #[test]
    fn unknown_mount_mode_fails() {
        assert!(parse_mount("nfs").is_err());
        assert_eq!(parse_mount("fuse").unwrap(), (TreeMode::Fuse, false));
    }
}
