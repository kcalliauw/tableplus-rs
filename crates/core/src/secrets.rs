//! Password storage.
//!
//! On macOS secrets live in the login Keychain (via the `keyring` crate). Other platforms,
//! where a Secret Service daemon is often unavailable (headless servers, CI), use a JSON
//! file readable only by the current user.

use std::path::{Path, PathBuf};

use anyhow::Result;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    Password,
    SshPassword,
    SshPassphrase,
}

impl SecretKind {
    fn suffix(self) -> &'static str {
        match self {
            SecretKind::Password => "password",
            SecretKind::SshPassword => "ssh-password",
            SecretKind::SshPassphrase => "ssh-passphrase",
        }
    }
}

pub fn secret_key(connection: Uuid, kind: SecretKind) -> String {
    format!("{connection}:{}", kind.suffix())
}

#[derive(Debug, Clone)]
pub struct SecretStore {
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    file: PathBuf,
}

impl SecretStore {
    pub fn new(config_dir: &Path) -> Self {
        Self { file: config_dir.join("secrets.json") }
    }

    pub fn backend_name(&self) -> &'static str {
        if cfg!(target_os = "macos") { "macOS Keychain" } else { "local secrets file (0600)" }
    }

    pub fn get_for(&self, id: Uuid, kind: SecretKind) -> Option<String> {
        self.get(&secret_key(id, kind))
    }

    pub fn set_for(&self, id: Uuid, kind: SecretKind, value: &str) -> Result<()> {
        if value.is_empty() {
            return self.delete(&secret_key(id, kind));
        }
        self.set(&secret_key(id, kind), value)
    }

    pub fn delete_all(&self, id: Uuid) -> Result<()> {
        for kind in [SecretKind::Password, SecretKind::SshPassword, SecretKind::SshPassphrase] {
            self.delete(&secret_key(id, kind))?;
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod backend {
    use super::*;

    const SERVICE: &str = "tableplus-rs";

    impl SecretStore {
        pub fn get(&self, key: &str) -> Option<String> {
            keyring::Entry::new(SERVICE, key).ok()?.get_password().ok()
        }

        pub fn set(&self, key: &str, value: &str) -> Result<()> {
            keyring::Entry::new(SERVICE, key)?.set_password(value)?;
            Ok(())
        }

        pub fn delete(&self, key: &str) -> Result<()> {
            match keyring::Entry::new(SERVICE, key)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.into()),
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod backend {
    use std::collections::BTreeMap;

    use super::*;

    impl SecretStore {
        fn load(&self) -> BTreeMap<String, String> {
            std::fs::read(&self.file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
        }

        fn save(&self, map: &BTreeMap<String, String>) -> Result<()> {
            if let Some(dir) = self.file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.file.with_extension("json.tmp");
            {
                use std::io::Write;
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create(true).truncate(true);
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
                let mut f = opts.open(&tmp)?;
                f.write_all(&serde_json::to_vec_pretty(map)?)?;
            }
            std::fs::rename(&tmp, &self.file)?;
            Ok(())
        }

        pub fn get(&self, key: &str) -> Option<String> {
            self.load().remove(key)
        }

        pub fn set(&self, key: &str, value: &str) -> Result<()> {
            let mut map = self.load();
            map.insert(key.to_string(), value.to_string());
            self.save(&map)
        }

        pub fn delete(&self, key: &str) -> Result<()> {
            let mut map = self.load();
            if map.remove(key).is_some() {
                self.save(&map)?;
            }
            Ok(())
        }
    }
}

#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn file_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(dir.path());
        let id = Uuid::new_v4();
        assert_eq!(store.get_for(id, SecretKind::Password), None);
        store.set_for(id, SecretKind::Password, "s3cret").unwrap();
        store.set_for(id, SecretKind::SshPassword, "tunnel").unwrap();
        assert_eq!(store.get_for(id, SecretKind::Password).as_deref(), Some("s3cret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("secrets.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.set_for(id, SecretKind::Password, "").unwrap();
        assert_eq!(store.get_for(id, SecretKind::Password), None);
        store.delete_all(id).unwrap();
        assert_eq!(store.get_for(id, SecretKind::SshPassword), None);
    }
}
