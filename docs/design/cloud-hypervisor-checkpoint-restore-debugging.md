# Cloud Hypervisor checkpoint and restore debugging

This document records the end-to-end debugging of Pod checkpoint and restore
with Cloud Hypervisor v53.0 and runtime-rs. It complements the
[QEMU Pod checkpoint and restore design](qemu-pod-checkpoint-restore.md) and
focuses on failures that only became visible while restoring a running
virtio-fs workload.

The verified flow is:

1. Start a Pod with the `kata-clh` runtime.
2. Start a container that continuously increments `/counter`.
3. Checkpoint the running sandbox with `shim-ctl`.
4. Remove the source sandbox.
5. Create a new restore-annotated sandbox.
6. Rebind the restored guest containers to their new CRI IDs.
7. Read `/counter` twice and verify that it continues increasing.

## Test environment

The successful run used:

- Cloud Hypervisor v53.0
- `virtiofsd` v1.14.0
- runtime-rs `containerd-shim-kata-v2`
- containerd CRI with the `podsandbox` sandboxer
- a 256 MiB guest for faster test iteration
- a same-node checkpoint directory on a local filesystem

The test inputs are in `.e2e-clh/`:

- `kata-clh.toml` registers the `kata-clh` runtime handler.
- `source-pod.json` defines the source sandbox.
- `restore-pod.json` enables restore and selects the checkpoint directory.
- `container.json` runs the monotonic counter workload.

!!! warning "The checkpoint is not self-contained"
    `metadata.json` records the container image overlay lower directories.
    Restore therefore requires the same node and retained containerd image
    snapshots. Removing the image or allowing snapshot garbage collection
    before restore can invalidate those paths.

## Reproduction procedure

Register the test runtime by importing `.e2e-clh/kata-clh.toml` from the
containerd configuration, then restart containerd.

Build and install the runtime and checkpoint client:

```bash title="$ build runtime-rs and shim-ctl"
cargo build --release -p runtime-rs --bin containerd-shim-kata-v2
cargo build --release -p shim-ctl --bin shim-ctl
sudo install -D -m 0755 target/release/containerd-shim-kata-v2 \
    /opt/kata/bin/containerd-shim-kata-v2
sudo install -D -m 0755 target/release/shim-ctl \
    /opt/kata/bin/shim-ctl
```

Create the source Pod and workload:

```bash title="$ start the source workload"
pod_id=$(sudo crictl runp --runtime kata-clh .e2e-clh/source-pod.json)
container_id=$(sudo crictl create "$pod_id" \
    .e2e-clh/container.json .e2e-clh/source-pod.json)
sudo crictl start "$container_id"
sudo crictl exec "$container_id" cat /counter
```

Checkpoint the sandbox:

```bash title="$ checkpoint the running sandbox"
sudo /opt/kata/bin/shim-ctl checkpoint \
    "$pod_id" /var/lib/kata/checkpoints/clh-e2e
```

After releasing the source sandbox, create the restored Pod and adopt its
workload:

```bash title="$ restore and adopt the workload"
restored_pod_id=$(sudo crictl -t 120s runp \
    --runtime kata-clh .e2e-clh/restore-pod.json)
restored_container_id=$(sudo crictl create "$restored_pod_id" \
    .e2e-clh/container.json .e2e-clh/restore-pod.json)
sudo crictl start "$restored_container_id"

sudo crictl exec "$restored_container_id" cat /counter
sleep 2
sudo crictl exec "$restored_container_id" cat /counter
```

The successful test returned `26` and then `28`. The exact values are not
important; the second value must be larger and the counter must not restart
from zero.

## Problems found and fixes

### Cloud Hypervisor restore API did not carry all runtime state

Restore needs more than the snapshot directory. The new VMM must receive the
TAP file descriptors associated with the restored network devices, and the
snapshot configuration contains source-runtime socket paths that are no longer
valid.

The restore implementation now:

- sends network FD metadata and the corresponding file descriptors;
- patches the vsock and virtio-fs socket paths for the new sandbox;
- validates that the restored network configuration matches the available
  devices and FDs;
- explicitly requests on-demand memory restore; and
- symlinks `memory-ranges` instead of copying the memory snapshot.

On-demand restore uses `userfaultfd` to populate memory as pages are touched.
This removes an additional full copy of `memory-ranges` from the restore path.

### The kata-agent connection became unusable after checkpoint

Pausing and snapshotting a VM while the host ttrpc connection was active left
the resumed source with a stale connection. Subsequent CRI operations timed out
while waiting for the agent.

The checkpoint sequence now disconnects the agent before pausing the VM, waits
briefly for the guest server to observe the disconnect, and reconnects after
the VM resumes. If pausing fails, it also attempts to reconnect before
returning the original error.

!!! warning "Source teardown during development"
    Some failed iterations still required killing the source shim, VMM, and
    virtiofsd before restarting containerd. The successful restore proves the
    checkpoint and destination paths, but graceful source teardown needs
    additional stress testing.

### Restored virtio-fs paths referred to the source sandbox

The migrated virtio-fs inode table contains paths looked up by the source
guest, including the old pause-container and workload rootfs paths. Starting a
new empty share for the destination caused `ENOENT` errors such as a missing
`rootfs/proc`.

Checkpoint metadata now includes both pause and workload containers. Before
Cloud Hypervisor restores device state, runtime-rs reconstructs each rootfs
overlay and bind-mounts it at the old container ID under the destination
sandbox's virtio-fs share.

The writable overlay diff is copied while the source VM is paused. This
captures guest-created mount-point directories such as `proc` and `sys`, which
may not exist in the image lower layer.

### Failed restore attempts modified the checkpoint

The initial implementation mounted the saved `rw-diff` directly as the
destination overlay upper directory. Container cleanup then wrote whiteouts
into the checkpoint. A later retry failed with missing paths even though the
first attempt started from a valid checkpoint.

Each restore now creates:

```text
containers/<old-container-id>/restores/<new-sandbox-id>/
├── rw-diff/
├── rw-work/
└── merged/
```

`rw-diff` is cloned from the saved checkpoint with `cp -a --reflink=auto`.
All destination writes and cleanup are isolated under the new sandbox ID, so
the original checkpoint remains reusable.

### `virtiofsd` and Cloud Hypervisor deadlocked during device restore

With `virtiofsd` v1.13.1, Cloud Hypervisor restore stopped immediately after:

```text
virtiofsd: Client connected, servicing requests
```

System call tracing showed the Cloud Hypervisor vhost-user thread and
`virtiofsd` both blocked in `recvmsg()`. This matches the known vhost-user
`REPLY_ACK` negotiation problem described in
cloud-hypervisor/cloud-hypervisor#8313 and rust-vmm/vhost#290.

The Kata dependency is already `virtiofsd` v1.14.0. Updating the installed host
binary from v1.13.1 to v1.14.0 resolved the deadlock; the same restore completed
in less than one second.

!!! danger "Version requirement"
    Do not test Cloud Hypervisor vhost-user-fs restore with an older installed
    `virtiofsd` merely because `versions.yaml` names a newer release. Verify the
    binary that the runtime actually launches:

    ```bash
    sudo /opt/kata/libexec/virtiofsd --version
    ```

### Copy memory mode was not the root cause

Changing the restore memory mode alone did not fix the vhost-user failure. It
reduced restore overhead and is appropriate for this implementation, but the
remaining hang was a protocol compatibility problem in the installed
`virtiofsd`.

This distinction matters when diagnosing future failures: memory population,
virtio-fs path reconstruction, and vhost-user device-state transfer are
separate restore stages.

## Verification results

The final run verified all of the following:

- Cloud Hypervisor accepted the snapshot and restored the VM.
- The destination used a new sandbox, network namespace, TAP device, vsock
  socket, and virtio-fs socket.
- Pause and workload root filesystems were reconstructed before device restore.
- `virtiofsd` restored its migrated inode state.
- kata-agent reconnected and rebound old guest container IDs to new CRI IDs.
- `CreateContainer` and `StartContainer` adopted the restored processes instead
  of starting a new counter.
- `/counter` continued from `26` to `28`.
- The saved pause rootfs still contained `proc` after restore, confirming that
  the checkpoint overlay was not modified.

Focused verification commands also passed:

```bash title="$ focused build and unit tests"
cargo check -p virt_container
cargo test -p ch-config \
    test_restore_config_serializes_network_fd_metadata
cargo test -p hypervisor test_patch_checkpoint_runtime_paths
cargo test -p hypervisor \
    test_prepare_checkpoint_restore_files_reuses_memory_snapshot
git diff --check
```

## Remaining limitations

- Restore is same-node only.
- Image overlay lower directories must remain available.
- The flow has only been verified with one workload container, virtio-fs,
  one network interface, and no VFIO devices or extra volumes.
- The test uses Kata-private restore annotations rather than the Kubernetes
  Pod checkpoint and restore API.
- Failed restore directories under `containers/*/restores/` need a lifecycle
  and garbage-collection policy.
- Graceful source shutdown and repeated checkpoint/restore cycles need broader
  stress testing.
