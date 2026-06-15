mod cli;
mod config;
mod provision;
mod ssh;

use clap::Parser;
use cli::{Cli, Commands};
use colored::Colorize;
use config::NodeConfig;

fn cmd_init() -> anyhow::Result<()> {
    use dialoguer::{Confirm, Input, Select};
    use config::NodeInfo;

    println!("{}", ">>> PiFrost Cluster Configuration\n".bright_cyan());

    let cluster_name: String = Input::new()
        .with_prompt("Cluster name")
        .default("pifrost-cluster".into())
        .interact_text()?;

    let num_servers: usize = Input::new()
        .with_prompt("Number of control-plane (server) nodes")
        .default(1)
        .validate_with(|v: &usize| if *v >= 1 { Ok(()) } else { Err("At least 1 server required") })
        .interact_text()?;

    let default_user: String = Input::new()
        .with_prompt("Default SSH user for all nodes")
        .default("root".into())
        .interact_text()?;

    // Global auth method
    let auth_idx = Select::new()
        .with_prompt("Default SSH authentication method")
        .items(&["SSH key", "Password"])
        .default(0)
        .interact()?;
    let default_auth = if auth_idx == 0 { "key" } else { "password" };

    let default_key_path: String = if default_auth == "key" {
        Input::new()
            .with_prompt("Default SSH key path")
            .default({
                let home = std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .unwrap_or_else(|_| "~".into());
                std::path::Path::new(&home).join(".ssh").join("id_rsa").display().to_string()
            })
            .interact_text()?
    } else {
        String::new()
    };

    let default_password: String = if default_auth == "password" {
        Input::new()
            .with_prompt("Default SSH password")
            .interact_text()?
    } else {
        String::new()
    };

    fn prompt_nodes(
        label: &str,
        count: usize,
        default_user: &str,
        default_auth: &str,
        default_key: &str,
        default_pass: &str,
    ) -> Vec<NodeInfo> {
        let mut nodes = Vec::new();
        for i in 0..count {
            println!("\n--- {} #{} ---", label, i + 1);
            let host: String = Input::new()
                .with_prompt("IP address or hostname")
                .interact_text()
                .unwrap();
            let port: u16 = Input::new().with_prompt("SSH port").default(22).interact_text().unwrap();
            let user: String = Input::new()
                .with_prompt("SSH user")
                .default(default_user.into())
                .interact_text()
                .unwrap();

            let auth_idx = Select::new()
                .with_prompt("Auth method for this node")
                .items(&["SSH key", "Password"])
                .default(if default_auth == "key" { 0 } else { 1 })
                .interact()
                .unwrap();
            let auth = if auth_idx == 0 { "key" } else { "password" };

            let (key_path, password) = if auth == "key" {
                let kp: String = Input::new()
                    .with_prompt("SSH key path")
                    .default(default_key.into())
                    .interact_text()
                    .unwrap();
                (kp, String::new())
            } else {
                let pw: String = Input::new()
                    .with_prompt("SSH password")
                    .default(default_pass.into())
                    .interact_text()
                    .unwrap();
                (String::new(), pw)
            };

            let hostname: String = Input::new()
                .with_prompt("Node hostname (leave empty to auto-generate)")
                .default(String::new())
                .interact_text()
                .unwrap();

            nodes.push(NodeInfo { hostname, host, port, user, auth: auth.into(), key_path, password });
        }
        nodes
    }

    let server_nodes = prompt_nodes(
        "Control-plane node",
        num_servers,
        &default_user,
        default_auth,
        &default_key_path,
        &default_password,
    );

    let use_as_workers = Confirm::new()
        .with_prompt("Also use control-plane nodes as workers (schedulable)?")
        .default(true)
        .interact()?;

    let num_workers: usize = Input::new()
        .with_prompt("Number of dedicated worker nodes")
        .default(0)
        .interact_text()?;
    let worker_nodes = prompt_nodes(
        "Worker node",
        num_workers,
        &default_user,
        default_auth,
        &default_key_path,
        &default_password,
    );

    println!();
    let rke2_version: String = Input::new()
        .with_prompt("RKE2 version")
        .default("v1.32.3+rke2r1".into())
        .interact_text()?;

    let deploy_rancher = Confirm::new()
        .with_prompt("Deploy Rancher management UI?")
        .default(true)
        .interact()?;

    let rancher_hostname: String = if deploy_rancher {
        Input::new()
            .with_prompt("Rancher UI hostname (or IP)")
            .default("rancher.pifrost.local".into())
            .interact_text()?
    } else {
        String::new()
    };

    let cfg = NodeConfig {
        cluster: config::ClusterConfig {
            name: cluster_name,
            use_servers_as_workers: use_as_workers,
            server_nodes,
            worker_nodes,
        },
        rke2: config::Rke2Config {
            version: rke2_version,
            extra_args: String::new(),
        },
        hardening: config::HardeningConfig::default(),
        rancher: config::RancherConfig {
            deploy: deploy_rancher,
            hostname: rancher_hostname,
        },
    };

    cfg.to_file("nodeconfig.toml")?;
    println!("\n{}", "✓ Wrote nodeconfig.toml".bright_green().bold());
    println!("  Run: pifrost provision");
    Ok(())
}

fn cmd_provision(path: &str, rancher_only: bool) -> anyhow::Result<()> {
    let cfg = NodeConfig::from_file(path)?;
    if rancher_only {
        println!(">>> Deploying Rancher dashboard only...");
    } else {
        println!(">>> Provisioning cluster '{}'...", cfg.cluster.name);
    }
    provision::provision_cluster(&cfg, rancher_only)
}

fn cmd_template() -> anyhow::Result<()> {
    let template = r#"[cluster]
name = "my-cluster"
use_servers_as_workers = false

[[cluster.server_nodes]]
host = "192.168.1.10"
port = 22
user = "root"
auth = "key"
key_path = "~/.ssh/id_rsa"
# password = ""       # only needed when auth = "password"
hostname = "ctrl-01"

[[cluster.server_nodes]]
host = "192.168.1.11"
port = 22
user = "root"
auth = "key"
key_path = "~/.ssh/id_rsa"
hostname = "ctrl-02"

[[cluster.server_nodes]]
host = "192.168.1.12"
port = 22
user = "root"
auth = "key"
key_path = "~/.ssh/id_rsa"
hostname = "ctrl-03"

[[cluster.worker_nodes]]
host = "192.168.1.20"
port = 22
user = "root"
auth = "key"
key_path = "~/.ssh/id_rsa"
hostname = "node-01"

[[cluster.worker_nodes]]
host = "192.168.1.21"
port = 22
user = "root"
auth = "key"
key_path = "~/.ssh/id_rsa"
hostname = "node-02"

[rke2]
version = "v1.32.3+rke2r1"
extra_args = ""

[hardening]
zero_logs = true
no_fs_bloat = true
disable_swap = true

[rancher]
deploy = true
hostname = "rancher.mycluster.local"
"#;
    println!("{}", template.bright_cyan());
    Ok(())
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init => cmd_init(),
        Commands::Provision { config, rancher_only } => cmd_provision(&config, rancher_only),
        Commands::Template => cmd_template(),
    }
}
