use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

const WORKER_IMAGE: &str = "pifrost-worker:latest";
const WORKER_DOCKERFILE: &str = r#"
FROM debian:bookworm-slim
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    parted e2fsprogs dosfstools curl ca-certificates util-linux mount kpartx xz-utils \
    && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["/bin/bash", "-c"]
"#;

pub struct DockerClient;

impl DockerClient {
    pub fn new() -> Result<Self> {
        info!("Verifying Docker daemon connectivity...");
        let output = Command::new("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output()
            .context("Failed to execute docker. Is Docker installed and running?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "Docker daemon is not accessible: {}\n\
                 pifrost requires Docker for cross-platform execution safety.\n\
                 Please install Docker Desktop or Docker Engine and ensure the daemon is running.",
                stderr.trim()
            );
        }
        info!(
            "Docker daemon detected (v{})",
            String::from_utf8_lossy(&output.stdout).trim()
        );

        let client = DockerClient;
        client.ensure_worker_image()?;
        Ok(client)
    }

    fn ensure_worker_image(&self) -> Result<()> {
        let output = Command::new("docker")
            .args(["image", "inspect", WORKER_IMAGE])
            .output()
            .context("Failed to inspect worker image")?;

        if output.status.success() {
            debug!("Worker image {} already exists", WORKER_IMAGE);
            return Ok(());
        }

        info!("Building worker image {}...", WORKER_IMAGE);
        let mut child = Command::new("docker")
            .args(["build", "-t", WORKER_IMAGE, "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("Failed to spawn docker build")?;

        use std::io::Write;
        if let Some(ref mut stdin) = child.stdin {
            stdin
                .write_all(WORKER_DOCKERFILE.as_bytes())
                .context("Failed to write Dockerfile to stdin")?;
        }
        drop(child.stdin.take());

        let output = child
            .wait_with_output()
            .context("Worker image build failed")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to build worker image:\n{}", stderr.trim());
        }

        info!("Worker image built successfully");
        Ok(())
    }

    fn run_docker_cmd(
        &self,
        container_args: &[&str],
        privileged: bool,
    ) -> Result<String> {
        let mut cmd = Command::new("docker");
        cmd.arg("run").args(["--rm"]);

        if privileged {
            cmd.arg("--privileged");
        }

        cmd.args([
            "-v",
            "/dev:/dev",
            "-v",
            &format!("{}:/build-images", resolve_host_path("./output")?),
            "--entrypoint",
            "/bin/bash",
            WORKER_IMAGE,
            "-c",
        ]);

        let script = container_args.join(" ");
        cmd.arg(&script);

        debug!("Running docker: {:?}", cmd);

        let output = cmd
            .output()
            .context("Failed to execute docker command")?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            bail!(
                "Docker container failed (exit code: {:?}):\nstdout: {}\nstderr: {}",
                output.status.code(),
                stdout.trim(),
                stderr.trim()
            );
        }

        Ok(stdout)
    }

    /// Auto-attach any USB storage devices to WSL2 via usbipd (Windows only).
    #[cfg(target_os = "windows")]
    fn ensure_usb_storage_attached(&self) {
        // Check if usbipd is available
        let has_usbipd = Command::new("usbipd")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !has_usbipd {
            println!("    installing usbipd (USB passthrough for WSL2)...");
            let _ = Command::new("winget")
                .args(["install", "--accept-source-agreements", "--accept-package-agreements",
                    "usbipd", "-h"])
                .output();
            // Refresh PATH
            match Command::new("powershell")
                .args(["-NoProfile", "-Command",
                    "& {[Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path','Machine'), 'Process')}"])
                .output()
            {
                Ok(_) => (),
                Err(_) => return,
            }
        }

        // List USB devices and find unattached storage ones
        let output = match Command::new("usbipd")
            .args(["wsl", "list"])
            .output()
        {
            Ok(o) => o,
            Err(_) => return,
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("BUSID") || line.starts_with('─') {
                continue;
            }
            let lower = line.to_lowercase();
            // Skip already attached devices
            if lower.contains("attached") && !lower.contains("not attached") {
                continue;
            }
            // Look for storage-related devices
            let is_storage = lower.contains("storage") || lower.contains("card")
                || lower.contains("flash") || lower.contains("usb disk")
                || lower.contains("mass") || lower.contains("sd");
            if !is_storage && !lower.contains("card") {
                continue;
            }
            // Extract BUSID (first whitespace-delimited token)
            let busid = line.split_whitespace().next().unwrap_or("");
            if busid.is_empty() || busid == "BUSID" {
                continue;
            }
            println!("    attaching USB storage ({}) via usbipd...", line);
            let _ = Command::new("usbipd")
                .args(["wsl", "attach", "--busid", busid])
                .output();
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
    }

    /// List available block devices visible inside the container.
    pub fn list_disks(&self) -> Result<Vec<String>> {
        #[cfg(target_os = "windows")]
        self.ensure_usb_storage_attached();

        let script = r#"
            lsblk -d -o NAME,SIZE,TYPE,MODEL -n 2>/dev/null | while read name size type model; do
                echo "/dev/$name  ($size, $type, $model)"
            done
        "#;
        let output = self.run_docker_cmd(&[script], true)?;
        let disks: Vec<String> = output
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if disks.is_empty() {
            #[cfg(target_os = "windows")]
            println!("No disks found inside Docker. Is your USB drive attached to WSL2?");
        }
        Ok(disks)
    }

    /// Wipe, partition, and pre-seed a target drive.
    #[allow(clippy::too_many_arguments)]
    pub fn bootstrap_drive(
        &self,
        drive: &str,
        name: &str,
        role: &str,
        token: &str,
        server_ip: Option<&str>,
        ip: Option<&str>,
        gateway: Option<&str>,
        dns: Option<&str>,
        kube_uuid: &str,
    ) -> Result<()> {
        let server_url = server_ip
            .map(|s| format!("https://{}:6443", s))
            .unwrap_or_default();

        let static_network = build_network_config(ip, gateway, dns);

        let script = format!(
            r#"set -euo pipefail

DEVICE="{drive}"
KUBE_UUID="{kube_uuid}"
NODE_NAME="{name}"
KUBE_ROLE="{role}"
CLUSTER_TOKEN="{token}"
SERVER_URL="{server_url}"
NETCONF='{static_network}'

echo ">>> Clearing partition table on $DEVICE"
parted -s "$DEVICE" mklabel gpt

echo ">>> Creating boot partition (512MB, FAT32)"
parted -s "$DEVICE" mkpart primary fat32 1MiB 513MiB
parted -s "$DEVICE" set 1 esp on
parted -s "$DEVICE" set 1 boot on

echo ">>> Creating OS partition (4GB, EXT4)"
parted -s "$DEVICE" mkpart primary ext4 513MiB 4609MiB

echo ">>> Creating kube-state partition (remainder, EXT4)"
parted -s "$DEVICE" mkpart primary ext4 4609MiB 100%

echo ">>> Waiting for kernel to re-read partition table"
sleep 2
partprobe "$DEVICE" 2>/dev/null || true
sleep 1

# Determine partition device names
if [[ "$DEVICE" == /dev/nvme* ]] || [[ "$DEVICE" == /dev/mmcblk* ]] || [[ "$DEVICE" == /dev/loop* ]]; then
    PART1="${{DEVICE}}p1"
    PART2="${{DEVICE}}p2"
    PART3="${{DEVICE}}p3"
else
    PART1="${{DEVICE}}1"
    PART2="${{DEVICE}}2"
    PART3="${{DEVICE}}3"
fi

echo ">>> Formatting boot partition (FAT32)"
mkfs.vfat -F 32 -n "ESP" "$PART1"

echo ">>> Formatting OS partition (EXT4)"
mkfs.ext4 -F -L "nixos" "$PART2"

echo ">>> Formatting kube-state partition (EXT4, UUID=$KUBE_UUID)"
mkfs.ext4 -F -L "kube-state" -U "$KUBE_UUID" "$PART3"

echo ">>> Seeding kube-state partition"
TMPMNT=$(mktemp -d)
mount "$PART3" "$TMPMNT"

# Generate machine-id
MACHINE_ID=$(head -c 32 /dev/urandom | xxd -p -c 64)
echo "$MACHINE_ID" > "$TMPMNT/machine-id"

# Seed directories
install -d -m 755 "$TMPMNT/k3s"
install -d -m 755 "$TMPMNT/containerd"
install -d -m 755 "$TMPMNT/etc/k3s"
install -d -m 755 "$TMPMNT/etc/network" 2>/dev/null || true

# Write node-mode.env
cat > "$TMPMNT/node-mode.env" << 'EOF2'
KUBE_ROLE="{role}"
NODE_NAME="{name}"
{server_line}
EOF2

# Write config.yaml for K3s
cat > "$TMPMNT/etc/k3s/config.yaml" << 'EOF3'
token: "{token}"
node-name: "{name}"
{k3s_config_extra}
EOF3

# Write network config if provided
if [ -n "$NETCONF" ] && [ "$NETCONF" != "" ]; then
    echo "$NETCONF" > "$TMPMNT/etc/network/interfaces" 2>/dev/null || true
fi

umount "$TMPMNT"
rmdir "$TMPMNT"
echo ">>> Bootstrap complete"
"#,
            drive = drive,
            kube_uuid = kube_uuid,
            name = name,
            role = role,
            token = token,
            server_url = server_url,
            static_network = static_network,
            server_line = if role == "agent" && !server_url.is_empty() {
                format!("SERVER_URL=\"{}\"", server_url)
            } else {
                String::new()
            },
            k3s_config_extra = if role == "server" && server_ip.is_none() {
                "cluster-init: true".to_string()
            } else if role == "agent" && !server_url.is_empty() {
                format!("server: \"{}\"", server_url)
            } else {
                String::new()
            },
        );

        info!("Bootstrapping drive {} (role: {}, name: {})", drive, role, name);
        self.run_docker_cmd(&[&script], true)?;
        Ok(())
    }

    /// Flash OS partitions (1 & 2) from a baked image, preserving partition 3 (kube-state).
    pub fn flash_drive(&self, drive: &str, image: &str) -> Result<()> {
        let abs_image = std::fs::canonicalize(image)
            .with_context(|| format!("Cannot resolve image path: {}", image))?;

        let script = format!(
            r#"set -euo pipefail

DEVICE="{drive}"
IMAGE="/build-images/{img_name}"

echo ">>> Verifying image exists"
if [ ! -f "$IMAGE" ]; then
    echo "ERROR: Image not found at $IMAGE"
    echo "Ensure the image is in the ./output directory (mounted at /build-images)."
    exit 1
fi

echo ">>> Attaching image via loopback"
kpartx -av "$IMAGE" 2>&1

# Determine loop device
LOOP_DEV=$(kpartx -l "$IMAGE" | head -1 | awk '{{print $1}}' | sed 's/p[0-9]//' | head -1)
if [ -z "$LOOP_DEV" ]; then
    LOOP_DEV=$(losetup --show -f "$IMAGE")
    kpartx -av "$LOOP_DEV" 2>&1
fi

echo ">>> Loop device: $LOOP_DEV"

# Map partition names
MAPPER_BASE=$(basename "$LOOP_DEV")

if [[ "$DEVICE" == /dev/nvme* ]] || [[ "$DEVICE" == /dev/mmcblk* ]] || [[ "$DEVICE" == /dev/loop* ]]; then
    TARGET_P1="${{DEVICE}}p1"
    TARGET_P2="${{DEVICE}}p2"
else
    TARGET_P1="${{DEVICE}}1"
    TARGET_P2="${{DEVICE}}2"
fi

echo ">>> Flashing partition 1 (boot) to $TARGET_P1"
dd if=/dev/mapper/${{MAPPER_BASE}}p1 of="$TARGET_P1" bs=4M status=progress conv=fsync

echo ">>> Flashing partition 2 (OS) to $TARGET_P2"
dd if=/dev/mapper/${{MAPPER_BASE}}p2 of="$TARGET_P2" bs=4M status=progress conv=fsync

echo ">>> Detaching loopback"
kpartx -dv "$IMAGE" 2>&1 || true
losetup -d "$LOOP_DEV" 2>/dev/null || true

echo ">>> Flash complete — kube-state partition (p3) preserved intact"
"#,
            drive = drive,
            img_name = abs_image
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
        );

        info!("Flashing OS from {} to {}", image, drive);
        self.run_docker_cmd(&[&script], true)?;
        Ok(())
    }
}

fn resolve_host_path(path: &str) -> Result<String> {
    let path = Path::new(path);
    if !path.exists() {
        std::fs::create_dir_all(path)
            .with_context(|| format!("Failed to create directory: {}", path.display()))?;
    }

    // Build absolute path without canonicalize (which can return \\?\ UNC prefixes on Windows
    // that confuse Docker Desktop's volume mount parser).
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let cwd = std::env::current_dir()
            .context("Failed to get current working directory")?;
        cwd.join(path)
    };

    let mut result = abs.to_string_lossy().replace('\\', "/");

    // Docker Desktop on Windows accepts native paths like C:/foo/bar — no need for /c/... conversion.
    // But we trim any trailing slash or dot that could confuse the mount parser.
    while result.ends_with('/') || result.ends_with('.') {
        result.truncate(result.len() - 1);
    }

    debug!("Resolved host path '{}' -> '{}'", path.display(), result);
    Ok(result)
}

fn build_network_config(ip: Option<&str>, gateway: Option<&str>, dns: Option<&str>) -> String {
    match (ip, gateway, dns) {
        (Some(ip), Some(gw), Some(dns)) => {
            format!(
                r#"auto lo
iface lo inet loopback

auto eth0
iface eth0 inet static
    address {ip}
    gateway {gw}
    dns-nameservers {dns}
"#
            )
        }
        (Some(ip), Some(gw), None) => {
            format!(
                r#"auto lo
iface lo inet loopback

auto eth0
iface eth0 inet static
    address {ip}
    gateway {gw}
"#
            )
        }
        _ => String::new(),
    }
}
