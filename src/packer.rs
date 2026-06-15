use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

use crate::cli::BakeArgs;

pub struct PackerManager {
    packer_bin: String,
    use_docker: bool,
}

impl PackerManager {
    pub fn new() -> Result<Self> {
        if let Some(bin) = Self::find_local_packer() {
            info!("Packer detected at {}", bin);
            return Ok(Self {
                packer_bin: bin,
                use_docker: false,
            });
        }
        warn!("Packer not found in PATH or current dir — will use Dockerized Packer");
        Self::check_docker_for_packer()?;
        Ok(Self {
            packer_bin: String::new(),
            use_docker: true,
        })
    }

    fn find_local_packer() -> Option<String> {
        // Check PATH first
        if Command::new("packer")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some("packer".into());
        }
        // Check current directory for a packer binary
        let local = std::env::current_dir().ok()?.join("packer");
        if local.exists() {
            Command::new(&local)
                .arg("--version")
                .output()
                .ok()
                .filter(|o| o.status.success())?;
            return Some(local.to_string_lossy().to_string());
        }
        #[cfg(target_os = "windows")]
        {
            let local_exe = std::env::current_dir().ok()?.join("packer.exe");
            if local_exe.exists() {
                Command::new(&local_exe)
                    .arg("--version")
                    .output()
                    .ok()
                    .filter(|o| o.status.success())?;
                return Some(local_exe.to_string_lossy().to_string());
            }
        }
        None
    }

    fn check_docker_for_packer() -> Result<()> {
        let output = Command::new("docker")
            .args([
                "run",
                "--rm",
                "hashicorp/packer:latest",
                "--version",
            ])
            .output()
            .context("Failed to run Packer via Docker. Is Docker running?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "Packer is not available locally or via Docker: {}",
                stderr.trim()
            );
        }
        info!(
            "Dockerized Packer available (v{})",
            String::from_utf8_lossy(&output.stdout).trim()
        );
        Ok(())
    }

    /// Build the OS image using the Packer template.
    pub fn build_image(&self, args: &BakeArgs) -> Result<PathBuf> {
        let output_dir = Path::new(&args.output_dir);
        if !output_dir.exists() {
            std::fs::create_dir_all(output_dir)
                .context("Failed to create output directory")?;
        }

        let output_file = output_dir.join("stateless-debian-kube.img");
        if output_file.exists() && !args.force {
            info!(
                "Output image already exists at {} (use --force to rebuild)",
                output_file.display()
            );
            return Ok(output_file);
        }

        let template_path = locate_template()?;
        let template_dir = template_path
            .parent()
            .context("Failed to get template directory")?;

        let var_file = prepare_vars(args)?;

        if self.use_docker {
            self.build_via_docker(
                template_dir,
                &var_file,
                output_dir,
                &args.arch,
            )?;
        } else {
            self.build_locally(
                template_dir,
                &var_file,
                output_dir,
                &args.arch,
            )?;
        }

        if !output_file.exists() {
            bail!(
                "Build completed but output file not found at {}",
                output_file.display()
            );
        }

        let metadata = std::fs::metadata(&output_file)?;
        info!(
            "Image built: {} ({})",
            output_file.display(),
            humansize(metadata.len())
        );

        Ok(output_file)
    }

    fn build_locally(
        &self,
        template_dir: &Path,
        var_file: &Path,
        output_dir: &Path,
        arch: &str,
    ) -> Result<()> {
        let abs_template_dir = resolve_abs_path(template_dir)?;
        let abs_var_file = resolve_abs_path(var_file)?;
        let abs_output = resolve_abs_path(output_dir)?;

        info!("Running Packer build locally...");
        let status = Command::new(&self.packer_bin)
            .args([
                "build",
                "-var-file",
                &abs_var_file,
                "-var",
                &format!("output_dir={}", abs_output),
                "-var",
                &format!("arch={}", arch),
                &format!("{}/debian-node.pkr.hcl", abs_template_dir),
            ])
            .status()
            .context("Failed to execute packer build")?;

        if !status.success() {
            bail!("Packer build failed with exit code: {:?}", status.code());
        }
        Ok(())
    }

    fn build_via_docker(
        &self,
        template_dir: &Path,
        var_file: &Path,
        output_dir: &Path,
        arch: &str,
    ) -> Result<()> {
        let abs_template_dir = resolve_abs_path(template_dir)?;
        let abs_var_file = resolve_abs_path(var_file)?;
        let abs_output = resolve_abs_path(output_dir)?;

        let host_template = host_path_for_docker(&abs_template_dir)?;
        let host_var = host_path_for_docker(&abs_var_file)?;
        let host_output = host_path_for_docker(&abs_output)?;

        info!("Running Packer build via Docker...");
        let status = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--privileged",
                "-v",
                &format!("{}:/templates", host_template),
                "-v",
                &format!("{}:/output", host_output),
                "-v",
                &format!("{}:/vars.pkr.hcl", host_var),
                "-e",
                "PACKER_PLUGIN_DIR=/tmp/plugins",
                "hashicorp/packer:latest",
                "build",
                "-var-file=/vars.pkr.hcl",
                "-var",
                &format!("output_dir=/output"),
                "-var",
                &format!("arch={}", arch),
                "/templates/debian-node.pkr.hcl",
            ])
            .status()
            .context("Failed to execute Packer build via Docker")?;

        if !status.success() {
            bail!(
                "Dockerized Packer build failed with exit code: {:?}",
                status.code()
            );
        }
        Ok(())
    }
}

fn resolve_abs_path(path: &Path) -> Result<String> {
    if path.is_absolute() {
        Ok(path.to_string_lossy().replace('\\', "/"))
    } else {
        let cwd = std::env::current_dir()
            .context("Failed to get current working directory")?;
        Ok(cwd.join(path).to_string_lossy().replace('\\', "/"))
    }
}

fn locate_template() -> Result<PathBuf> {
    let candidates = [
        "templates/debian-node.pkr.hcl",
        "../templates/debian-node.pkr.hcl",
    ];
    for c in &candidates {
        if Path::new(c).exists() {
            let abs = if Path::new(c).is_absolute() {
                Path::new(c).to_path_buf()
            } else {
                std::env::current_dir()
                    .context("Failed to get current directory")?
                    .join(c)
            };
            return Ok(abs);
        }
    }
    // Search relative to the executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("templates/debian-node.pkr.hcl");
            if p.exists() {
                return Ok(p);
            }
        }
    }
    bail!(
        "Cannot locate template file 'templates/debian-node.pkr.hcl'. \
         Ensure the templates/ directory is present in the project root."
    );
}

fn prepare_vars(args: &BakeArgs) -> Result<PathBuf> {
    let var_content = format!(
        r#"kube_uuid = "{}"
arch = "{}"
output_dir = "{}"
"#,
        args.kube_uuid, args.arch, args.output_dir
    );

    let var_path = Path::new(&args.output_dir).join("build.vars.pkr.hcl");
    std::fs::write(&var_path, &var_content)
        .context("Failed to write Packer variable file")?;
    Ok(var_path)
}

fn host_path_for_docker(path: &str) -> Result<String> {
    let raw = path.replace('\\', "/");
    debug!("Docker host path: {}", raw);
    Ok(raw)
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
