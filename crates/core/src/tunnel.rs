//! SSH tunnels: a local TCP listener whose connections are forwarded through an SSH
//! `direct-tcpip` channel to the database host as seen from the bastion.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::config::{Credentials, SshAuth, SshConfig};

struct Handler {
    host: String,
    port: u16,
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        // Trust-on-first-use against ~/.ssh/known_hosts: unknown hosts are accepted, but a key
        // that contradicts a recorded one is rejected (possible MITM).
        if let PublicKeyOrCertificate::PublicKey { key, .. } = key
            && let Err(e) = russh::keys::check_known_hosts(&self.host, self.port, key)
        {
            tracing::warn!("SSH host key check failed for {}: {e}", self.host);
            return Ok(false);
        }
        Ok(true)
    }
}

/// An open tunnel. Dropping it stops accepting new local connections.
pub struct SshTunnel {
    pub local_addr: SocketAddr,
    accept_task: JoinHandle<()>,
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

pub fn expand_tilde(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    std::path::PathBuf::from(path)
}

/// Opens an SSH session and starts forwarding `127.0.0.1:<random>` to `target_host:target_port`.
pub async fn open_tunnel(
    ssh: &SshConfig,
    creds: &Credentials,
    target_host: &str,
    target_port: u16,
) -> Result<SshTunnel> {
    if ssh.host.trim().is_empty() {
        bail!("SSH host is empty");
    }
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });
    let handler = Handler { host: ssh.host.clone(), port: ssh.port };
    let mut handle =
        tokio::time::timeout(Duration::from_secs(15), client::connect(config, (ssh.host.as_str(), ssh.port), handler))
            .await
            .context("SSH connection timed out")?
            .with_context(|| format!("SSH connection to {}:{} failed", ssh.host, ssh.port))?;

    let auth = match ssh.auth {
        SshAuth::Password => {
            let pw = creds.ssh_password.clone().unwrap_or_default();
            handle.authenticate_password(ssh.user.clone(), pw).await?
        }
        SshAuth::PrivateKey => {
            let path = expand_tilde(&ssh.key_path);
            let key = russh::keys::load_secret_key(&path, creds.ssh_passphrase.as_deref())
                .with_context(|| format!("loading SSH key {}", path.display()))?;
            let hash = handle.best_supported_rsa_hash().await?.flatten();
            handle.authenticate_publickey(ssh.user.clone(), PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await?
        }
    };
    if !auth.success() {
        bail!("SSH authentication failed for {}@{}", ssh.user, ssh.host);
    }

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;
    let handle = Arc::new(handle);
    let target_host = target_host.to_string();
    let accept_task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, peer)) = listener.accept().await else {
                break;
            };
            let handle = handle.clone();
            let target_host = target_host.clone();
            tokio::spawn(async move {
                let channel = handle
                    .channel_open_direct_tcpip(
                        target_host.clone(),
                        u32::from(target_port),
                        peer.ip().to_string(),
                        u32::from(peer.port()),
                    )
                    .await;
                match channel {
                    Ok(channel) => {
                        let mut stream = channel.into_stream();
                        let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                    }
                    Err(e) => {
                        tracing::warn!("SSH forward to {target_host}:{target_port} failed: {e}")
                    }
                }
            });
        }
    });
    Ok(SshTunnel { local_addr, accept_task })
}
