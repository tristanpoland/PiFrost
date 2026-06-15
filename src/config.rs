use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    #[serde(default)]
    pub hostname: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_ssh_user")]
    pub user: String,
    /// "key" or "password"
    #[serde(default = "default_auth_method")]
    pub auth: String,
    /// Path to SSH private key (used when auth = "key")
    #[serde(default = "default_key_path")]
    pub key_path: String,
    /// SSH password (used when auth = "password")
    #[serde(default)]
    pub password: String,
}

fn default_auth_method() -> String { "key".into() }

fn default_port() -> u16 { 22 }
fn default_ssh_user() -> String { "root".into() }

fn default_key_path() -> String {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "~".into());
    std::path::Path::new(&home).join(".ssh").join("id_rsa").display().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    #[serde(default = "default_cluster_name")]
    pub name: String,
    #[serde(default)]
    pub use_servers_as_workers: bool,
    pub server_nodes: Vec<NodeInfo>,
    #[serde(default)]
    pub worker_nodes: Vec<NodeInfo>,
}

fn default_cluster_name() -> String { "pifrost-cluster".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rke2Config {
    #[serde(default = "default_rke2_version")]
    pub version: String,
    #[serde(default)]
    pub extra_args: String,
}

impl Default for Rke2Config {
    fn default() -> Self {
        Rke2Config {
            version: default_rke2_version(),
            extra_args: String::new(),
        }
    }
}

fn default_rke2_version() -> String { "v1.32.3+rke2r1".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardeningConfig {
    #[serde(default = "default_true")]
    pub zero_logs: bool,
    #[serde(default = "default_true")]
    pub no_fs_bloat: bool,
    #[serde(default = "default_true")]
    pub disable_swap: bool,
}

impl Default for HardeningConfig {
    fn default() -> Self {
        HardeningConfig { zero_logs: true, no_fs_bloat: true, disable_swap: true }
    }
}

fn default_true() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RancherConfig {
    #[serde(default)]
    pub deploy: bool,
    #[serde(default = "default_rancher_hostname")]
    pub hostname: String,
}

fn default_rancher_hostname() -> String { "rancher.pifrost.local".into() }

impl Default for RancherConfig {
    fn default() -> Self {
        RancherConfig { deploy: true, hostname: default_rancher_hostname() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub cluster: ClusterConfig,
    #[serde(default)]
    pub rke2: Rke2Config,
    #[serde(default)]
    pub hardening: HardeningConfig,
    #[serde(default)]
    pub rancher: RancherConfig,
}

impl NodeConfig {
    pub fn total_nodes(&self) -> usize {
        let servers = self.cluster.server_nodes.len();
        let workers = self.cluster.worker_nodes.len();
        servers + workers
    }

    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let cfg: NodeConfig = toml::from_str(&contents)?;
        Ok(cfg)
    }

    pub fn to_file(&self, path: &str) -> anyhow::Result<()> {
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(path, contents)?;
        Ok(())
    }
}
