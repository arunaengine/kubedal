//! Volume state that survives a restart of the node plugin.
//!
//! Each published volume has one small JSON file. The files hold no credentials.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// A pod that uses a volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodRef {
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub service_account: String,
}

/// A DataNode, always with its namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataNodeRef {
    pub name: String,
    pub namespace: String,
}

impl std::fmt::Display for DataNodeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.namespace, self.name)
    }
}

/// How the volume content reaches the pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VolumeMode {
    /// Files were copied to the node and bind mounted.
    Cache,
    /// A FUSE file system serves the files directly from storage.
    Fuse,
    /// Files listed in a manifest were copied to the node and bind mounted.
    FileList,
}

/// Where a volume is in its life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    /// Content is being prepared. Data may be incomplete.
    Preparing,
    /// The volume is mounted at the target path.
    Published,
    /// Unpublish started. Local data must stay until the upload is done.
    Unpublishing,
}

/// What to upload when the volume is unpublished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadPlan {
    pub data_node: DataNodeRef,
    /// Prefix inside the DataNode, in the form of `paths::normalize_prefix`.
    pub prefix: String,
    /// Folder inside the volume to upload. Empty means the whole volume.
    #[serde(default)]
    pub local_path: String,
    /// Only upload files that are new or changed since the volume was prepared.
    #[serde(default)]
    pub only_changed: bool,
}

/// Everything kubedal remembers about one published volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeRecord {
    pub volume_id: String,
    pub target_path: PathBuf,
    pub pod: PodRef,
    pub mode: VolumeMode,
    pub read_only: bool,
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload: Option<UploadPlan>,
}

/// Stores one JSON file per volume in a directory.
#[derive(Debug, Clone)]
pub struct StateStore {
    dir: PathBuf,
}

impl StateStore {
    /// Open (and create if needed) a state directory.
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Write a record. The file is replaced in one step, so a crash never leaves
    /// a half written record behind.
    pub fn save(&self, id: &str, record: &VolumeRecord) -> std::io::Result<()> {
        let data = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
        let tmp = self.dir.join(format!(".{id}.json.tmp"));
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&data)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, self.path(id))?;
        // Make the rename itself durable.
        std::fs::File::open(&self.dir)?.sync_all()
    }

    pub fn load(&self, id: &str) -> std::io::Result<Option<VolumeRecord>> {
        match std::fs::read(self.path(id)) {
            Ok(data) => serde_json::from_slice(&data)
                .map(Some)
                .map_err(std::io::Error::other),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Remove a record. Removing a missing record is not an error.
    pub fn remove(&self, id: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.path(id)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// All records. Files that cannot be read are logged and skipped.
    pub fn list(&self) -> std::io::Result<Vec<(String, VolumeRecord)>> {
        let mut records = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
                .filter(|n| !n.starts_with('.'))
            else {
                continue;
            };
            match self.load(id) {
                Ok(Some(record)) => records.push((id.to_string(), record)),
                Ok(None) => {}
                Err(err) => tracing::warn!("skipping unreadable state file {path:?}: {err}"),
            }
        }
        records.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(records)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn sample_record() -> VolumeRecord {
        VolumeRecord {
            volume_id: "vol-1".into(),
            target_path: PathBuf::from("/var/lib/kubelet/pods/p/volumes/x/mount"),
            pod: PodRef {
                name: "pod".into(),
                namespace: "ns".into(),
                uid: "uid-1".into(),
                service_account: "default".into(),
            },
            mode: VolumeMode::Cache,
            read_only: false,
            phase: Phase::Published,
            upload: Some(UploadPlan {
                data_node: DataNodeRef {
                    name: "store".into(),
                    namespace: "ns".into(),
                },
                prefix: "out/".into(),
                local_path: String::new(),
                only_changed: true,
            }),
        }
    }

    #[test]
    fn save_load_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path().join("state")).unwrap();
        let record = sample_record();
        assert_eq!(store.load("a").unwrap(), None);
        store.save("a", &record).unwrap();
        assert_eq!(store.load("a").unwrap(), Some(record.clone()));

        let mut changed = record.clone();
        changed.phase = Phase::Unpublishing;
        store.save("a", &changed).unwrap();
        assert_eq!(store.load("a").unwrap(), Some(changed));

        store.remove("a").unwrap();
        store.remove("a").unwrap();
        assert_eq!(store.load("a").unwrap(), None);
    }

    #[test]
    fn list_skips_broken_and_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(dir.path()).unwrap();
        store.save("b", &sample_record()).unwrap();
        store.save("a", &sample_record()).unwrap();
        std::fs::write(dir.path().join("broken.json"), "{not json").unwrap();
        std::fs::write(dir.path().join(".c.json.tmp"), "{}").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let ids: Vec<_> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn record_format_is_stable() {
        let json = serde_json::to_value(sample_record()).unwrap();
        assert_eq!(json["phase"], "published");
        assert_eq!(json["mode"], "cache");
        assert_eq!(json["upload"]["dataNode"]["name"], "store");
        assert_eq!(json["pod"]["serviceAccount"], "default");
    }
}
