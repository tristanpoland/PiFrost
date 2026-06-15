use std::time::Duration;

use anyhow::{Context, Result};
use colored::Colorize;

use crate::config::{HardeningConfig, NodeConfig, NodeInfo, RancherConfig, Rke2Config};
use crate::ssh::SshClient;

/// Helper to wrap a command with password-fed sudo
fn with_sudo(node: &NodeInfo, cmd: &str) -> String {
    format!("echo '{}' | sudo -S {}", node.password, cmd)
}

/// Provision the full cluster: all control-plane nodes, worker nodes, then Rancher.
pub fn provision_cluster(cfg: &NodeConfig, rancher_only: bool) -> Result<()> {
    let cluster = &cfg.cluster;

    if cluster.server_nodes.is_empty() {
        anyhow::bail!("At least one server (control-plane) node is required");
    }

    let first = &cluster.server_nodes[0];
    let first_ip = &first.host;

    if !rancher_only {
        let total = cfg.total_nodes();
        println!(
            ">>> Provisioning cluster '{}' ({} nodes: {} servers + {} workers)",
            cluster.name,
            total,
            cluster.server_nodes.len(),
            cluster.worker_nodes.len(),
        );

        // 1. Bootstrap first server
        println!("\n--- Bootstrapping first server {} ---", first_ip);
        let ssh = SshClient::connect(first)?;
        provision_single_node(&ssh, &cfg.hardening, &cfg.rke2, None, first, cfg.cluster.use_servers_as_workers)?;

        // 2. Wait for RKE2 to start and read the join token
        println!("\n--- Waiting for RKE2 to start on first server ---");
        std::thread::sleep(Duration::from_secs(15));
        let token = get_join_token(&ssh, first)?;
        println!("    join token obtained");

        // 3. Join remaining server nodes
        for (i, node) in cluster.server_nodes.iter().enumerate().skip(1) {
            println!("\n--- Joining server {}/{} ({}) ---", i + 1, cluster.server_nodes.len(), node.host);
            let ssh = SshClient::connect(node)?;
            let server_config = format!("server: https://{}:9345\ntoken: {}", first_ip, token);
            provision_single_node(&ssh, &cfg.hardening, &cfg.rke2, Some(&server_config), node, cfg.cluster.use_servers_as_workers)?;
        }

        // 4. Provision worker nodes
        for (i, node) in cluster.worker_nodes.iter().enumerate() {
            println!("\n--- Provisioning worker {}/{} ({}) ---", i + 1, cluster.worker_nodes.len(), node.host);
            let ssh = SshClient::connect(node)?;
            let agent_config = format!("server: https://{}:9345\ntoken: {}", first_ip, token);
            // Workers use the agent install type
            let rke2_agent = Rke2Config { version: cfg.rke2.version.clone(), extra_args: cfg.rke2.extra_args.clone() };
            provision_single_node_agent(&ssh, &cfg.hardening, &rke2_agent, &agent_config, node)?;
        }
    }

    // 5. Deploy Rancher if enabled (or if rancher-only mode)
    if cfg.rancher.deploy || rancher_only {
        println!("\n--- Deploying Rancher dashboard ---");
        let ssh = SshClient::connect(first)?;
        deploy_rancher(&ssh, &cfg.rancher, first)?;
    }

    if rancher_only {
        println!("\n{}", "✓ Rancher deployment complete!".green().bold());
    } else {
        println!("\n{}", "✓ Cluster provisioning complete!".green().bold());
        println!("  First server:  ssh {}@{}", first.user, first.host);
        println!("  RKE2 config:   /etc/rancher/rke2/rke2.yaml");
    }

    if cfg.rancher.deploy || rancher_only {
        println!("  Rancher UI:    https://{} (user: admin / password: admin)", cfg.rancher.hostname);
    }
    Ok(())
}

/// Provision a single server node with hardening + RKE2 server.
fn provision_single_node(
    ssh: &SshClient,
    hardening: &HardeningConfig,
    rke2: &Rke2Config,
    join_config: Option<&str>,
    node: &NodeInfo,
    use_as_worker: bool, // if true, remove control-plane taint
) -> Result<()> {
    println!("    hardening node...");
    apply_hardening(ssh, hardening, node)?;
    set_hostname(ssh, &node_info_hostname(node, "server"), node)?;

    let mut config_lines = vec![
        "token: pifrost-cluster-token".to_string(),
        "embedded-registry: true".to_string(),
        r#"write-kubeconfig-mode: "0644""#.to_string(),
    ];

    if use_as_worker {
        config_lines.push("node-taint: []".to_string());
    }

    if let Some(join) = join_config {
        config_lines.push(join.to_string());
    }

    if !rke2.extra_args.is_empty() {
        config_lines.push(rke2.extra_args.clone());
    }

    let config_yaml = config_lines.join("\n");
    install_rke2(ssh, rke2, "server", &config_yaml, node)?;

    Ok(())
}

/// Provision a single worker node with hardening + RKE2 agent.
fn provision_single_node_agent(
    ssh: &SshClient,
    hardening: &HardeningConfig,
    rke2: &Rke2Config,
    join_config: &str,
    node: &NodeInfo,
) -> Result<()> {
    println!("    hardening node...");
    apply_hardening(ssh, hardening, node)?;
    set_hostname(ssh, &node_info_hostname(node, "worker"), node)?;

    let config_yaml = format!("{}\n{}", join_config, rke2.extra_args);
    install_rke2(ssh, rke2, "agent", &config_yaml, node)?;

    Ok(())
}

/// Read the node-token from the first server after RKE2 starts.
fn get_join_token(ssh: &SshClient, node: &NodeInfo) -> Result<String> {
    for attempt in 0..30 {
        std::thread::sleep(Duration::from_secs(2));
        // sudo needed to read privileged /var/lib/rancher/... files
        let cmd = with_sudo(node, "cat /var/lib/rancher/rke2/server/node-token 2>/dev/null");
        let (stdout, _, code) = ssh
            .exec_allow_fail(&cmd)
            .context("Failed to read node-token")?;
        if code == 0 {
            let token = stdout.trim();
            if !token.is_empty() {
                // Extract the actual token value (format: K...<hash>::server:<value>)
                if let Some(value) = token.split("::server:").nth(1) {
                    return Ok(value.trim().to_string());
                }
                return Ok(token.to_string());
            }
        }
        if attempt == 0 {
            println!("    waiting for RKE2 to generate node-token...");
        }
    }
    anyhow::bail!("Timed out waiting for RKE2 node-token on first server")
}

/// Generate a hostname for a node based on its role and index.
fn node_info_hostname(node: &NodeInfo, role: &str) -> String {
    if !node.hostname.is_empty() {
        node.hostname.clone()
    } else {
        format!("pifrost-{}-{}", role, node.host.replace('.', "-"))
    }
}

/// Apply all hardening measures to a node.
fn apply_hardening(ssh: &SshClient, hardening: &HardeningConfig, node: &NodeInfo) -> Result<()> {
    if hardening.zero_logs {
        configure_journald_volatile(ssh, node)?;
        disable_auditd(ssh, node)?;
    }
    if hardening.no_fs_bloat {
        add_tmpfs_mounts(ssh, node)?;
        set_noatime(ssh, node)?;
    }
    if hardening.disable_swap {
        disable_swap(ssh, node)?;
    }
    apply_sysctl_k8s(ssh, node)?;
    Ok(())
}

fn set_hostname(ssh: &SshClient, hostname: &str, node: &NodeInfo) -> Result<()> {
    ssh.exec(&with_sudo(node, &format!("hostnamectl set-hostname {}", hostname)))?;
    println!("    hostname → {}", hostname);
    Ok(())
}

fn configure_journald_volatile(ssh: &SshClient, node: &NodeInfo) -> Result<()> {
    ssh.exec(&with_sudo(node, "mkdir -p /etc/systemd/journald.conf.d"))?;

    // Upload to /tmp first, then move with sudo to the protected location
    let tmp_path = "/tmp/99-pifrost-journald.conf";
    let target_path = "/etc/systemd/journald.conf.d/99-pifrost.conf";
    ssh.upload_str(
        "[Journal]\nStorage=volatile\nRuntimeMaxUse=100M\nMaxRetentionSec=1day\nForwardToSyslog=no\nSystemMaxUse=0\n",
        tmp_path,
    )?;
    ssh.exec(&with_sudo(node, &format!("mv {} {}", tmp_path, target_path)))?;

    ssh.exec(&with_sudo(node, "systemctl restart systemd-journald"))?;
    println!("    journald → volatile");
    Ok(())
}

fn disable_auditd(ssh: &SshClient, node: &NodeInfo) -> Result<()> {
    let (_, _, code) = ssh.exec_allow_fail("systemctl is-enabled auditd 2>/dev/null")?;
    if code == 0 {
        ssh.exec(&with_sudo(node, "systemctl stop auditd && systemctl disable auditd"))?;
        println!("    auditd → disabled");
    }
    Ok(())
}

fn add_tmpfs_mounts(ssh: &SshClient, node: &NodeInfo) -> Result<()> {
    let entries = [
        ("tmpfs", "/var/log", "tmpfs", "defaults,noatime,mode=0755,size=100M"),
        ("tmpfs", "/tmp", "tmpfs", "defaults,noatime,mode=1777,size=500M"),
        ("tmpfs", "/var/tmp", "tmpfs", "defaults,noatime,mode=1777,size=100M"),
    ];
    for (fs, dir, typ, opts) in &entries {
        let (_, _, code) = ssh.exec_allow_fail(&format!("grep -q ' {} ' /etc/fstab", dir))?;
        if code != 0 {
            let line = format!("{} {} {} {} 0 0", fs, dir, typ, opts);
            // Use bash -c to ensure the redirection happens within sudo context
            ssh.exec(&with_sudo(node, &format!("bash -c \"echo '{}' >> /etc/fstab\"", line)))?;
            ssh.exec(&with_sudo(node, &format!("mkdir -p {}", dir)))?;
            ssh.exec(&with_sudo(node, &format!("mount {}", dir)))?;
        }
    }
    println!("    tmpfs → /var/log, /tmp, /var/tmp");
    Ok(())
}

fn set_noatime(ssh: &SshClient, node: &NodeInfo) -> Result<()> {
    let _ = ssh.exec_allow_fail(&with_sudo(node, "mount -o remount,noatime /"));
    let _ = ssh.exec_allow_fail(&with_sudo(node, r#"sed -i 's/  errors=remount-ro/  noatime,errors=remount-ro/' /etc/fstab"#));
    println!("    root → noatime");
    Ok(())
}

fn disable_swap(ssh: &SshClient, node: &NodeInfo) -> Result<()> {
    ssh.exec(&with_sudo(node, "swapoff -a"))?;
    ssh.exec(&with_sudo(node, r#"sed -i '/swap/d' /etc/fstab"#))?;
    println!("    swap → disabled");
    Ok(())
}

fn apply_sysctl_k8s(ssh: &SshClient, node: &NodeInfo) -> Result<()> {
    // Upload to /tmp first, then move with sudo to the protected location
    let tmp_path = "/tmp/99-pifrost-sysctl.conf";
    let target_path = "/etc/sysctl.d/99-pifrost.conf";
    ssh.upload_str(
        "net.bridge.bridge-nf-call-iptables=1\nnet.ipv4.ip_forward=1\nnet.ipv6.conf.all.forwarding=1\nkernel.panic=10\nkernel.panic_on_oops=1\n",
        tmp_path,
    )?;
    ssh.exec(&with_sudo(node, &format!("mv {} {}", tmp_path, target_path)))?;
    ssh.exec(&with_sudo(node, "sysctl --system"))?;
    println!("    sysctl → K8s params");
    Ok(())
}

/// Install or verify RKE2 is present, then write config.yaml and enable the service.
fn install_rke2(ssh: &SshClient, rke2: &Rke2Config, role: &str, config_yaml: &str, node: &NodeInfo) -> Result<()> {
    let exists = ssh.file_exists("/usr/local/bin/rke2")?;
    if !exists {
        println!("    RKE2 → installing v{} ({})...", rke2.version, role);
        let type_env = match role {
            "agent" => "INSTALL_RKE2_TYPE=agent ",
            _ => "",
        };
        // Run the installation script with sudo in a bash context
        let install_cmd = format!(
            "curl -sfL https://get.rke2.io | {}INSTALL_RKE2_VERSION={} sh -",
            type_env, rke2.version
        );
        ssh.exec(&with_sudo(node, &format!("bash -c '{}'", install_cmd)))?;
    } else {
        println!("    RKE2 → already installed");
    }

    // Write config.yaml - upload to /tmp first, then move with sudo
    let config_dir = "/etc/rancher/rke2";
    ssh.exec(&with_sudo(node, &format!("mkdir -p {}", config_dir)))?;
    let tmp_path = "/tmp/rke2-config.yaml";
    let target_path = format!("{}/config.yaml", config_dir);

    // Debug: show what we're writing
    println!("    RKE2 config content:\n{}", config_yaml);

    ssh.upload_str(config_yaml, tmp_path)?;
    ssh.exec(&with_sudo(node, &format!("mv {} {}", tmp_path, target_path)))?;

    // Verify what was written
    let (file_content, _, _) = ssh.exec_allow_fail(&with_sudo(node, &format!("cat {}", target_path)))?;
    println!("    RKE2 config → written\n    Actual file content:\n{}", file_content);

    // Enable & start
    let service = format!("rke2-{}", role);
    let (_, _, code) = ssh.exec_allow_fail(&format!("systemctl is-enabled {}", service))?;
    if code != 0 {
        // Enable the service first
        ssh.exec(&with_sudo(node, &format!("systemctl enable {}", service)))?;
        println!("    RKE2 {} → enabled", service);
    }

    // Start or restart the service
    let (_, _, start_code) = ssh.exec_allow_fail(&with_sudo(node, &format!("systemctl start {}", service)))?;

    // Check if service is actually running
    let (_, _, is_active) = ssh.exec_allow_fail(&with_sudo(node, &format!("systemctl is-active {}", service)))?;

    if is_active != 0 || start_code != 0 {
        // Service failed to start - get detailed status
        println!("\n!!! RKE2 service failed to start - collecting diagnostics...");
        let (status_out, _, _) = ssh.exec_allow_fail(&with_sudo(node, &format!("systemctl status {} --no-pager", service)))?;
        let (journal_out, _, _) = ssh.exec_allow_fail(&with_sudo(node, &format!("journalctl -xeu {} --no-pager -n 100", service)))?;

        println!("\n=== Service Status ===\n{}", status_out);
        println!("\n=== Journal Logs (last 100 lines) ===\n{}", journal_out);

        anyhow::bail!("Failed to start {} service", service);
    }

    println!("    RKE2 {} → started", service);

    Ok(())
}

/// Deploy Rancher management UI via Helm on the first server node.
fn deploy_rancher(ssh: &SshClient, rancher: &RancherConfig, node: &NodeInfo) -> Result<()> {
    let kube_env = "export KUBECONFIG=/etc/rancher/rke2/rke2.yaml && export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/var/lib/rancher/rke2/bin";

    // 1. Install Helm
    let (_, _, code) = ssh.exec_allow_fail("command -v helm")?;
    if code != 0 {
        println!("    installing Helm...");
        // Download script to user's home directory
        let script_path = format!("/home/{}/get-helm.sh", node.user);
        println!("    downloading Helm install script to {}...", script_path);

        let (stdout, stderr, dl_code) = ssh.exec_allow_fail(&format!("curl -sfL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 -o {}", script_path))?;
        if dl_code != 0 {
            anyhow::bail!("Failed to download Helm script: {} / {}", stdout, stderr);
        }

        // Verify the script exists and is readable
        let (_, _, check_code) = ssh.exec_allow_fail(&format!("test -f {}", script_path))?;
        if check_code != 0 {
            anyhow::bail!("Helm script was not created at {}", script_path);
        }

        println!("    executing Helm install script with sudo...");
        ssh.exec(&with_sudo(node, &format!("bash {}", script_path)))?;
        ssh.exec(&format!("rm -f {}", script_path))?;
    }

    // 2. Add Rancher repo
    let helm_cmd = format!("{} && helm repo add rancher-stable https://releases.rancher.com/server-charts/stable && helm repo update", kube_env);
    ssh.exec(&with_sudo(node, &format!("bash -c '{}'", helm_cmd)))?;

    // 3. Create namespace
    let ns_cmd = format!("{} && kubectl create namespace cattle-system --dry-run=client -o yaml | kubectl apply -f -", kube_env);
    ssh.exec(&with_sudo(node, &format!("bash -c '{}'", ns_cmd)))?;

    // 4. Install cert-manager
    println!("    installing cert-manager...");
    let check_cm_cmd = format!("{} && kubectl get ns cert-manager 2>/dev/null", kube_env);
    let (_, _, cm_code) = ssh.exec_allow_fail(&with_sudo(node, &format!("bash -c '{}'", check_cm_cmd)))?;
    if cm_code != 0 {
        let apply_cm_cmd = format!(
            "{} && kubectl apply -f https://github.com/cert-manager/cert-manager/releases/download/v1.16.0/cert-manager.yaml",
            kube_env
        );
        ssh.exec(&with_sudo(node, &format!("bash -c '{}'", apply_cm_cmd)))?;

        // Wait for cert-manager pods
        println!("    waiting for cert-manager...");
        let wait_cm_cmd = format!(
            "{} && kubectl wait --for=condition=Available --timeout=120s -n cert-manager deployment/cert-manager deployment/cert-manager-webhook deployment/cert-manager-cainjector",
            kube_env
        );
        ssh.exec(&with_sudo(node, &format!("bash -c '{}'", wait_cm_cmd)))?;
    }

    // 5. Install Rancher
    println!("    installing Rancher...");
    let hostname = &rancher.hostname;
    let rancher_install_cmd = format!(
        "{} && helm upgrade --install rancher rancher-stable/rancher \
         --namespace cattle-system --create-namespace \
         --set hostname={} \
         --set bootstrapPassword=admin \
         --set replicas=1 \
         --wait",
        kube_env, hostname
    );
    ssh.exec(&with_sudo(node, &format!("bash -c '{}'", rancher_install_cmd)))?;

    println!("    Rancher UI: https://{}", hostname);
    Ok(())
}