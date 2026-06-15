use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::config::NodeInfo;

pub struct SshClient {
    node: NodeInfo,
}

impl SshClient {
    pub fn connect(node: &NodeInfo) -> Result<Self> {
        let (_, _, code) = Self::run(node, "echo connected")?;
        if code != 0 {
            bail!("Failed to connect to {}@{}", node.user, node.host);
        }
        Ok(SshClient { node: node.clone() })
    }

    fn base_args(node: &NodeInfo) -> Vec<String> {
        let mut args = vec![
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            "UserKnownHostsFile=NUL".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
            format!("{}@{}", node.user, node.host),
        ];
        if node.auth == "key" && !node.key_path.is_empty() {
            args.push("-i".into());
            args.push(node.key_path.clone());
        }
        args
    }

    /// Execute a command and return (stdout, stderr, exit_code).
    fn run(node: &NodeInfo, command: &str) -> Result<(String, String, i32)> {
        let mut args = Self::base_args(node);
        args.push(command.into());

        let mut cmd = Command::new("ssh");
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if node.auth == "password" {
            let script = create_askpass_script(&node.password)?;
            cmd.env("SSH_ASKPASS", &script).env("SSH_ASKPASS_REQUIRE", "force");
        }

        let output = cmd
            .output()
            .with_context(|| format!("Failed to spawn ssh for {}", node.host))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let code = output.status.code().unwrap_or(-1);

        // Clean up the temporary askpass script
        if node.auth == "password" {
            let script = std::env::temp_dir().join("pifrost").join("askpass.bat");
            let _ = std::fs::remove_file(&script);
        }

        Ok((stdout, stderr, code))
    }

    /// Run a command; bails on non-zero exit.
    pub fn exec(&self, command: &str) -> Result<(String, String)> {
        let (stdout, stderr, code) = Self::run(&self.node, command)?;
        if code != 0 {
            bail!(
                "Command exited with code {}:\n  stderr: {}",
                code,
                stderr.trim()
            );
        }
        Ok((stdout, stderr))
    }

    /// Run a command, return (stdout, stderr, exit_code) regardless of exit code.
    pub fn exec_allow_fail(&self, command: &str) -> Result<(String, String, i32)> {
        Self::run(&self.node, command)
    }

    /// Upload a string as a remote file using a heredoc over SSH.
    pub fn upload_str(&self, content: &str, remote: &str) -> Result<()> {
        let cmd = format!(
            "mkdir -p \"$(dirname '{}')\" && cat > '{}' << 'PIFROST_EOF'\n{}\nPIFROST_EOF",
            remote, remote, content
        );
        let (_, _, code) = Self::run(&self.node, &cmd)?;
        if code != 0 {
            bail!("Failed to upload to remote path {}", remote);
        }
        Ok(())
    }

    /// Upload a local file via scp.
    pub fn upload(&self, local: &str, remote: &str) -> Result<()> {
        let mut args = vec![
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            "UserKnownHostsFile=NUL".into(),
        ];
        if self.node.auth == "key" && !self.node.key_path.is_empty() {
            args.push("-i".into());
            args.push(self.node.key_path.clone());
        }
        args.push(local.into());
        args.push(format!("{}@{}:{}", self.node.user, self.node.host, remote));

        let status = Command::new("scp")
            .args(&args)
            .stdin(Stdio::null())
            .status()
            .with_context(|| format!("Failed to scp {} to {}", local, remote))?;
        if !status.success() {
            bail!("scp failed with exit code {:?}", status.code());
        }
        Ok(())
    }

    /// Download a remote file via scp.
    pub fn download(&self, remote: &str, local: &str) -> Result<()> {
        let mut args = vec![
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            "UserKnownHostsFile=NUL".into(),
        ];
        if self.node.auth == "key" && !self.node.key_path.is_empty() {
            args.push("-i".into());
            args.push(self.node.key_path.clone());
        }
        args.push(format!("{}@{}:{}", self.node.user, self.node.host, remote));
        args.push(local.into());

        let status = Command::new("scp")
            .args(&args)
            .stdin(Stdio::null())
            .status()
            .with_context(|| format!("Failed to scp {} from {}", remote, local))?;
        if !status.success() {
            bail!("scp failed with exit code {:?}", status.code());
        }
        Ok(())
    }

    /// Check if a file exists on the remote.
    pub fn file_exists(&self, path: &str) -> Result<bool> {
        let (_, _, code) = self.exec_allow_fail(&format!("test -f '{}'", path))?;
        Ok(code == 0)
    }
}

/// Create a temporary askpass batch file that outputs the password.
fn create_askpass_script(password: &str) -> Result<String> {
    let dir = std::env::temp_dir().join("pifrost");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("askpass.bat");
    let mut file = std::fs::File::create(&path)?;
    writeln!(file, "@echo off")?;
    writeln!(file, "echo {}", password)?;
    Ok(path.to_string_lossy().to_string())
}

impl Drop for SshClient {
    fn drop(&mut self) {}
}
