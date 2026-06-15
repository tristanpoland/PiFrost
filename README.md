# pifrost — Immutable NixOS K3s Node Provisioner

**pifrost** bakes immutable NixOS raw disk images via `nix build`, partitions
bare-metal media (SD cards, NVMe, USB drives) inside a privileged Docker
container, and seeds zero-touch K3s nodes with automatic role detection.

All low-level disk operations (`parted`, `mkfs`, `dd`, `kpartx`) run inside an
ephemeral `pifrost-worker` Docker container so the tool works identically on
**Windows, macOS, and Linux** with no native partitioning tools required.

## How it works

```
┌──────────┐     ┌──────────────┐     ┌──────────────────────┐
│ pifrost  │────▶│  nix build   │────▶│  NixOS raw disk img  │
│ bake     │     │  (or Docker) │     │  (p1:FAT32 p2:EXT4)  │
└──────────┘     └──────────────┘     └──────────────────────┘

┌──────────┐     ┌──────────────┐     ┌──────────────────────┐
│ pifrost  │────▶│  Docker CLI  │────▶│  Worker Container    │
│ bootstrap│     │  (privileged)│     │  parted / mkfs / dd  │
└──────────┘     └──────────────┘     └──────────────────────┘

┌──────────┐     ┌──────────────┐     ┌──────────────────────┐
│ pifrost  │────▶│  kpartx + dd │────▶│  p1+p2 only, p3 kept │
│ flash    │     │  (loopback)  │     │  preserves kube-state│
└──────────┘     └──────────────┘     └──────────────────────┘
```

## Architecture

Each bootable drive has a three-partition GPT layout:

| # | Label | Size | Format | Content |
|---|-------|------|--------|---------|
| 1 | `ESP` | 512 MB | FAT32 | systemd-boot + kernel |
| 2 | `nixos` | 4 GB | EXT4 | NixOS store + root |
| 3 | `kube-state` | Remainder | EXT4 | Persistent K3s data |

**The OS root is ephemeral.** NixOS impermanence mounts a tmpfs root; only
`/mnt/kube-state` (partition 3) persists across reboots, holding K3s state,
containerd data, machine identity, and cluster configuration. Symlinks redirect
`/var/lib/rancher/k3s`, `/var/lib/containerd`, and `/etc/rancher/k3s` into the
persistent partition.

During early boot, the `kube-identity` systemd service:
1. Mounts the kube-state partition read-only
2. Bind-mounts the seeded `machine-id`
3. Reads `node-mode.env` → decides server or agent role
4. Enables the correct K3s systemd service
5. Stages any pre-seeded network configuration

## Prerequisites

- **Rust toolchain** (edition 2021)
- **Docker** (Desktop or Engine) — required for `bootstrap` and `flash`
- **Nix package manager** (optional) — if missing, `bake` falls back to
  `nixos/nix` via Docker

## Install

```bash
git clone <repo> && cd pifrost
cargo build --release
./target/release/pifrost --help
```

## Usage

### 1. Bake — Build the NixOS raw disk image

```bash
pifrost bake --output-dir ./output
```

This generates `./output/stateless-debian-kube.img` by running `nix build .#rawImage`
(either with a local Nix installation or inside `nixos/nix` Docker container).
The image contains a minimal NixOS installation with:

- systemd-boot on EFI System Partition
- Impermanence (ephemeral root, data flows to kube-state)
- K3s binaries cached (server + agent), neither enabled by default
- `kube-identity` early-boot systemd service
- Kernel cgroup flags and sysctl settings for Kubernetes
- Symlinks: K3s runtime directories → `/mnt/kube-state/`

Options:
| Flag | Default | Description |
|------|---------|-------------|
| `--arch` | `x86_64-linux` | NixOS platform string |
| `--output-dir` | `./output` | Output directory |
| `--force` | false | Rebuild even if output exists |

### 2. Bootstrap — Partition and seed a drive

Connect your target drive (SD card, NVMe-to-USB, etc.).

```bash
# Interactive — lists available disks
pifrost bootstrap

# Non-interactive
pifrost bootstrap \
  --drive /dev/sdc \
  --name k8s-control-1 \
  --role server \
  --token my-cluster-token
```

The command:
1. Wipes the partition table and creates GPT layout (p1 FAT32 512 MB, p2 EXT4 4 GB, p3 EXT4 remainder)
2. Formats all three partitions with correct labels (`ESP`, `nixos`, `kube-state`)
3. Generates a random 32-char hex `machine-id`
4. Seeds `node-mode.env` for auto role detection
5. Writes `config.yaml` for K3s configuration
6. Creates empty `k3s/`, `containerd/`, `etc/k3s/` directories on kube-state

**Agent example:**
```bash
pifrost bootstrap \
  --drive /dev/sdc \
  --name k8s-worker-1 \
  --role agent \
  --server-ip 192.168.1.100 \
  --token my-cluster-token
```

**Static networking:**
```bash
pifrost bootstrap \
  --drive /dev/sdc \
  --name k8s-control-1 \
  --role server \
  --token my-token \
  --ip 192.168.1.100/24 \
  --gateway 192.168.1.1 \
  --dns 192.168.1.1
```

### 3. Flash — Update OS while preserving K3s state

When you have a new image but want to keep the kube-state partition:

```bash
pifrost flash --drive /dev/sdc --image ./output/stateless-debian-kube.img
```

This attaches the image via loopback (`kpartx`), then **only writes partitions
1 and 2** to the target drive. Partition 3 (kube-state) is untouched — cluster
identity, K3s state, and containerd data survive the upgrade.

## What happens on first boot

1. UEFI boots from partition 1 (systemd-boot)
2. Kernel loads with `cgroup_enable=cpuset cgroup_enable=memory`
3. `kube-identity.service` runs **before** `local-fs-pre.target`:
   - Mounts partition 3 read-only at `/mnt/kube-state`
   - Copies `machine-id` → bind-mounts over `/etc/machine-id`
   - Reads `node-mode.env` → enables `k3s.service` or `k3s-agent.service`
   - Stages network configs from `/mnt/kube-state/etc/network/`
4. Impermanence takes effect — root is a tmpfs snapshot
5. K3s starts with config from `/mnt/kube-state/etc/k3s/config.yaml`
6. All runtime data flows through symlinks to `/mnt/kube-state/`

## File layout on kube-state partition

```
/mnt/kube-state/
├── machine-id              # Cryptographically random identity
├── node-mode.env           # KUBE_ROLE + NODE_NAME
├── k3s/                    # Symlink target: /var/lib/rancher/k3s
├── containerd/             # Symlink target: /var/lib/containerd
└── etc/
    └── k3s/
        └── config.yaml     # K3s cluster config
```

## How pifrost finds Nix

Priority order for `bake`:
1. **`nix` on `PATH`** — runs `nix build .#rawImage` locally
2. **`nixos/nix` Docker image** — auto-pulled if Nix is absent (always the case
   on Windows)

## Safety

- Every destructive operation requires explicit typed confirmation
- The `flash` command never touches partition 3
- Docker daemon verification at startup
- All subprocess stdout/stderr captured and reported on failure

## Development

```bash
cargo check
cargo build --release
```

## License

MIT
