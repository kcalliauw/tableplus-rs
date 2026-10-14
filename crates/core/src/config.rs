//! Saved connection settings.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::dialect::Dialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DriverKind {
    Postgres,
    MySql,
    MariaDb,
    Sqlite,
}

impl DriverKind {
    pub const ALL: [DriverKind; 4] = [DriverKind::Postgres, DriverKind::MySql, DriverKind::MariaDb, DriverKind::Sqlite];

    pub fn label(self) -> &'static str {
        match self {
            DriverKind::Postgres => "PostgreSQL",
            DriverKind::MySql => "MySQL",
            DriverKind::MariaDb => "MariaDB",
            DriverKind::Sqlite => "SQLite",
        }
    }

    /// Two-letter badge shown in the connection list.
    pub fn badge(self) -> &'static str {
        match self {
            DriverKind::Postgres => "Pg",
            DriverKind::MySql => "My",
            DriverKind::MariaDb => "Ma",
            DriverKind::Sqlite => "Sl",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            DriverKind::Postgres => 5432,
            DriverKind::MySql | DriverKind::MariaDb => 3306,
            DriverKind::Sqlite => 0,
        }
    }

    pub fn default_user(self) -> &'static str {
        match self {
            DriverKind::Postgres => "postgres",
            DriverKind::MySql | DriverKind::MariaDb => "root",
            DriverKind::Sqlite => "",
        }
    }

    pub fn dialect(self) -> Dialect {
        match self {
            DriverKind::Postgres => Dialect::Postgres,
            DriverKind::MySql | DriverKind::MariaDb => Dialect::MySql,
            DriverKind::Sqlite => Dialect::Sqlite,
        }
    }

    pub fn is_file_based(self) -> bool {
        self == DriverKind::Sqlite
    }
}

/// The colored dot/stripe that identifies a connection (TablePlus-style environment tags).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ColorTag {
    #[default]
    None,
    Gray,
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
}

impl ColorTag {
    pub const ALL: [ColorTag; 8] = [
        ColorTag::None,
        ColorTag::Gray,
        ColorTag::Red,
        ColorTag::Orange,
        ColorTag::Yellow,
        ColorTag::Green,
        ColorTag::Blue,
        ColorTag::Purple,
    ];

    pub fn rgb(self) -> Option<[u8; 3]> {
        match self {
            ColorTag::None => None,
            ColorTag::Gray => Some([142, 142, 147]),
            ColorTag::Red => Some([255, 69, 58]),
            ColorTag::Orange => Some([255, 159, 10]),
            ColorTag::Yellow => Some([255, 214, 10]),
            ColorTag::Green => Some([48, 209, 88]),
            ColorTag::Blue => Some([10, 132, 255]),
            ColorTag::Purple => Some([191, 90, 242]),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ColorTag::None => "None",
            ColorTag::Gray => "Gray",
            ColorTag::Red => "Red",
            ColorTag::Orange => "Orange",
            ColorTag::Yellow => "Yellow",
            ColorTag::Green => "Green",
            ColorTag::Blue => "Blue",
            ColorTag::Purple => "Purple",
        }
    }
}

/// Environment label shown next to the connection name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum EnvTag {
    #[default]
    Local,
    Development,
    Testing,
    Staging,
    Production,
}

impl EnvTag {
    pub const ALL: [EnvTag; 5] =
        [EnvTag::Local, EnvTag::Development, EnvTag::Testing, EnvTag::Staging, EnvTag::Production];

    pub fn label(self) -> &'static str {
        match self {
            EnvTag::Local => "local",
            EnvTag::Development => "development",
            EnvTag::Testing => "testing",
            EnvTag::Staging => "staging",
            EnvTag::Production => "production",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum SslMode {
    Disable,
    /// Try TLS, fall back to plaintext.
    #[default]
    Prefer,
    /// TLS without certificate verification.
    Require,
    /// TLS, verify the certificate chain but not the host name.
    VerifyCa,
    /// TLS with full verification.
    VerifyFull,
}

impl SslMode {
    pub const ALL: [SslMode; 5] =
        [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull];

    pub fn label(self) -> &'static str {
        match self {
            SslMode::Disable => "Disable",
            SslMode::Prefer => "Prefer",
            SslMode::Require => "Require",
            SslMode::VerifyCa => "Verify CA",
            SslMode::VerifyFull => "Verify Full",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TlsConfig {
    pub mode: SslMode,
    /// PEM CA certificate used for the verify modes.
    pub ca_cert: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SshAuth {
    #[default]
    Password,
    PrivateKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    /// Path to an OpenSSH private key (for `SshAuth::PrivateKey`).
    pub key_path: String,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: SshAuth::Password,
            key_path: String::new(),
        }
    }
}

/// A saved connection. Secrets are never stored here — see [`crate::secrets`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionConfig {
    pub id: Uuid,
    pub name: String,
    pub driver: DriverKind,
    pub color: ColorTag,
    pub env: EnvTag,
    pub group: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
    /// SQLite database file.
    pub path: String,
    pub save_password: bool,
    pub tls: TlsConfig,
    pub ssh: SshConfig,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self::new(DriverKind::Postgres)
    }
}

impl ConnectionConfig {
    pub fn new(driver: DriverKind) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: String::new(),
            driver,
            color: ColorTag::None,
            env: EnvTag::Local,
            group: String::new(),
            host: if driver.is_file_based() { String::new() } else { "127.0.0.1".into() },
            port: driver.default_port(),
            user: driver.default_user().into(),
            database: String::new(),
            path: String::new(),
            save_password: true,
            tls: TlsConfig::default(),
            ssh: SshConfig::default(),
        }
    }

    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.clone();
        }
        if self.driver.is_file_based() {
            return std::path::Path::new(&self.path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "SQLite".into());
        }
        format!("{}@{}", self.user, self.host)
    }

    /// `PostgreSQL · tp@127.0.0.1:5432/shop` style subtitle.
    pub fn summary(&self) -> String {
        if self.driver.is_file_based() {
            return format!("{} · {}", self.driver.label(), self.path);
        }
        let db = if self.database.is_empty() { String::new() } else { format!("/{}", self.database) };
        let ssh = if self.ssh.enabled { format!(" via SSH {}", self.ssh.host) } else { String::new() };
        format!("{} · {}@{}:{}{db}{ssh}", self.driver.label(), self.user, self.host, self.port)
    }
}

/// Secrets supplied at connect time (resolved from the keychain or prompted).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub password: Option<String>,
    pub ssh_password: Option<String>,
    pub ssh_passphrase: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trip_and_defaults() {
        let mut c = ConnectionConfig::new(DriverKind::MySql);
        c.name = "local".into();
        c.ssh.enabled = true;
        let json = serde_json::to_string(&c).unwrap();
        let back: ConnectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
        // Older/partial files still load.
        let partial: ConnectionConfig =
            serde_json::from_str(r#"{"name":"x","driver":"Sqlite","path":"/tmp/a.db"}"#).unwrap();
        assert_eq!(partial.driver, DriverKind::Sqlite);
        assert_eq!(partial.display_name(), "x");
        assert_eq!(partial.tls.mode, SslMode::Prefer);
        assert!(!json.contains("password\":\""), "no secrets in config");
    }
}
