# QEMU Pod checkpoint and restore debugging

This document archives the failures found while implementing and validating
QEMU-level Pod checkpoint and restore in runtime-rs. It complements the
[QEMU Pod checkpoint and restore design](qemu-pod-checkpoint-restore.md) by
recording symptoms, root causes, fixes, and diagnostic lessons from the
end-to-end bring-up.

The verified path checkpoints a running pause container and workload, removes
the source sandbox, creates a new restore-annotated sandbox, and adopts the
original guest processes under new CRI IDs.

## Debugging stages

Treat restore as a sequence of independent stages:

```mermaid
flowchart LR
    Annotation[Restore annotations] --> HostState[Rebuild host state]
    HostState --> QemuStart[Start incoming QEMU]
    QemuStart --> Migration[Load device state]
    Migration --> Agent[Reconnect kata-agent]
    Agent --> Rebind[Rebind IDs]
    Rebind --> Adopt[Adopt tasks]
```

A failure late in this sequence does not prove that an earlier stage is
correct. In particular, a QEMU migration success does not prove that
virtio-fs paths, the agent connection, or container adoption are valid.

## Problems found and fixes

### Restore annotations did not reach the Kata shim

The restore Pod started as a normal new VM. The QEMU command line had no
`-incoming defer`, and the restore path in `VirtSandbox::start` was never
selected.

The Pod annotations were present in the CRI request but containerd did not pass
them into Kata's `SandboxConfig`. The Kata runtime entry must allow the private
annotation namespace:

```toml title="/etc/containerd/config.toml"
pod_annotations = ["io.katacontainers.*"]
```

After changing the configuration, restart containerd and inspect the shim
configuration or debug log before investigating QEMU. If the restore annotation
is absent there, migration debugging is premature.

### A restored vsock CID could not be allocated

The restored kata-agent continues listening on the source VM's vsock CID. The
normal device initialization path selected a new random CID, so the host could
not reconnect to the restored agent even if QEMU migration completed.

Checkpoint now writes `vm.json` with the source vsock CID and guest NIC MAC
addresses. Restore loads that identity before command-line generation and
explicitly binds `/dev/vhost-vsock` to the saved CID. The CID validation also
excludes `u32::MAX`, which is not a valid assignable guest CID.

!!! warning "Source and destination must not own the CID concurrently"
    Stop and remove the source sandbox before starting the destination. An
    active source VM still owns its vsock CID and makes destination binding
    fail.

### Incoming migration crashed near `apic_sipi`

QEMU crashed while loading migration state. Trace output ended around
`qemu_loadvm_state_section` and the stack included `apic_sipi`, which initially
made APIC restore look like the root cause.

Core-dump analysis showed that QEMU was executing `memcpy()` into a read-only
mapping. The guest image was exposed as a read-only NVDIMM backed by
`memory-backend-file`. QEMU treated that mapping as a migratable RAMBlock and
incoming migration attempted to restore bytes into it.

The read-only NVDIMM backend must be both shared and read-only:

```text
memory-backend-file,...,share=on,readonly=on
```

With the `x-ignore-shared` migration capability enabled, QEMU excludes that
immutable RAMBlock from the stream. The private file-backed guest RAM is also
shared and excluded because its bytes are preserved separately in the
checkpoint `memory` file.

Restore additionally starts QEMU with:

```text
-S -incoming defer
```

This keeps vCPUs paused while incoming state and host backends are established.

!!! note "The last trace event can be a false lead"
    A migration trace identifies where execution stopped, not necessarily the
    invalid object. Confirm the destination address and mapping permissions
    from the core dump before changing CPU or interrupt-controller code.

### `virtiofsd` could not find source passthrough files

After the NVDIMM crash was fixed, migration failed with errors similar to:

```text
No such file or directory: /passthrough/sandbox-*-resolv.conf
```

The migrated virtio-fs inode state still referenced source-side host files for
`/etc/hostname`, `/etc/hosts`, and `/etc/resolv.conf`. Source sandbox cleanup
removed those files, and a newly created destination share used different
names.

Checkpoint now copies the direct files from the source `passthrough` directory
into:

```text
checkpoint_dir/share-passthrough/
```

Restore recreates them under the destination sandbox's share, preserving their
old names, before starting QEMU and loading device state. Container root filesystems
are handled separately by rebuilding their overlays from saved metadata and
`rw-diff`.

### Restore initially assumed exactly one container

A Kubernetes Pod sandbox contains at least the pause container and may contain
one or more workload containers. Rejecting metadata unless it contained exactly
one container made a realistic CRI restore impossible.

Each destination `CreateContainer` now selects an unadopted checkpoint entry by:

1. matching the CRI container type;
2. matching the CRI container name when available; and
3. accepting a sole remaining candidate only when the match is unambiguous.

After `RebindSandbox` succeeds, the old ID is marked adopted. `CreateContainer`
and `StartContainer` then register host-side state without creating or starting
another guest process.

An ambiguous match fails explicitly instead of attaching a new CRI container to
the wrong restored process.

### A moved `old_id` caused a Rust build failure

The first multi-container adoption implementation moved `old_id` into
`ContainerIDMapping` and then attempted to insert it into the adopted-ID set.
The compiler correctly rejected the second use.

The mapping receives a clone while the owned value is retained for bookkeeping.
This was a compile-time ownership issue, not a restore protocol problem, but it
is recorded because it appeared while adding multi-container adoption.

### Repeated checkpoints exhausted the host filesystem

Each test checkpoint contained a private guest RAM file of approximately the
configured VM memory size. Failed and successful iterations accumulated under
temporary checkpoint directories until checkpoint failed with:

```text
No space left on device
```

Check free space and checkpoint sizes before interpreting this as a migration
failure:

```bash title="$ inspect checkpoint storage"
df -h /var/lib/kata/checkpoints
du -sh /var/lib/kata/checkpoints/*
```

Remove checkpoint directories that are no longer needed. A production
integration needs an explicit retention, cleanup, and quota policy; runtime
failure cleanup alone is not a substitute for checkpoint lifecycle management.

### Restores must not modify the saved overlay

Mounting the checkpoint's saved `rw-diff` directly as a restored container's
active upper directory makes the checkpoint one-shot. Guest activity and
cleanup can add whiteouts or otherwise mutate it, causing later restore attempts
to fail.

Each restore now clones the saved upper directory into a destination-specific
location:

```text
containers/<old-container-id>/restores/<new-sandbox-id>/
├── rw-diff/
├── rw-work/
└── merged/
```

The restored VM writes only to that clone. The original `rw-diff` remains a
reusable checkpoint artifact.

## Diagnostic order

Use this order to avoid debugging the wrong layer:

1. Confirm restore annotations reached `SandboxConfig`.
2. Confirm the destination QEMU command contains `-S -incoming defer`.
3. Confirm the private RAM file, `device-state`, `vm.json`, `metadata.json`,
   passthrough files, and per-container `rw-diff` exist.
4. Confirm the source QEMU has released the saved vsock CID and memory file.
5. Check QMP migration status and QEMU core dumps.
6. Check `virtiofsd` for missing source paths or protocol errors.
7. Confirm the host reconnects to kata-agent on the saved CID.
8. Confirm `RebindSandbox` maps the pause container and every workload.
9. Confirm CRI create/start adopts existing processes instead of executing new
   init processes.
10. Verify state continuity with a monotonic in-memory or filesystem counter.

## Verification result

The final QEMU run verified:

- checkpoint pauses, saves, and resumes the source VM;
- the bundle contains private RAM, device state, VM identity, metadata,
  passthrough files, and pause/workload overlay diffs;
- the source sandbox can be stopped and removed before restore;
- a normal CRI sandbox create is converted into incoming QEMU restore;
- kata-agent reconnects on the restored vsock CID;
- pause and workload IDs are rebound and adopted;
- the original process tree remains running; and
- workload state continues without resetting.

Focused checks also passed:

```bash title="$ repository checks"
cargo check
make docs-lint
git diff --check
```

## Remaining limitations

- Restore is same-node only.
- Overlay lower directories still refer to retained containerd image snapshots.
- The private Kata annotations are a temporary restore trigger.
- Preserved Pod networking, live TCP connections, extra writable volumes,
  GPU/VFIO devices, and cross-node restore are not covered.
- Checkpoint retention and restore-clone garbage collection need a lifecycle
  policy.
- Repeated checkpoint/restore cycles and failure rollback need broader stress
  testing.
