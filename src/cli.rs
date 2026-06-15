use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::*;

use crate::docker::DockerClient;
use crate::packer::PackerManager;

#[derive(Parser)]
#[command(
    name = "pifrost",
    about = "Immutable K3s node image baker & bare-metal provisioner",
    version,
    long_about = "pifrost builds NixOS raw disk images with nix build, partitions \
    bare-metal media via Docker isolation, and seeds automated K3s nodes."
)]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build a NixOS raw disk image using nix build
    Bake(BakeArgs),
    /// Wipe, partition, and pre-seed a drive for zero-touch autoinstall
    Bootstrap(BootstrapArgs),
    /// Flash OS partitions while preserving Kube data partition
    Flash(FlashArgs),
}

#[derive(clap::Args, Clone)]
pub struct BakeArgs {
    /// Target CPU architecture (NixOS config must match)
    #[arg(long, default_value = "x86_64-linux")]
    pub arch: String,

    /// Output directory for the baked image
    #[arg(long, default_value = "./output")]
    pub output_dir: String,

    /// Force rebuild even if output exists
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

#[derive(clap::Args, Clone)]
pub struct BootstrapArgs {
    /// Target block device (e.g., /dev/sdb). If omitted, lists available disks interactively.
    #[arg(long)]
    pub drive: Option<String>,

    /// Node hostname
    #[arg(long, default_value = "kube-node")]
    pub name: String,

    /// Node role
    #[arg(long, default_value = "server", value_parser = clap::builder::PossibleValuesParser::new(["server", "agent"]))]
    pub role: String,

    /// Cluster shared token (auto-generated if omitted)
    #[arg(long)]
    pub token: Option<String>,

    /// Control-plane endpoint IP (required for agents)
    #[arg(long)]
    pub server_ip: Option<String>,

    /// Static IP in CIDR notation (e.g., 192.168.1.100/24)
    #[arg(long)]
    pub ip: Option<String>,

    /// Gateway IP
    #[arg(long)]
    pub gateway: Option<String>,

    /// DNS server IP
    #[arg(long)]
    pub dns: Option<String>,

    /// UUID for the kube-state partition
    #[arg(long, default_value = "deadbeef-1234-5678-9abc-def012345678")]
    pub kube_uuid: String,

    /// Skip interactive confirmation
    #[arg(long, default_value_t = false)]
    pub yes: bool,
}

#[derive(clap::Args, Clone)]
pub struct FlashArgs {
    /// Target block device. If omitted, lists available disks interactively.
    #[arg(long)]
    pub drive: Option<String>,

    /// Path to the baked .img file
    #[arg(long)]
    pub image: String,

    /// Skip interactive confirmation
    #[arg(long, default_value_t = false)]
    pub yes: bool,
}

impl Cli {
    pub fn run(self) -> Result<()> {
        match self.command {
            Commands::Bake(args) => cmd_bake(args),
            Commands::Bootstrap(args) => cmd_bootstrap(args),
            Commands::Flash(args) => cmd_flash(args),
        }
    }
}

fn cmd_bake(args: BakeArgs) -> Result<()> {
    println!("{}", ">>> Building NixOS raw disk image...".bold().cyan());
    println!("    output-dir: {}", args.output_dir);
    println!();

    let packer = PackerManager::new()?;
    packer
        .build_image(&args)
        .context("Image build failed")?;

    println!();
    println!(
        "{}",
        "✔ Image built successfully!".bold().green()
    );
    Ok(())
}

fn cmd_bootstrap(args: BootstrapArgs) -> Result<()> {
    let docker = DockerClient::new()?;

    let drive = match &args.drive {
        Some(d) => d.clone(),
        None => {
            println!(
                "{}",
                ">>> No --drive specified. Scanning for available disks...".bold().yellow()
            );
            let disks = docker.list_disks().context("Failed to list disks")?;
            if disks.is_empty() {
                anyhow::bail!("No block devices found. Insert a drive and retry.");
            }
            select_disk_interactive(&disks)?
        }
    };

    let token = args.token.clone().unwrap_or_else(|| {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        hex::encode((0..16).map(|_| rng.gen::<u8>()).collect::<Vec<_>>())
    });

    println!("{}", "\n>>> Bootstrap Configuration:".bold().cyan());
    println!("    drive:      {}", drive);
    println!("    name:       {}", args.name);
    println!("    role:       {}", args.role);
    println!("    token:      {}", token);
    if let Some(ref sip) = args.server_ip {
        println!("    server-ip:  {}", sip);
    }
    if let Some(ref ip) = args.ip {
        println!("    ip:         {}", ip);
    }
    if let Some(ref gw) = args.gateway {
        println!("    gateway:    {}", gw);
    }
    if let Some(ref dns) = args.dns {
        println!("    dns:        {}", dns);
    }
    println!("    kube-uuid:  {}", args.kube_uuid);
    println!();

    if !args.yes {
        confirm_destructive_action(&drive)?;
    }

    docker
        .bootstrap_drive(
            &drive,
            &args.name,
            &args.role,
            &token,
            args.server_ip.as_deref(),
            args.ip.as_deref(),
            args.gateway.as_deref(),
            args.dns.as_deref(),
            &args.kube_uuid,
        )
        .context("Drive bootstrap failed")?;

    println!();
    println!(
        "{}",
        format!("✔ Drive {} bootstrapped successfully!", drive)
            .bold()
            .green()
    );
    Ok(())
}

fn cmd_flash(args: FlashArgs) -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    let docker = DockerClient::new()?;

    let drive = match &args.drive {
        Some(d) => d.clone(),
        None => {
            println!(
                "{}",
                ">>> No --drive specified. Scanning for available disks...".bold().yellow()
            );
            #[cfg(target_os = "windows")]
            let disks = {
                let raw = crate::winflash::list_physical_drives().context("Failed to list disks")?;
                raw.iter()
                    .map(|(idx, model, size)| {
                        format!("\\\\.\\PhysicalDrive{}  ({} - {})", idx, crate::winflash::format_size(*size), model)
                    })
                    .collect::<Vec<_>>()
            };
            #[cfg(not(target_os = "windows"))]
            let disks = docker.list_disks().context("Failed to list disks")?;
            if disks.is_empty() {
                anyhow::bail!("No block devices found. Insert a drive and retry.");
            }
            select_disk_interactive(&disks)?
        }
    };

    println!("{}", ">>> Flashing OS to drive...".bold().cyan());
    println!("    drive: {}", drive);
    println!("    image: {}", args.image);
    println!();

    if !args.yes {
        confirm_destructive_action(&drive)?;
    }

    #[cfg(target_os = "windows")]
    {
        // Extract drive number from "\\.\PhysicalDriveN" or "\\.\PhysicalDriveN  (...)"
        let trimmed = drive.trim_start_matches(r"\\.\PhysicalDrive")
            .trim_start_matches(r"\\.\PHYSICALDRIVE");
        let idx: u32 = trimmed.split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("")
            .parse()
            .with_context(|| format!("Cannot parse drive number from '{}'", drive))?;
        crate::winflash::flash_image_to_drive(&args.image, idx)
            .context("Drive flash failed")?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        docker
            .flash_drive(&drive, &args.image)
            .context("Drive flash failed")?;
    }

    println!();
    println!(
        "{}",
        format!("✔ Drive {} flashed successfully!", drive)
            .bold()
            .green()
    );
    Ok(())
}

fn select_disk_interactive(disks: &[String]) -> Result<String> {
    use dialoguer::{Select, theme::ColorfulTheme};
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select target disk")
        .items(disks)
        .default(0)
        .interact()
        .context("Disk selection cancelled")?;
    Ok(disks[selection].clone())
}

fn confirm_destructive_action(drive: &str) -> Result<()> {
    use dialoguer::Confirm;
    let confirmed = Confirm::new()
        .with_prompt(format!(
            "{}",
            format!(
                "WARNING: This will DESTROY ALL DATA on {}. Are you absolutely sure?",
                drive
            )
            .red()
            .bold()
        ))
        .default(false)
        .interact()
        .context("Confirmation prompt failed")?;
    if !confirmed {
        anyhow::bail!("Aborted by user.");
    }
    Ok(())
}
