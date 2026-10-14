//! SSH tunnel tests through the `sshd` bastion container, which reaches the databases by
//! their compose service names.

mod common;

use common::*;
use tableplus_core::config::{Credentials, SshAuth, SshConfig};
use tableplus_core::driver;

fn bastion(auth: SshAuth) -> SshConfig {
    let key = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docker/sshd/test_key");
    SshConfig {
        enabled: true,
        host: env_or("TP_SSH_HOST", "127.0.0.1"),
        port: env_or("TP_SSH_PORT", "22229").parse().unwrap(),
        user: "tunnel".into(),
        auth,
        key_path: key.into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_over_ssh_password() {
    let mut cfg = pg_config();
    cfg.host = "postgres".into(); // resolved by the bastion
    cfg.port = 5432;
    cfg.ssh = bastion(SshAuth::Password);
    let creds = Credentials {
        password: Some("tp_pass".into()),
        ssh_password: Some("tunnel_pass".into()),
        ..Default::default()
    };
    let Some(conn) = connect_or_skip(&cfg, &creds).await else {
        return;
    };
    assert_eq!(driver::scalar(conn.as_ref(), "SELECT count(*) FROM users").await.unwrap().as_deref(), Some("10000"));
    // Several queries reuse the same tunnel.
    for _ in 0..3 {
        conn.ping().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_over_ssh_key() {
    let mut cfg = mysql_config();
    cfg.host = "mysql".into();
    cfg.port = 3306;
    cfg.ssh = bastion(SshAuth::PrivateKey);
    let Some(conn) = connect_or_skip(&cfg, &password()).await else {
        return;
    };
    assert_eq!(driver::scalar(conn.as_ref(), "SELECT COUNT(*) FROM users").await.unwrap().as_deref(), Some("10000"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ssh_bad_password_is_reported() {
    let mut cfg = pg_config();
    cfg.host = "postgres".into();
    cfg.port = 5432;
    cfg.ssh = bastion(SshAuth::Password);
    let creds = Credentials { ssh_password: Some("wrong".into()), ..Default::default() };
    match driver::connect(&cfg, &creds).await {
        Ok(_) => panic!("connected with a wrong SSH password"),
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("SSH connection to") && std::env::var_os("TABLEPLUS_REQUIRE_DB").is_none() {
                eprintln!("SKIPPED: bastion not running ({msg})");
                return;
            }
            assert!(msg.contains("SSH authentication failed"), "{msg}");
        }
    }
}
