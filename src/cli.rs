use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pifrost", about = "Ubuntu Server provisioner — zero-log, no-FS-bloat, RKE2")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Interactively generate a nodeconfig.toml
    Init,

    /// Provision a node via SSH using a nodeconfig.toml
    Provision {
        /// Path to nodeconfig.toml (default: nodeconfig.toml)
        #[arg(default_value = "nodeconfig.toml")]
        config: String,

        /// Skip node provisioning and only deploy Rancher dashboard
        #[arg(long)]
        rancher_only: bool,
    },

    /// Print nodeconfig.toml template to stdout
    Template,
}
