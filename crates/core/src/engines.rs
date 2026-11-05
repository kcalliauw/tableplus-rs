//! Engines that run in a separate helper program instead of inside the app: where each helper is
//! released, how it is installed, and how the app finds it.
//!
//! Nothing downloads until the user asks for an install. The app runs only helpers released with
//! it: the installed binary's SHA-256 must match the release listed here, after the download and
//! again before every start.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::config::ConnectionConfig;
use crate::duckdb::DuckDbTarget;

/// One build of a helper for one platform, as the release publishes it: a gzip-compressed binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelperRelease {
    /// [`platform()`] of the machines it runs on, such as `macos-aarch64`.
    pub platform: &'static str,
    pub url: &'static str,
    /// Bytes of the compressed download.
    pub download_size: u64,
    /// Bytes of the installed binary.
    pub size: u64,
    /// SHA-256 of the installed binary, in lowercase hex.
    pub sha256: &'static str,
}

pub struct HelperEngine {
    /// Identifies the engine in the handshake, for example `duckdb`.
    pub name: &'static str,
    /// For people, for example `DuckDB`.
    pub label: &'static str,
    /// The helper's file name.
    pub binary: &'static str,
    /// Overrides where the helper is, for development and tests. The helper it names is trusted
    /// without a hash check.
    pub env_var: &'static str,
    /// The engine version the helper was released with; installs go into a folder named after it.
    pub version: &'static str,
    /// The installed helper's size, shown before it is installed.
    pub size_label: &'static str,
    pub releases: &'static [HelperRelease],
    /// The file a connection holds open, with its access mode, when sessions on it must share
    /// one helper process. `None` gives the connection a helper of its own.
    pub shared_file: fn(&ConnectionConfig) -> Option<(PathBuf, bool)>,
}

/// The fork's GitHub releases host the helper for now (`docs/duckdb-research.md`, section 8). A
/// release is built by `.github/workflows/duckdb-helper.yml`, which prints the entries to paste
/// here.
pub static DUCKDB: HelperEngine = HelperEngine {
    name: "duckdb",
    label: "DuckDB",
    binary: "tableplus-duckdb",
    env_var: "TABLEPLUS_DUCKDB_HELPER",
    version: "1.5.6",
    size_label: "36 MB",
    releases: &[
        HelperRelease {
            platform: "linux-x86_64",
            url: "https://github.com/kcalliauw/tableplus-rs/releases/download/duckdb-helper-1.5.6-1/tableplus-duckdb-1.5.6-linux-x86_64.gz",
            download_size: 15199501,
            size: 41922792,
            sha256: "a78292de1f3a1a9e026eeebacfa9e64cd0e386843a8d6c4195e7c3e03e458f31",
        },
        HelperRelease {
            platform: "macos-aarch64",
            url: "https://github.com/kcalliauw/tableplus-rs/releases/download/duckdb-helper-1.5.6-1/tableplus-duckdb-1.5.6-macos-aarch64.gz",
            download_size: 11968958,
            size: 35747552,
            sha256: "d8359ebfa4c994e6bf019079d4d1b3f0bfc6b44ac6b6a68e3719867312d44122",
        },
    ],
    shared_file: duckdb_shared_file,
};

/// Every engine that runs in a helper.
pub static ALL: [&HelperEngine; 1] = [&DUCKDB];

fn duckdb_shared_file(config: &ConnectionConfig) -> Option<(PathBuf, bool)> {
    match DuckDbTarget::of(config).ok()? {
        DuckDbTarget::Database(path) => {
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            Some((path, config.read_only))
        }
        DuckDbTarget::Memory | DuckDbTarget::DataFile { .. } => None,
    }
}

/// This machine's operating system and processor, as release entries name them.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

impl HelperEngine {
    /// The release built for this machine, if there is one.
    pub fn release(&self) -> Option<&'static HelperRelease> {
        if cfg!(debug_assertions)
            && let Some(dev) = self.dev_release()
        {
            return Some(dev);
        }
        let here = platform();
        self.releases.iter().find(|r| r.platform == here)
    }

    /// Debug builds only: a release described in `TABLEPLUS_DEV_DUCKDB_RELEASE` as
    /// `url download_size size sha256`, to try the installer against a locally served package.
    fn dev_release(&self) -> Option<&'static HelperRelease> {
        let var = format!("TABLEPLUS_DEV_{}_RELEASE", self.name.to_ascii_uppercase());
        let text = std::env::var(var).ok()?;
        let parts: Vec<&str> = text.split_whitespace().collect();
        let [url, download_size, size, sha256] = parts.as_slice() else { return None };
        let leak = |s: &str| -> &'static str { Box::leak(s.to_string().into_boxed_str()) };
        Some(Box::leak(Box::new(HelperRelease {
            platform: leak(&platform()),
            url: leak(url),
            download_size: download_size.parse().ok()?,
            size: size.parse().ok()?,
            sha256: leak(sha256),
        })))
    }

    /// The folder that holds every installed version of this engine.
    pub fn root_dir(&self) -> PathBuf {
        crate::store::config_dir().join("engines").join(self.name)
    }

    /// Where this app's version of the helper is installed.
    pub fn install_dir(&self) -> PathBuf {
        self.root_dir().join(self.version)
    }

    pub fn installed_path(&self) -> PathBuf {
        self.install_dir().join(self.binary)
    }
}

/// What the app can run for an engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// A helper the app may start.
    Ready(PathBuf),
    /// Nothing usable is installed; an install of this release fixes that.
    Installable(&'static HelperRelease),
    /// Nothing usable is installed, and no release exists for this platform.
    Unreleased,
}

impl Availability {
    pub fn is_ready(&self) -> bool {
        matches!(self, Availability::Ready(_))
    }
}

/// Finds the helper: the path in its environment variable, then in debug builds a binary next to
/// the app's own executable (where `cargo build` puts it), then the installed copy if its hash
/// matches this app's release.
pub fn availability(engine: &HelperEngine) -> Availability {
    if let Some(path) = std::env::var_os(engine.env_var) {
        return Availability::Ready(PathBuf::from(path));
    }
    if cfg!(debug_assertions)
        && let Some(path) = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join(engine.binary)))
        && path.is_file()
    {
        return Availability::Ready(path);
    }
    match engine.release() {
        Some(release) if verify(&engine.installed_path(), release).is_ok() => {
            Availability::Ready(engine.installed_path())
        }
        Some(release) => Availability::Installable(release),
        None => Availability::Unreleased,
    }
}

/// The helper to start, after checking it is the one released with this app.
pub fn locate(engine: &HelperEngine) -> Result<PathBuf> {
    match availability(engine) {
        Availability::Ready(path) => Ok(path),
        Availability::Installable(release) => {
            let installed = engine.installed_path();
            if installed.exists() {
                verify(&installed, release)?;
            }
            bail!("{} support isn't installed.", engine.label)
        }
        Availability::Unreleased => {
            bail!("{} support isn't installed, and there is no release of it for {} yet.", engine.label, platform())
        }
    }
}

/// Whether the app can start the engine's helper.
pub fn is_available(engine: &HelperEngine) -> bool {
    availability(engine).is_ready()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Checks the file at `path` against the release: its size, then its SHA-256.
pub fn verify(path: &Path, release: &HelperRelease) -> Result<()> {
    let mut file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let size = file.metadata()?.len();
    if size != release.size {
        bail!("{} doesn't match the released helper ({size} bytes, the release has {})", path.display(), release.size);
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hex(&hasher.finalize());
    if digest != release.sha256 {
        bail!("{} doesn't match the released helper (SHA-256 {digest})", path.display());
    }
    Ok(())
}

/// Bytes received so far and the total the server announced.
pub type Progress<'a> = dyn FnMut(u64, Option<u64>) + Send + 'a;

/// Downloads this platform's release and installs it, reporting progress as bytes arrive.
pub async fn install(engine: &HelperEngine, progress: &mut Progress<'_>) -> Result<PathBuf> {
    let release =
        engine.release().ok_or_else(|| anyhow!("There is no {} release for {}.", engine.label, platform()))?;
    install_release(engine, release, progress).await
}

/// Downloads `release` and installs it as `engine`'s helper.
pub async fn install_release(
    engine: &HelperEngine,
    release: &HelperRelease,
    progress: &mut Progress<'_>,
) -> Result<PathBuf> {
    let client = reqwest::Client::builder().build()?;
    let mut response = client
        .get(release.url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("downloading {}", release.url))?;
    let total = response.content_length().or(Some(release.download_size));
    let mut compressed = Vec::with_capacity(release.download_size as usize);
    progress(0, total);
    while let Some(chunk) = response.chunk().await.with_context(|| format!("downloading {}", release.url))? {
        compressed.extend_from_slice(&chunk);
        if compressed.len() as u64 > release.download_size.saturating_mul(2) {
            bail!("{} is larger than the release says", release.url);
        }
        progress(compressed.len() as u64, total);
    }
    let engine_dir = engine.install_dir();
    let binary = engine.binary;
    let release = *release;
    tokio::task::spawn_blocking(move || put_in_place(&engine_dir, binary, &release, &compressed)).await?
}

/// Installs a release file the user picked, for machines without internet: the downloaded `.gz`
/// or the binary inside it.
pub fn install_from_file(engine: &HelperEngine, file: &Path) -> Result<PathBuf> {
    let release =
        engine.release().ok_or_else(|| anyhow!("There is no {} release for {}.", engine.label, platform()))?;
    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    put_in_place(&engine.install_dir(), engine.binary, release, &bytes)
}

/// Decompresses `bytes` when they are gzip, checks them against `release`, and moves them into
/// `dir` only once they match, so a failed install leaves nothing runnable behind.
fn put_in_place(dir: &Path, binary: &str, release: &HelperRelease, bytes: &[u8]) -> Result<PathBuf> {
    let mut contents = Vec::new();
    if bytes.starts_with(&[0x1f, 0x8b]) {
        flate2::read::GzDecoder::new(bytes)
            .take(release.size + 1)
            .read_to_end(&mut contents)
            .context("decompressing the helper")?;
    } else {
        contents = bytes.to_vec();
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let staged = dir.join(format!(".{binary}.download"));
    {
        let mut file = std::fs::File::create(&staged)?;
        file.write_all(&contents)?;
        file.sync_all()?;
    }
    if let Err(e) = verify(&staged, release) {
        let _ = std::fs::remove_file(&staged);
        return Err(e.context("The helper was not installed"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    let target = dir.join(binary);
    std::fs::rename(&staged, &target)?;
    Ok(target)
}

/// An installed version of an engine, as Settings lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    pub path: PathBuf,
    pub size: u64,
    /// The version this app was released with, checked against its hash.
    pub current: bool,
}

/// Every version of `engine` in the install folder, newest name first.
pub fn installed(engine: &HelperEngine) -> Vec<Installed> {
    let Ok(dirs) = std::fs::read_dir(engine.root_dir()) else { return Vec::new() };
    let mut found: Vec<Installed> = dirs
        .filter_map(|d| d.ok())
        .filter_map(|d| {
            let path = d.path().join(engine.binary);
            let size = std::fs::metadata(&path).ok()?.len();
            let version = d.file_name().to_string_lossy().into_owned();
            let current = version == engine.version && engine.release().is_some_and(|r| verify(&path, r).is_ok());
            Some(Installed { version, path, size, current })
        })
        .collect();
    found.sort_by(|a, b| b.version.cmp(&a.version));
    found
}

/// Removes every installed version of `engine`. Running helpers keep their open file.
pub fn remove(engine: &HelperEngine) -> Result<()> {
    match std::fs::remove_dir_all(engine.root_dir()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", engine.root_dir().display())),
    }
}

/// `12.0 MB`, for download and install sizes.
pub fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}
