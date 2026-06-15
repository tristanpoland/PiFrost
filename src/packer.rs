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
            .ok_or_else(|| {
                anyhow::anyhow!("Docker daemon is not running")
            })
    }

    pub fn build_image(&self, args: &BakeArgs) -> Result<PathBuf> {
        let output_dir = Path::new(&args.output_dir);

        let final_file = output_dir.join("stateless-debian-kube.img");
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

        if output_dir.exists() {
            std::fs::remove_dir_all(output_dir)
                .context("Failed to remove existing output directory")?;
        }
        std::fs::create_dir_all(output_dir)
            .context("Failed to create output directory")?;

        info!("Building NixOS raw image...");
        let status = Command::new("nix")
            .args([
                "build",
                "--no-sandbox",
                "--out-link",
                &output_dir.join("result").to_string_lossy(),
                &format!("{}#rawImage", flake_dir),
            ])
            .status()
            .context("Failed to execute nix build")?;

        if !status.success() {
            bail!("nix build failed with exit code: {:?}", status.code());
        }

        let built = output_dir.join("result").join("disk.raw");
        if !built.exists() {
            bail!("Nix build completed but disk.raw not found");
        }

        std::fs::copy(&built, final_file)
            .context("Failed to copy raw image")?;
        let _ = std::fs::remove_dir_all(output_dir.join("result"));

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
        let host_output = resolve_host_path(
            &output_dir.to_string_lossy(),
        )?;

        if output_dir.exists() {
            std::fs::remove_dir_all(output_dir)
                .context("Failed to remove existing output directory")?;
        }
        std::fs::create_dir_all(output_dir)
            .context("Failed to create output directory")?;

        info!("Building NixOS raw image via Docker (nixos/nix)...");

        let script = r#"set -eux
cd /host-project
# Ensure Nix files are Git-tracked (required by Nix in a Git repo)
git add flake.nix nix/ 2>/dev/null || true
nix build --no-sandbox --out-link /host-output/result .#rawImage
if [ -f /host-output/result/disk.raw ]; then
  cp /host-output/result/disk.raw /host-output/stateless-debian-kube.img
  rm -r /host-output/result
fi
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
