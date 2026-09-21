// Copyright (c) 2026
//
// SPDX-License-Identifier: Apache-2.0

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use common::{
    types::{
        CheckpointSandboxRequest, RestoreSandboxInfo, RestoreSandboxRequest, RestoredSandboxTask,
        SandboxCheckpointTask, SandboxRestoreTask,
    },
    ContainerCheckpointRestore, SandboxCheckpointRestore,
};
use kata_types::mount::Mount;
use nix::mount::{mount, MsFlags};
use oci_spec::runtime::Spec;
use serde::{Deserialize, Serialize};

use crate::{container_manager::VirtContainerManager, sandbox::VirtSandbox};

#[derive(Clone)]
pub(crate) struct VirtCheckpointRestore {
    sandbox: Arc<VirtSandbox>,
    container_manager: Arc<VirtContainerManager>,
}

impl VirtCheckpointRestore {
    pub(crate) fn new(
        sandbox: Arc<VirtSandbox>,
        container_manager: Arc<VirtContainerManager>,
    ) -> Self {
        Self {
            sandbox,
            container_manager,
        }
    }

    async fn reconnect_agent(&self) -> Result<()> {
        let address = self
            .sandbox
            .hypervisor
            .get_agent_socket()
            .await
            .context("get agent socket after checkpoint")?;
        self.sandbox
            .get_agent()
            .start(&address)
            .await
            .context("reconnect agent after checkpoint")
    }
}

async fn copy_passthrough_files(source: &Path, destination: &Path) -> Result<()> {
    tokio::fs::create_dir_all(destination).await?;
    let mut entries = tokio::fs::read_dir(source)
        .await
        .with_context(|| format!("read virtio-fs passthrough {}", source.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let file_type = entry.file_type().await?;
        // Container rootfs directories are rebuilt from lowerdirs + rw-diff.
        // The direct files are sandbox/container bind files whose old names
        // are embedded in virtiofsd's migrated inode table.
        if file_type.is_file() || file_type.is_symlink() {
            let target = destination.join(entry.file_name());
            tokio::fs::copy(entry.path(), &target)
                .await
                .with_context(|| {
                    format!(
                        "copy virtio-fs passthrough file {} to {}",
                        entry.path().display(),
                        target.display()
                    )
                })?;
        }
    }
    Ok(())
}

impl VirtSandbox {
    pub(crate) async fn prepare_restored_rootfs(&self, checkpoint_dir: &str) -> Result<()> {
        let checkpoint_dir = Path::new(checkpoint_dir);
        let metadata: CheckpointMetadata = serde_json::from_slice(
            &tokio::fs::read(checkpoint_dir.join("metadata.json"))
                .await
                .context("read checkpoint metadata")?,
        )
        .context("parse checkpoint metadata")?;

        let passthrough =
            resource::share_fs::get_host_rw_shared_path(&self.sid).join("passthrough");
        copy_passthrough_files(&checkpoint_dir.join("share-passthrough"), &passthrough)
            .await
            .context("restore virtio-fs passthrough files")?;

        for container in metadata.containers {
            let container_dir = checkpoint_dir.join("containers").join(&container.old_id);
            let restore_dir = container_dir.join("restores").join(&self.sid);
            let upper = restore_dir.join("rw-diff");
            let work = restore_dir.join("rw-work");
            let merged = restore_dir.join("merged");
            copy_rw_diff(&container_dir.join("rw-diff"), &upper)
                .await
                .context("clone checkpoint overlay upperdir")?;
            tokio::fs::create_dir_all(&work).await?;
            tokio::fs::create_dir_all(&merged).await?;
            let options = format!(
                "lowerdir={},upperdir={},workdir={}",
                container.lower_dirs.join(":"),
                upper.display(),
                work.display()
            );
            mount(
                Some("overlay"),
                &merged,
                Some("overlay"),
                MsFlags::empty(),
                Some(options.as_str()),
            )
            .with_context(|| format!("mount restored overlay {}", merged.display()))?;

            let destination = resource::share_fs::get_host_rw_shared_path(&self.sid)
                .join("passthrough")
                .join(&container.old_id)
                .join("rootfs");
            tokio::fs::create_dir_all(&destination).await?;
            mount(
                Some(&merged),
                &destination,
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REC,
                None::<&str>,
            )
            .with_context(|| {
                format!(
                    "bind restored rootfs {} to {}",
                    merged.display(),
                    destination.display()
                )
            })?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CheckpointMetadata {
    pub version: u32,
    pub sandbox_id: String,
    pub agent_socket: String,
    pub containers: Vec<CheckpointContainerMetadata>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CheckpointContainerMetadata {
    pub old_id: String,
    pub bundle: String,
    pub rootfs_mounts: Vec<Mount>,
    pub lower_dirs: Vec<String>,
    pub spec: Spec,
}

fn overlay_paths(options: &[String]) -> (Vec<String>, Option<String>) {
    let lower_dirs = options
        .iter()
        .find_map(|option| option.strip_prefix("lowerdir="))
        .map(|value| value.split(':').map(str::to_string).collect())
        .unwrap_or_default();
    let upper_dir = options
        .iter()
        .find_map(|option| option.strip_prefix("upperdir="))
        .map(str::to_string);
    (lower_dirs, upper_dir)
}

fn rootfs_overlay_paths(bundle: &str, mounts: &[Mount]) -> Result<(Vec<String>, String)> {
    for mount in mounts {
        let (lowers, upper) = overlay_paths(&mount.options);
        if let Some(upper) = upper {
            return Ok((lowers, upper));
        }
    }

    let mut candidates: Vec<PathBuf> = mounts
        .iter()
        .map(|mount| PathBuf::from(&mount.source))
        .collect();
    candidates.push(Path::new(bundle).join("rootfs"));
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .context("read shim mountinfo for overlay snapshot")?;
    for line in mountinfo.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let left_fields: Vec<&str> = left.split_whitespace().collect();
        let right_fields: Vec<&str> = right.split_whitespace().collect();
        if left_fields.len() < 6 || right_fields.len() < 3 || right_fields[0] != "overlay" {
            continue;
        }
        if !candidates
            .iter()
            .any(|candidate| candidate == Path::new(left_fields[4]))
        {
            continue;
        }
        let options: Vec<String> = right_fields[2].split(',').map(str::to_string).collect();
        let (lowers, upper) = overlay_paths(&options);
        if let Some(upper) = upper {
            return Ok((lowers, upper));
        }
    }
    bail!("cannot locate overlay upperdir for bundle {bundle}")
}

async fn copy_rw_diff(source: &Path, destination: &Path) -> Result<()> {
    tokio::fs::create_dir_all(destination).await?;
    let source_dot = source.join(".");
    let status = tokio::process::Command::new("cp")
        .arg("-a")
        .arg("--reflink=auto")
        .arg(source_dot)
        .arg(destination)
        .status()
        .await
        .context("run cp for overlay upperdir")?;
    if !status.success() {
        bail!("copy overlay upperdir failed with {status}");
    }
    Ok(())
}

#[async_trait]
impl SandboxCheckpointRestore for VirtCheckpointRestore {
    async fn validate_checkpoint_sandbox(&self, req: &CheckpointSandboxRequest) -> Result<()> {
        if req.sandbox_id != self.sandbox.sid {
            bail!(
                "checkpoint sandbox id {} does not match {}",
                req.sandbox_id,
                self.sandbox.sid
            );
        }
        if req.output_path.is_empty() {
            bail!("checkpoint output path is empty");
        }
        Ok(())
    }

    async fn checkpoint_sandbox(&self, req: &CheckpointSandboxRequest) -> Result<()> {
        self.sandbox
            .hypervisor
            .save_vm_to(&req.output_path)
            .await
            .context("save VM checkpoint")?;

        let passthrough =
            resource::share_fs::get_host_rw_shared_path(&self.sandbox.sid).join("passthrough");
        copy_passthrough_files(
            &passthrough,
            &Path::new(&req.output_path).join("share-passthrough"),
        )
        .await
        .context("checkpoint virtio-fs passthrough files")?;

        let containers = self.container_manager.containers.read().await;
        let mut metadata = CheckpointMetadata {
            version: 1,
            sandbox_id: self.sandbox.sid.clone(),
            agent_socket: self
                .sandbox
                .hypervisor
                .get_agent_socket()
                .await
                .context("get checkpoint agent socket")?,
            containers: Vec::with_capacity(containers.len()),
        };
        for container in containers.values() {
            let config = container.config().await;
            let (lower_dirs, upper_dir) =
                rootfs_overlay_paths(&config.bundle, &config.rootfs_mounts)?;
            let container_dir = Path::new(&req.output_path)
                .join("containers")
                .join(container.container_id.to_string());
            copy_rw_diff(Path::new(&upper_dir), &container_dir.join("rw-diff")).await?;
            tokio::fs::create_dir_all(container_dir.join("rw-work")).await?;
            tokio::fs::create_dir_all(container_dir.join("merged")).await?;
            metadata.containers.push(CheckpointContainerMetadata {
                old_id: container.container_id.to_string(),
                bundle: config.bundle,
                rootfs_mounts: config.rootfs_mounts,
                lower_dirs,
                spec: container.spec().await,
            });
        }
        let metadata_path = Path::new(&req.output_path).join("metadata.json");
        tokio::fs::write(
            &metadata_path,
            serde_json::to_vec_pretty(&metadata).context("serialize checkpoint metadata")?,
        )
        .await
        .with_context(|| format!("write {}", metadata_path.display()))?;
        Ok(())
    }

    async fn restore_sandbox(&self, _req: &RestoreSandboxRequest) -> Result<RestoreSandboxInfo> {
        bail!("restore is not implemented yet")
    }

    async fn prepare_restore_sandbox(
        &self,
        _req: &RestoreSandboxRequest,
    ) -> Result<RestoreSandboxInfo> {
        bail!("restore prepare is not implemented yet")
    }

    async fn complete_restore_sandbox(
        &self,
        _req: &RestoreSandboxRequest,
    ) -> Result<RestoreSandboxInfo> {
        bail!("restore complete is not implemented yet")
    }

    async fn cleanup_sandbox_checkpoint(&self, _req: &CheckpointSandboxRequest) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ContainerCheckpointRestore for VirtCheckpointRestore {
    async fn prepare_checkpoint_tasks(&self, _tasks: &mut [SandboxCheckpointTask]) -> Result<()> {
        Ok(())
    }

    async fn pause_checkpoint_tasks(&self, _tasks: &[SandboxCheckpointTask]) -> Result<()> {
        self.sandbox
            .get_agent()
            .disconnect()
            .await
            .context("disconnect agent before checkpoint")?;
        // Let the guest observe the closed vsock connection and return its
        // ttrpc server to accept() before freezing device and process state.
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Err(err) = self.sandbox.hypervisor.pause_vm().await.context("pause VM") {
            return match self.reconnect_agent().await {
                Ok(()) => Err(err),
                Err(reconnect_err) => Err(err).with_context(|| {
                    format!("failed to reconnect agent after pause error: {reconnect_err:#}")
                }),
            };
        }
        Ok(())
    }

    async fn resume_checkpoint_tasks(&self, _tasks: &[SandboxCheckpointTask]) -> Result<()> {
        self.sandbox
            .hypervisor
            .resume_vm()
            .await
            .context("resume VM")?;
        self.reconnect_agent().await
    }

    async fn restore_tasks(
        &self,
        _tasks: &[SandboxRestoreTask],
        _restored: &[RestoredSandboxTask],
    ) -> Result<()> {
        bail!("task adoption is not implemented yet")
    }
}
