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

        let block_containers = metadata
            .containers
            .iter()
            .filter(|container| !container.block_disks.is_empty())
            .count();
        if block_containers == metadata.containers.len() {
            return Ok(());
        }
        if block_containers != 0 {
            bail!("mixed virtio-fs and block rootfs checkpoints are not supported");
        }

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
    #[serde(default)]
    pub block_disks: Vec<CheckpointBlockDisk>,
    pub spec: Spec,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CheckpointBlockRole {
    ErofsLower,
    Ext4Upper,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CheckpointBlockDisk {
    pub id: String,
    pub role: CheckpointBlockRole,
    pub layer_index: Option<usize>,
    pub path: String,
    pub readonly: bool,
    pub size: u64,
    pub num_queues: usize,
    pub queue_size: u64,
}

fn snapshot_disk<'a>(
    snapshot_config: &'a serde_json::Value,
    source: &Path,
) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
    snapshot_config
        .get("disks")
        .and_then(serde_json::Value::as_array)
        .context("Cloud Hypervisor checkpoint config is missing disks")?
        .iter()
        .filter_map(serde_json::Value::as_object)
        .find(|disk| disk.get("path").and_then(serde_json::Value::as_str) == source.to_str())
        .with_context(|| {
            format!(
                "Cloud Hypervisor checkpoint config has no disk backed by {}",
                source.display()
            )
        })
}

async fn copy_block_image(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if tokio::fs::symlink_metadata(destination).await.is_ok() {
        bail!(
            "checkpoint block image destination already exists: {}",
            destination.display()
        );
    }
    let status = tokio::process::Command::new("cp")
        .arg("-a")
        .arg("--reflink=auto")
        .arg("--sparse=always")
        .arg(source)
        .arg(destination)
        .status()
        .await
        .with_context(|| format!("copy block image {}", source.display()))?;
    if !status.success() {
        bail!(
            "copy block image {} to {} failed with {status}",
            source.display(),
            destination.display()
        );
    }
    let copied = tokio::fs::symlink_metadata(destination)
        .await
        .with_context(|| format!("access copied block image {}", destination.display()))?;
    if copied.file_type().is_symlink() || !copied.is_file() {
        bail!(
            "copied checkpoint block image is not a regular file: {}",
            destination.display()
        );
    }
    Ok(())
}

async fn checkpoint_block_rootfs(
    checkpoint_dir: &Path,
    container_id: &str,
    mounts: &[Mount],
    snapshot_config: &serde_json::Value,
) -> Result<Vec<CheckpointBlockDisk>> {
    let mut disks = Vec::new();
    let mut lower_index = 0;
    for rootfs_mount in mounts {
        let (role, layer_index, readonly, extension) =
            if rootfs_mount.fs_type.eq_ignore_ascii_case("erofs") {
                let index = lower_index;
                lower_index += 1;
                (CheckpointBlockRole::ErofsLower, Some(index), true, "erofs")
            } else if rootfs_mount.fs_type.eq_ignore_ascii_case("ext4") {
                (CheckpointBlockRole::Ext4Upper, None, false, "ext4")
            } else {
                continue;
            };

        let source = Path::new(&rootfs_mount.source);
        let disk = snapshot_disk(snapshot_config, source)?;
        let id = disk
            .get("id")
            .and_then(serde_json::Value::as_str)
            .context("Cloud Hypervisor checkpoint disk is missing an id")?
            .to_string();
        let relative_path = match layer_index {
            Some(index) => PathBuf::from("containers")
                .join(container_id)
                .join(format!("lower-{index}.{extension}")),
            None => PathBuf::from("containers")
                .join(container_id)
                .join(format!("upper.{extension}")),
        };
        let destination = checkpoint_dir.join(&relative_path);
        copy_block_image(source, &destination).await?;
        let size = tokio::fs::metadata(&destination).await?.len();

        disks.push(CheckpointBlockDisk {
            id,
            role,
            layer_index,
            path: relative_path.to_string_lossy().into_owned(),
            readonly,
            size,
            num_queues: disk
                .get("num_queues")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default() as usize,
            queue_size: disk
                .get("queue_size")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default(),
        });
    }
    Ok(disks)
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
        if tokio::fs::symlink_metadata(&req.output_path).await.is_ok() {
            bail!("checkpoint output path already exists");
        }
        let containers = self.container_manager.containers.read().await;
        let raw_erofs_disks = !self.sandbox.hypervisor.supports_structured_vmdk();
        let mut block_rootfs_count = 0;
        for container in containers.values() {
            if raw_erofs_disks
                && container
                    .config()
                    .await
                    .rootfs_mounts
                    .iter()
                    .any(|mount| mount.fs_type.eq_ignore_ascii_case("erofs"))
            {
                block_rootfs_count += 1;
            }
        }
        if block_rootfs_count != 0 && block_rootfs_count != containers.len() {
            bail!("mixed virtio-fs and block rootfs checkpoints are not supported");
        }
        Ok(())
    }

    async fn checkpoint_sandbox(&self, req: &CheckpointSandboxRequest) -> Result<()> {
        self.sandbox
            .hypervisor
            .save_vm_to(&req.output_path)
            .await
            .context("save VM checkpoint")?;

        let containers = self.container_manager.containers.read().await;
        let checkpoint_dir = Path::new(&req.output_path);
        let raw_erofs_disks = !self.sandbox.hypervisor.supports_structured_vmdk();
        let mut has_block_rootfs = false;
        for container in containers.values() {
            if raw_erofs_disks
                && container
                    .config()
                    .await
                    .rootfs_mounts
                    .iter()
                    .any(|mount| mount.fs_type.eq_ignore_ascii_case("erofs"))
            {
                has_block_rootfs = true;
                break;
            }
        }
        let snapshot_config: serde_json::Value = if has_block_rootfs {
            serde_json::from_slice(
                &tokio::fs::read(checkpoint_dir.join("config.json"))
                    .await
                    .context("read Cloud Hypervisor checkpoint config")?,
            )
            .context("parse Cloud Hypervisor checkpoint config")?
        } else {
            serde_json::Value::Null
        };
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
        let mut has_virtiofs_rootfs = false;
        let mut archived_block_disks: std::collections::HashMap<String, CheckpointBlockDisk> =
            std::collections::HashMap::new();
        for container in containers.values() {
            let config = container.config().await;
            let is_block_rootfs = raw_erofs_disks
                && config
                    .rootfs_mounts
                    .iter()
                    .any(|mount| mount.fs_type.eq_ignore_ascii_case("erofs"));
            let (lower_dirs, block_disks) = if is_block_rootfs {
                let mut block_disks = checkpoint_block_rootfs(
                    checkpoint_dir,
                    &container.container_id.to_string(),
                    &config.rootfs_mounts,
                    &snapshot_config,
                )
                .await?;
                for disk in &mut block_disks {
                    if let Some(archived) = archived_block_disks.get(&disk.id) {
                        if archived.role != disk.role
                            || archived.readonly != disk.readonly
                            || archived.size != disk.size
                            || archived.num_queues != disk.num_queues
                            || archived.queue_size != disk.queue_size
                        {
                            bail!("checkpoint disk id {} has conflicting metadata", disk.id);
                        }
                        let duplicate = checkpoint_dir.join(&disk.path);
                        if disk.path != archived.path {
                            tokio::fs::remove_file(&duplicate).await.with_context(|| {
                                format!("remove duplicate block image {}", duplicate.display())
                            })?;
                            disk.path.clone_from(&archived.path);
                        }
                    } else {
                        archived_block_disks.insert(disk.id.clone(), disk.clone());
                    }
                }
                (Vec::new(), block_disks)
            } else {
                has_virtiofs_rootfs = true;
                let (lower_dirs, upper_dir) =
                    rootfs_overlay_paths(&config.bundle, &config.rootfs_mounts)?;
                let container_dir = checkpoint_dir
                    .join("containers")
                    .join(container.container_id.to_string());
                copy_rw_diff(Path::new(&upper_dir), &container_dir.join("rw-diff")).await?;
                tokio::fs::create_dir_all(container_dir.join("rw-work")).await?;
                tokio::fs::create_dir_all(container_dir.join("merged")).await?;
                (lower_dirs, Vec::new())
            };
            metadata.containers.push(CheckpointContainerMetadata {
                old_id: container.container_id.to_string(),
                bundle: config.bundle,
                rootfs_mounts: config.rootfs_mounts,
                lower_dirs,
                block_disks,
                spec: container.spec().await,
            });
        }
        if has_virtiofs_rootfs {
            let passthrough =
                resource::share_fs::get_host_rw_shared_path(&self.sandbox.sid).join("passthrough");
            copy_passthrough_files(&passthrough, &checkpoint_dir.join("share-passthrough"))
                .await
                .context("checkpoint virtio-fs passthrough files")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn checkpoint_block_rootfs_archives_lower_and_upper() {
        let dir = tempfile::tempdir().unwrap();
        let source_lower = dir.path().join("source.erofs");
        let source_upper = dir.path().join("source.ext4");
        tokio::fs::write(&source_lower, b"lower").await.unwrap();
        tokio::fs::write(&source_upper, b"upper").await.unwrap();
        let checkpoint_dir = dir.path().join("checkpoint");
        let mounts = vec![
            Mount {
                source: source_upper.display().to_string(),
                fs_type: "ext4".to_string(),
                ..Default::default()
            },
            Mount {
                source: source_lower.display().to_string(),
                fs_type: "erofs".to_string(),
                ..Default::default()
            },
        ];
        let snapshot_config = serde_json::json!({
            "disks": [
                {
                    "id": "upper0",
                    "path": source_upper,
                    "readonly": false,
                    "num_queues": 1,
                    "queue_size": 128
                },
                {
                    "id": "lower0",
                    "path": source_lower,
                    "readonly": true,
                    "num_queues": 1,
                    "queue_size": 128
                }
            ]
        });

        let disks =
            checkpoint_block_rootfs(&checkpoint_dir, "container", &mounts, &snapshot_config)
                .await
                .unwrap();

        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].role, CheckpointBlockRole::Ext4Upper);
        assert_eq!(disks[1].role, CheckpointBlockRole::ErofsLower);
        assert_eq!(
            tokio::fs::read(checkpoint_dir.join(&disks[0].path))
                .await
                .unwrap(),
            b"upper"
        );
        assert_eq!(
            tokio::fs::read(checkpoint_dir.join(&disks[1].path))
                .await
                .unwrap(),
            b"lower"
        );
    }
}
