use std::{fs, io::Write, os::unix::fs::DirBuilderExt, path::Path};

use fuse3::raw::MountHandle;
use futures::TryStreamExt;
use tonic::Status;

use crate::resource::crd::{AccessMode, MountMode};

pub struct Mount {
    pub target_path: String,
    pub fuse_mount: Option<MountHandle>,
    pub operator: opendal::Operator,
    pub mount_mode: MountMode,
    pub access_mode: AccessMode,
}

impl Mount {
    pub fn new(
        target_path: String,
        operator: opendal::Operator,
        mount_mode: MountMode,
        access_mode: AccessMode,
    ) -> Self {
        Mount {
            target_path,
            fuse_mount: None,
            operator,
            mount_mode,
            access_mode,
        }
    }

    pub async fn mount(&mut self) -> Result<(), Status> {
        match self.mount_mode {
            MountMode::Cached => self.mount_cached().await,
            MountMode::Fuse => self.mount_fuse().await,
        }
    }

    async fn mount_fuse(&mut self) -> Result<(), Status> {
        Ok(())
    }

    async fn mount_cached(&mut self) -> Result<(), Status> {
        //TODO: Match Resource mount type. Currently the data source is just mirrored into the volume.
        //let data_source_children = operator.list_with("").recursive(true).await.map_err(|e| {
        let data_source_children = self
            .operator
            .list("")
            .await
            .map_err(|e| Status::internal(format!("Data source listing failed: {}", e)))?;

        // Create the target directory if it doesn't exist
        let target_path_obj = Path::new(&self.target_path);
        if !target_path_obj.exists() {
            let mut builder = fs::DirBuilder::new();
            builder
                .mode(0o755)
                .recursive(true)
                .create(target_path_obj)
                .map_err(|e| {
                    Status::internal(format!("Failed to create target directory: {}", e))
                })?;
        }

        // Cache data source in target directory
        //TODO: More sophisticated directory structure to cache for data-source/version
        for entry in data_source_children {
            let entry_path = Path::new(&self.target_path).join(entry.path());
            match entry.metadata().mode() {
                opendal::EntryMode::FILE => {
                    // Create file
                    let mut file = std::fs::File::create(entry_path)?;
                    // Create stream
                    let mut r = self
                        .operator
                        .reader(entry.path())
                        .await
                        .map_err(|e| {
                            Status::internal(format!("Failed to open file reader: {}", e))
                        })?
                        .into_bytes_stream(..)
                        .await
                        .map_err(|e| {
                            Status::internal(format!("Failed to convert reader into stream: {}", e))
                        })?;
                    // Write stream into file
                    while let Some(bytes) = r.try_next().await? {
                        file.write_all(&bytes)?;
                    }
                }
                opendal::EntryMode::DIR => {
                    if !entry_path.exists() {
                        let mut builder = fs::DirBuilder::new();
                        builder
                            .mode(0o755)
                            .recursive(true)
                            .create(entry_path)
                            .map_err(|e| {
                                Status::internal(format!(
                                    "Failed to create target directory: {}",
                                    e
                                ))
                            })?;
                    }
                }
                opendal::EntryMode::Unknown => {
                    return Err(Status::unknown("Data source entry type is unknown"));
                }
            }
        }
        Ok(())
    }
}
