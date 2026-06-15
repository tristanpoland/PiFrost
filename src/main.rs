mod cli;
mod docker;
mod packer;
#[cfg(target_os = "windows")]
mod winflash;

use std::process::Command;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let is_admin = Command::new("powershell")
            .args(["-NoProfile", "-Command",
                "[bool](New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "True")
            .unwrap_or(false);

        if !is_admin {
            let exe = std::env::current_exe().unwrap();
            let cwd = std::env::current_dir().unwrap();
            let args: Vec<String> = std::env::args().skip(1).collect();
            let cmd = format!(
                "/c cd /d \"{}\" & \"{}\" {} & pause",
                cwd.display(),
                exe.display(),
                args.join(" ")
            );
            Command::new("powershell")
                .args(["-NoProfile", "-Command", &format!(
                    "Start-Process cmd -ArgumentList '{}' -Verb RunAs -Wait",
                    cmd.replace('\'', "''")
                )])
                .status()
                .ok();
            std::process::exit(0);
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_file(true)
        .with_line_number(true)
        .init();

    let result = cli::Cli::parse().run();

    #[cfg(target_os = "windows")]
    {
        println!();
        println!("Press Enter to exit...");
        let _ = std::io::stdin().read_line(&mut String::new());
    }

    result
}
