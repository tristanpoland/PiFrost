# pifrost — Immutable K3s Node Provisioner

**pifrost** bakes stateless Debian 12 (Bookworm) Kubernetes node images with
HashiCorp Packer, partitions bare-metal media (SD cards, NVMe, USB drives) via
Docker isolation, and seeds fully automated, zero-touch K3s nodes.

## How it works

```
┌──────────┐     ┌──────────────┐     ┌──────────────────────┐
│ pifrost  │────▶│  Docker CLI  │────▶│  Worker Container    │
│ bake     │     │  (privileged)│     │  parted / mkfs / dd  │
└──────────┘     └──────────────┘     └──────────────────────┘
                                                     │
                     ┌──────────────┐                │
                     │  Packer      │◀───────────────┘
                     │  QEMU build  │  (or local packer)
                     └──────────────┘
```

All low-level disk operations run inside an ephemeral Docker container so
pifrost works identically on **Windows, macOS, and Linux** with no native
partitioning tools required.

## Architecture

Each bootable drive gets a three-partition layout:

| # | Label | Size | Format | Content |
|---|-------|------|--------|---------|
| 1 | `system-boot` | 512 MB | FAT32 | GRUB + kernel |
| 2 | `writable` | 4 GB | EXT4 | Immutable OS (overlayroot) |
| 3 | `kube-state` | Remainder | EXT4 | Persistent K3s data |

**The OS is read-only at runtime.** Overlayroot places root on a volatile tmpfs
ramdisk. Only `/mnt/kube-state` (the third partition) persists across reboots,
holding K3s state, containerd data, machine identity, and cluster config.

During early boot, the `kube-identity` systemd service:
1. Mounts the kube-state partition read-only
2. Bind-mounts `machine-id` from the seeded partition
3. Reads `node-mode.env` to decide server vs. agent role
4. Enables the correct K3s service
5. Stages any pre-seeded network configuration

## Prerequisites

- **Rust toolchain** (edition 2021, MSRV 1.79+)
- **Docker** (Desktop or Engine) — mandatory for all operations
- **Packer** (optional) — if missing, pifrost auto-detects `packer` on `PATH`,
  then checks `./packer` in the current directory, then falls back to
  `hashicorp/packer:latest` via Docker

```bash
# Verify Docker
docker info

# (Optional) Install Packer locally
# https://developer.hashicorp.com/packer/downloads
```

## Install

```bash
git clone <repo> && cd pifrost
cargo build --release
./target/release/pifrost --help
```

## Usage

### 1. Bake — Build the OS image

```bash
pifrost bake --arch amd64 --output-dir ./output
```

This generates `./output/stateless-debian-kube.img` — a raw disk image
containing the full Debian 12 installation with:

- overlayroot (read-only root on tmpfs)
- K3s binaries pre-cached (both server and agent)
- Symlinks from `/var/lib/rancher/k3s` → `/mnt/kube-state/k3s`
- kube-identity early-boot systemd service
- Kernel cgroup flags for Kubernetes
- Correct sysctl settings (net.bridge-nf-call-iptables, etc.)

Options:
| Flag | Default | Description |
|------|---------|-------------|
| `--arch` | `amd64` | `amd64` or `arm64` |
| `--kube-uuid` | `deadbeef-...` | UUID for kube-state partition |
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
  --token my-cluster-token \
  --kube-uuid deadbeef-1234-5678-9abc-def012345678
```

The command:
1. Wipes the partition table and creates GPT layout
2. Formats all three partitions (FAT32, EXT4, EXT4 with your UUID)
3. Generates a random 32-char hex `machine-id`
4. Seeds `node-mode.env` for auto role detection
5. Writes `config.yaml` to `/etc/k3s/` for K3s
6. Creates empty `k3s/`, `containerd/`, `etc/k3s/` directories

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

### 3. Flash — Update OS while preserving K3s data

When you have a new image but want to keep the kube-state partition intact:

```bash
pifrost flash --drive /dev/sdc --image ./output/stateless-debian-kube.img
```

This mounts the image via loopback, then **only writes partitions 1 and 2** to
the target drive. Partition 3 (kube-state) is left untouched — your cluster
identity, K3s state, and containerd data survive the upgrade.

## What happens on first boot

1. UEFI/BIOS boots from partition 1 (GRUB)
2. Kernel loads from partition 2 with `cgroup_enable=cpuset cgroup_enable=memory`
3. `kube-identity.service` runs **before** `local-fs-pre.target`:
   - Mounts partition 3 read-only at `/mnt/kube-state`
   - Reads `machine-id` → bind-mounts over `/etc/machine-id`
   - Reads `node-mode.env` → enables `k3s-server.service` or `k3s-agent.service`
   - Stages network configs from `/mnt/kube-state/etc/network/`
4. Overlayroot takes effect — root becomes read-only tmpfs
5. K3s starts with config from `/mnt/kube-state/etc/k3s/config.yaml`
6. All runtime state flows to `/mnt/kube-state/k3s` and `/mnt/kube-state/containerd`

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

## How pifrost finds Packer

Priority order:
1. **`packer` on `PATH`** — globally installed
2. **`./packer` (or `./packer.exe`)** — local binary shipped with project
3. **`hashicorp/packer:latest`** — Docker image (auto-pulled)

## Safety

- Every destructive operation requires an explicit typed confirmation
- The `flash` command never touches partition 3
- Docker verification happens at startup — pifrost refuses to run without it
- All subprocess stderr/stdout is captured and reported on failure

## Development

```bash
cargo check      # Verify compilation
cargo build      # Debug build
cargo build --release  # Release build
```

## License

MIT
