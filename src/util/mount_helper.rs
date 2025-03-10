use std::{fs, io::Write, os::unix::fs::DirBuilderExt, path::Path};

use fuse3::{MountOptions, path::Session, raw::MountHandle};
use fuse3_opendal::Filesystem;
use futures::TryStreamExt;
use sys_mount::{Mount as SysMount, MountFlags, UnmountFlags, unmount};
use tonic::Status;

use crate::resource::crd::{AccessMode, MountMode};

pub struct Mount {
    pub volume_id: String,
    pub target_path: String,
    pub fuse_mount: Option<MountHandle>,
    pub operator: opendal::Operator,
    pub mount_mode: MountMode,
    pub access_mode: AccessMode,
}

impl Mount {
    pub fn new(
        volume_id: String,
        target_path: String,
        operator: opendal::Operator,
        mount_mode: MountMode,
        access_mode: AccessMode,
    ) -> Self {
        Mount {
            volume_id,
            target_path,
            fuse_mount: None,
            operator,
            mount_mode,
            access_mode,
        }
    }

    pub async fn mount(&mut self) -> Result<(), Status> {
        // Check if openDAL operator is working
        self.operator
            .check()
            .await
            .map_err(|e| Status::internal(format!("Operator check failed: {}", e)))?;

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

        match self.mount_mode {
            MountMode::Cached => self.mount_cached().await,
            MountMode::Fuse => self.mount_fuse().await,
        }
    }

    pub async fn unmount(&mut self) -> Result<(), Status> {
        match self.mount_mode {
            MountMode::Cached => self.unmount_cached().await,
            MountMode::Fuse => self.unmount_fuse().await,
        }
    }

    async fn mount_fuse(&mut self) -> Result<(), Status> {
        let fs = Filesystem::new(self.operator.clone(), 1000, 1000);

        let mut mount_options = MountOptions::default();

        if self.access_mode == AccessMode::ReadOnly {
            mount_options.read_only(true);
        }

        let mount_handle = Session::new(mount_options)
            .mount_with_unprivileged(fs, self.target_path.clone())
            .await?;

        self.fuse_mount = Some(mount_handle);

        Ok(())
    }

    async fn unmount_fuse(&mut self) -> Result<(), Status> {
        if let Some(mount_handle) = self.fuse_mount.take() {
            mount_handle.unmount().await?;
        }
        Ok(())
    }

    async fn mount_cached(&mut self) -> Result<(), Status> {
        // Create local mount dir if not exists
        let cache_path = Path::new("/mnt").join(&self.volume_id);
        if !cache_path.exists() {
            let mut builder = fs::DirBuilder::new();
            builder
                .mode(0o755)
                .recursive(true)
                .create(&cache_path)
                .map_err(|e| {
                    Status::internal(format!("Failed to create cache directory: {}", e))
                })?;
        }

        let data_source_children = self
            .operator
            .list("")
            .await
            .map_err(|e| Status::internal(format!("Data source listing failed: {}", e)))?;

        // Cache data source in target directory
        //TODO: More sophisticated directory structure to cache for data-source/version
        for entry in data_source_children {
            let entry_path = Path::new(&cache_path).join(entry.path());
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

        let mut mount_builder = SysMount::builder();
        if self.access_mode == AccessMode::ReadOnly {
            mount_builder = mount_builder.flags(MountFlags::RDONLY)
        }
        // Mount cache directory to target directory
        let target_path = self.target_path.clone();
        tokio::task::spawn_blocking(move || {
            mount_builder
                .mount(cache_path, target_path)
                .map_err(|e| Status::internal(format!("Failed to mount cache: {}", e)))
        })
        .await
        .map_err(|_| Status::internal("Unable to tokio::join mount operation"))??;

        Ok(())
    }

    async fn unmount_cached(&self) -> Result<(), Status> {
        let target_path = self.target_path.clone();
        tokio::task::spawn_blocking(move || {
            unmount(target_path, UnmountFlags::empty())
                .map_err(|e| Status::internal(format!("Failed to unmount cache: {}", e)))
        })
        .await
        .map_err(|_| Status::internal("Unable to tokio::join unmount operation"))?
    }
}
