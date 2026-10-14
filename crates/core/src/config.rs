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

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl ConnectionConfig {
    /// Parses `postgres://user:pass@host:5432/db?sslmode=require`, `mysql://…`, `mariadb://…`
    /// or `sqlite:///path/to/file.db`. Returns the config and the password, if any.
    pub fn from_url(url: &str) -> Result<(ConnectionConfig, Option<String>), String> {
        let (scheme, rest) = url.split_once("://").ok_or("URL must look like scheme://user@host/db")?;
        let driver = match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" => DriverKind::Postgres,
            "mysql" => DriverKind::MySql,
            "mariadb" => DriverKind::MariaDb,
            "sqlite" | "sqlite3" | "file" => DriverKind::Sqlite,
            other => return Err(format!("Unsupported scheme \"{other}\"")),
        };
        let mut c = ConnectionConfig::new(driver);
        if driver == DriverKind::Sqlite {
            c.path = percent_decode(rest.split('?').next().unwrap_or_default());
            return Ok((c, None));
        }
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        let mut password = None;
        if let Some(u) = userinfo {
            let (user, pw) = u.split_once(':').unwrap_or((u, ""));
            c.user = percent_decode(user);
            if !pw.is_empty() {
                password = Some(percent_decode(pw));
            }
        }
        let (host, port) = if let Some(stripped) = hostport.strip_prefix('[') {
            let (h, after) = stripped.split_once(']').ok_or("Unterminated IPv6 address")?;
            (h.to_string(), after.strip_prefix(':'))
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p)),
                None => (hostport.to_string(), None),
            }
        };
        if host.is_empty() {
            return Err("Host is missing".into());
        }
        c.host = host;
        if let Some(p) = port.filter(|p| !p.is_empty()) {
            c.port = p.parse().map_err(|_| format!("Invalid port \"{p}\""))?;
        }
        c.database = percent_decode(path.trim_end_matches('/'));
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if k.eq_ignore_ascii_case("sslmode") || k.eq_ignore_ascii_case("ssl-mode") {
                c.tls.mode = match v.to_ascii_lowercase().replace('_', "-").as_str() {
                    "disable" | "disabled" => SslMode::Disable,
                    "require" | "required" => SslMode::Require,
                    "verify-ca" => SslMode::VerifyCa,
                    "verify-full" | "verify-identity" => SslMode::VerifyFull,
                    _ => SslMode::Prefer,
                };
            }
        }
        Ok((c, password))
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
    fn urls() {
        let (c, pw) =
            ConnectionConfig::from_url("postgres://tp:p%40ss@db.example.com:6543/shop?sslmode=require").unwrap();
        assert_eq!(c.driver, DriverKind::Postgres);
        assert_eq!(
            (c.user.as_str(), c.host.as_str(), c.port, c.database.as_str()),
            ("tp", "db.example.com", 6543, "shop")
        );
        assert_eq!(pw.as_deref(), Some("p@ss"));
        assert_eq!(c.tls.mode, SslMode::Require);
        let (c, pw) = ConnectionConfig::from_url("mysql://root@127.0.0.1/app").unwrap();
        assert_eq!((c.driver, c.port, pw), (DriverKind::MySql, 3306, None));
        let (c, _) = ConnectionConfig::from_url("mariadb://u:x@[::1]:3307").unwrap();
        assert_eq!((c.host.as_str(), c.port, c.database.as_str()), ("::1", 3307, ""));
        let (c, _) = ConnectionConfig::from_url("sqlite:///tmp/my%20db.sqlite").unwrap();
        assert_eq!(c.path, "/tmp/my db.sqlite");
        assert!(ConnectionConfig::from_url("redis://x").is_err());
        assert!(ConnectionConfig::from_url("nonsense").is_err());
    }

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
