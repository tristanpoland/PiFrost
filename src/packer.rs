use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tracing::info;

use crate::cli::BakeArgs;

pub struct PackerManager {
    use_docker: bool,
}

impl PackerManager {
    pub fn new() -> Result<Self> {
        if Self::check_nix() {
            info!("Nix detected on host");
            return Ok(PackerManager { use_docker: false });
        }

        info!("Nix not found on host — checking Docker for nixos/nix image");
        Self::check_docker().context(
            "Nix is not installed and Docker is not available.\n\
             Install Nix: https://nixos.org/download.html\n\
             Or install Docker Desktop for Nix-in-Docker fallback.",
        )?;
        Ok(PackerManager { use_docker: true })
    }

    fn check_nix() -> bool {
        Command::new("nix")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn check_docker() -> Result<()> {
        Command::new("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output()
            .context("Docker daemon not accessible")?
            .status
            .success()
            .then_some(())
            .ok_or_else(|| anyhow::anyhow!("Docker daemon is not running"))
    }

    pub fn build_image(&self, args: &BakeArgs) -> Result<PathBuf> {
        let output_dir = Path::new(&args.output_dir);

        let final_file = output_dir.join("pifrost-node.img");
        if final_file.exists() && !args.force {
            info!(
                "Output image already exists at {} (use --force to rebuild)",
                final_file.display()
            );
            return Ok(final_file);
        }

        if self.use_docker {
            self.build_via_docker(args, &final_file)
        } else {
            self.build_locally(args, &final_file)
        }
    }

    fn build_locally(&self, _args: &BakeArgs, final_file: &Path) -> Result<PathBuf> {
        let flake_dir = resolve_flake_dir()?;
        let output_dir = final_file.parent().unwrap_or(Path::new("."));

        let _ = std::fs::remove_dir_all(output_dir);
        std::fs::create_dir_all(output_dir)
            .context("Failed to create output directory")?;

        info!("Building NixOS system closure...");
        let status = Command::new("nix")
            .args([
                "build",
                "--no-sandbox",
                "--out-link",
                &output_dir.join("result").to_string_lossy(),
                &format!(
                    "{}#nixosConfigurations.pifrost-node.config.system.build.toplevel",
                    flake_dir
                ),
            ])
            .status()
            .context("Failed to execute nix build")?;

        if !status.success() {
            bail!("nix build failed with exit code: {:?}", status.code());
        }

        info!("Building nixos-install-tools...");
        let status = Command::new("nix")
            .args([
                "build",
                "--no-sandbox",
                "--out-link",
                &output_dir.join("install-tools").to_string_lossy(),
                "nixpkgs#nixos-install-tools",
            ])
            .status()
            .context("Failed to build nixos-install-tools")?;

        if !status.success() {
            bail!("Failed to build nixos-install-tools");
        }

        self.build_image_from_closure(output_dir, final_file)?;

        info!(
            "Image built: {} ({})",
            final_file.display(),
            humansize(std::fs::metadata(final_file)?.len())
        );
        Ok(final_file.to_path_buf())
    }

    fn build_via_docker(&self, _args: &BakeArgs, final_file: &Path) -> Result<PathBuf> {
        let output_dir = final_file.parent().unwrap_or(Path::new("."));
        let host_project = resolve_host_path(".")?;
        let host_output = resolve_host_path(&output_dir.to_string_lossy())?;

        let _ = std::fs::remove_dir_all(output_dir);
        std::fs::create_dir_all(output_dir)
            .context("Failed to create output directory")?;

        let script = r#"set -eux

cd /host-project
git add flake.nix nix/ 2>/dev/null || true

echo "=== Building NixOS system closure ==="
nix build --no-sandbox --out-link /host-output/closure \
  .#nixosConfigurations.pifrost-node.config.system.build.toplevel

echo "=== Building nixos-install-tools ==="
nix build --no-sandbox --out-link /host-output/install-tools \
  nixpkgs#nixos-install-tools

echo "=== Building disk utilities ==="
mkdir -p /host-output/tools/bin
for pkg in parted e2fsprogs dosfstools utillinux rsync gnused; do
  nix build --no-sandbox --out-link "/host-output/tools/$pkg" "nixpkgs#$pkg" 2>&1
  if [ -d "/host-output/tools/$pkg/bin" ]; then
    cp -r "/host-output/tools/$pkg/bin/"* /host-output/tools/bin/ 2>/dev/null || true
  fi
done
export PATH="/host-output/tools/bin:$PATH"

echo "=== Creating raw disk image ==="
DISK_IMAGE=/host-output/stateless-debian-kube.img
rm -f "$DISK_IMAGE"
dd if=/dev/zero of="$DISK_IMAGE" bs=1M count=5000 status=progress

echo "=== Partitioning ==="
parted -s "$DISK_IMAGE" mklabel gpt
parted -s "$DISK_IMAGE" mkpart primary fat32 1MiB 513MiB
parted -s "$DISK_IMAGE" set 1 esp on
parted -s "$DISK_IMAGE" set 1 boot on
parted -s "$DISK_IMAGE" mkpart primary ext4 513MiB 100%

echo "=== Setting up loopback ==="
LOOP=$(losetup --show -f "$DISK_IMAGE")
kpartx -av "$LOOP"
sleep 1
P1=/dev/mapper/$(basename "$LOOP")p1
P2=/dev/mapper/$(basename "$LOOP")p2

echo "=== Formatting ==="
mkfs.vfat -F 32 -n "ESP" "$P1"
mkfs.ext4 -F -L "nixos" "$P2"

echo "=== Mounting ==="
mkdir -p /mnt/root
mount "$P2" /mnt/root
mkdir -p /mnt/root/boot
mount "$P1" /mnt/root/boot

echo "=== Installing NixOS to image ==="
CLOSURE=$(readlink /host-output/closure)
INSTALL_TOOLS=$(readlink /host-output/install-tools)

# nixos-install copies the closure, runs activation, sets up bootloader
"$INSTALL_TOOLS/bin/nixos-install" \
  --root /mnt/root \
  --system "$CLOSURE" \
  --no-root-password \
  --no-bootloader 2>&1

if [ $? -ne 0 ]; then
  echo "WARNING: nixos-install had issues (continuing)"
fi

echo "=== Installing systemd-boot manually ==="
# Find systemd-boot in the closure
SYSTEMD_BOOT=$(find "$CLOSURE" -name "systemd-boot*.efi" -type f | head -1)
if [ -n "$SYSTEMD_BOOT" ]; then
  BOOTNAME=$(basename "$SYSTEMD_BOOT" | sed 's/systemd-boot/BOOT/')
  mkdir -p /mnt/root/boot/EFI/systemd /mnt/root/boot/EFI/BOOT
  cp "$SYSTEMD_BOOT" /mnt/root/boot/EFI/systemd/
  cp "$SYSTEMD_BOOT" "/mnt/root/boot/EFI/BOOT/$BOOTNAME"

  # Generate loader config
  mkdir -p /mnt/root/boot/loader/entries
  cat > /mnt/root/boot/loader/loader.conf << 'LOADER'
default nixos
timeout 5
console-mode max
editor no
LOADER

  CLOSURE_NAME=$(basename "$CLOSURE")
  cat > /mnt/root/boot/loader/entries/nixos.conf << 'ENTRY'
title NixOS
linux /nix/store/CLOSURE_NAME/kernel
initrd /nix/store/CLOSURE_NAME/initrd
options init=/nix/store/CLOSURE_NAME/init loglevel=4
ENTRY
  sed -i "s|CLOSURE_NAME|$CLOSURE_NAME|g" /mnt/root/boot/loader/entries/nixos.conf
fi

echo "=== Cleaning up ==="
sync
umount /mnt/root/boot 2>/dev/null || true
umount /mnt/root 2>/dev/null || true
kpartx -dv "$LOOP" 2>/dev/null || true
losetup -d "$LOOP" 2>/dev/null || true

cp "$DISK_IMAGE" /host-output/stateless-debian-kube.img.final
mv /host-output/stateless-debian-kube.img.final /host-output/stateless-debian-kube.img
echo "=== Image built successfully ==="
"#;

        let status = Command::new("docker")
            .args([
                "run", "--rm", "--privileged",
                "-v", &format!("{}:/host-project", host_project),
                "-v", &format!("{}:/host-output", host_output),
                "-e", "NIX_CONFIG=experimental-features = nix-command flakes",
                "nixos/nix",
                "/bin/sh", "-c", &script,
            ])
            .status()
            .context("Failed to run Nix build in Docker")?;

        if !status.success() {
            bail!("Nix build via Docker failed with exit code: {:?}", status.code());
        }

        if !final_file.exists() {
            bail!("Docker build completed but output not found");
        }

        info!(
            "Image built: {} ({})",
            final_file.display(),
            humansize(std::fs::metadata(final_file)?.len())
        );
        Ok(final_file.to_path_buf())
    }

    fn build_image_from_closure(
        &self,
        output_dir: &Path,
        final_file: &Path,
    ) -> Result<()> {
        // For local builds, run the disk assembly via Docker (pifrost-worker)
        // since it needs root/loopback
        let host_output = resolve_host_path(&output_dir.to_string_lossy())?;
        let final_name = final_file.file_name().unwrap().to_string_lossy();

        let script = format!(
            r#"set -eux
DISK_IMAGE=/build-images/{final_name}
rm -f "$DISK_IMAGE"
dd if=/dev/zero of="$DISK_IMAGE" bs=1M count=5000 status=progress

parted -s "$DISK_IMAGE" mklabel gpt
parted -s "$DISK_IMAGE" mkpart primary fat32 1MiB 513MiB
parted -s "$DISK_IMAGE" set 1 esp on
parted -s "$DISK_IMAGE" set 1 boot on
parted -s "$DISK_IMAGE" mkpart primary ext4 513MiB 100%

LOOP=$(losetup --show -f "$DISK_IMAGE")
kpartx -av "$LOOP"
sleep 1
P1=/dev/mapper/$(basename "$LOOP")p1
P2=/dev/mapper/$(basename "$LOOP")p2

mkfs.vfat -F 32 -n "ESP" "$P1"
mkfs.ext4 -F -L "nixos" "$P2"

mkdir -p /mnt/root
mount "$P2" /mnt/root
mkdir -p /mnt/root/boot
mount "$P1" /mnt/root/boot

# Copy store contents
mkdir -p /mnt/root/nix/store
cp -a /build-images/result/. /mnt/root/nix/store/

CLOSURE=$(find /mnt/root/nix/store -maxdepth 1 -name "*-nixos-system-*" | head -1)
if [ -z "$CLOSURE" ]; then
  echo "ERROR: system closure not found in store"
  exit 1
fi

CLOSURE_NAME=$(basename "$CLOSURE")

# Create system profile
mkdir -p /mnt/root/nix/var/nix/profiles/system-1-link
ln -sfn /nix/store/$CLOSURE_NAME /mnt/root/nix/var/nix/profiles/system-1-link
ln -sfn system-1-link /mnt/root/nix/var/nix/profiles/system
mkdir -p /mnt/root/nix/var/nix/profiles/per-user/root

mkdir -p /mnt/root/etc
echo "NixOS" > /mnt/root/etc/NIXOS

# Install systemd-boot
SYSTEMD_BOOT=$(find "$CLOSURE" -name "systemd-boot*.efi" -type f | head -1)
if [ -n "$SYSTEMD_BOOT" ]; then
  BOOTNAME=$(basename "$SYSTEMD_BOOT" | sed 's/systemd-boot/BOOT/')
  mkdir -p /mnt/root/boot/EFI/systemd /mnt/root/boot/EFI/BOOT
  cp "$SYSTEMD_BOOT" /mnt/root/boot/EFI/systemd/
  cp "$SYSTEMD_BOOT" "/mnt/root/boot/EFI/BOOT/$BOOTNAME"

  mkdir -p /mnt/root/boot/loader/entries
  cat > /mnt/root/boot/loader/loader.conf << 'LOADER'
default nixos
timeout 5
console-mode max
editor no
LOADER

  cat > /mnt/root/boot/loader/entries/nixos.conf << 'ENTRY'
title NixOS
linux /nix/store/CLOSURE_NAME/kernel
initrd /nix/store/CLOSURE_NAME/initrd
options init=/nix/store/CLOSURE_NAME/init loglevel=4
ENTRY
  sed -i "s|CLOSURE_NAME|$CLOSURE_NAME|g" /mnt/root/boot/loader/entries/nixos.conf
fi

sync
umount /mnt/root/boot 2>/dev/null || true
umount /mnt/root 2>/dev/null || true
kpartx -dv "$LOOP" 2>/dev/null || true
losetup -d "$LOOP" 2>/dev/null || true
echo "=== Image built ==="
"#
        );

        let status = Command::new("docker")
            .args([
                "run", "--rm", "--privileged",
                "-v", &format!("{}:/build-images", host_output),
                "--entrypoint", "/bin/bash",
                "pifrost-worker:latest",
                "-c", &script,
            ])
            .status()
            .context("Failed to assemble disk image in Docker")?;

        if !status.success() {
            bail!("Image assembly in Docker failed");
        }

        Ok(())
    }
}

fn resolve_flake_dir() -> Result<String> {
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let flake = cwd.join("flake.nix");
    if flake.exists() {
        return Ok(cwd.to_string_lossy().replace('\\', "/"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.join("flake.nix").exists() {
                return Ok(dir.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    bail!("Cannot locate flake.nix in current directory or executable directory");
}

fn resolve_host_path(path: &str) -> Result<String> {
    let p = Path::new(path);
    if p.exists() {
        if p.is_absolute() {
            Ok(p.to_string_lossy().replace('\\', "/"))
        } else {
            let cwd = std::env::current_dir().context("Failed to get cwd")?;
            Ok(cwd.join(p).to_string_lossy().replace('\\', "/"))
        }
    } else {
        Ok(path.replace('\\', "/"))
    }
}

fn humansize(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    format!("{:.2} {}", size, UNITS[unit_idx])
}
