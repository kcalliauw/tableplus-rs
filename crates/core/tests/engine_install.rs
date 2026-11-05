//! Installing a helper engine from a release, against a local HTTP server, into a throwaway
//! `TABLEPLUS_RS_HOME`.

use std::io::Write;

use sha2::{Digest, Sha256};
use tableplus_core::engines::{self, Availability, HelperEngine, HelperRelease};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(bytes).unwrap();
    gz.finish().unwrap()
}

/// Serves `body` to every request on a local port, and returns the port.
async fn serve(body: Vec<u8>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let body = body.clone();
            tokio::spawn(async move {
                let mut request = [0u8; 2048];
                let _ = socket.read(&mut request).await;
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });
    port
}

fn leak<T>(value: T) -> &'static T {
    Box::leak(Box::new(value))
}

#[tokio::test(flavor = "multi_thread")]
async fn installs_only_the_released_helper() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary, and set before anything reads it.
    unsafe { std::env::set_var("TABLEPLUS_RS_HOME", home.path()) };

    let helper = b"#!/bin/sh\necho pretend helper\n".repeat(1000);
    let sha256: String = Sha256::digest(&helper).iter().map(|b| format!("{b:02x}")).collect();
    let compressed = gzip(&helper);
    let port = serve(compressed.clone()).await;
    let release = |sha: &str| HelperRelease {
        platform: leak(engines::platform()).as_str(),
        url: leak(format!("http://127.0.0.1:{port}/tableplus-duckdb.gz")).as_str(),
        download_size: compressed.len() as u64,
        size: helper.len() as u64,
        sha256: leak(sha.to_string()).as_str(),
    };
    let engine = |releases: &'static [HelperRelease]| -> &'static HelperEngine {
        leak(HelperEngine { env_var: "TABLEPLUS_TEST_UNSET_HELPER", releases, ..engines::DUCKDB })
    };

    let good = engine(leak([release(&sha256)]));
    assert!(matches!(engines::availability(good), Availability::Installable(_)), "nothing installed yet");
    assert!(engines::locate(good).unwrap_err().to_string().contains("isn't installed"));
    let mut seen = Vec::new();
    let path = engines::install(good, &mut |done, total| seen.push((done, total))).await.unwrap();
    assert_eq!(path, home.path().join("engines/duckdb/1.5.6/tableplus-duckdb"));
    assert_eq!(std::fs::read(&path).unwrap(), helper);
    assert_eq!(seen.first(), Some(&(0, Some(compressed.len() as u64))));
    assert_eq!(seen.last(), Some(&(compressed.len() as u64, Some(compressed.len() as u64))));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o755);
    }
    assert_eq!(engines::locate(good).unwrap(), path);
    let listed = engines::installed(good);
    assert_eq!((listed.len(), listed[0].version.as_str(), listed[0].current), (1, "1.5.6", true));

    // A changed binary is never started.
    std::fs::write(&path, b"#!/bin/sh\necho tampered\n".repeat(1000)).unwrap();
    let err = engines::locate(good).unwrap_err().to_string();
    assert!(err.contains("doesn't match the released helper"), "{err}");

    // A download whose hash doesn't match leaves nothing runnable behind.
    engines::remove(good).unwrap();
    let wrong = engine(leak([release(&"0".repeat(64))]));
    let err = format!("{:#}", engines::install(wrong, &mut |_, _| {}).await.unwrap_err());
    assert!(err.contains("not installed") && err.contains("doesn't match"), "{err}");
    let dir = home.path().join("engines/duckdb/1.5.6");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "the staged download is gone");

    // Install from file takes the .gz or the binary itself.
    let file = home.path().join("tableplus-duckdb-macos.gz");
    std::fs::write(&file, &compressed).unwrap();
    engines::install_from_file(good, &file).unwrap();
    assert!(engines::is_available(good));
    let raw = home.path().join("tableplus-duckdb");
    std::fs::write(&raw, &helper).unwrap();
    engines::install_from_file(good, &raw).unwrap();
    engines::remove(good).unwrap();
    assert!(!home.path().join("engines/duckdb").exists());
    assert!(engines::installed(good).is_empty());

    // Without a release for this platform there is nothing to install.
    let unreleased = engine(&[]);
    assert_eq!(engines::availability(unreleased), Availability::Unreleased);
    assert!(engines::locate(unreleased).unwrap_err().to_string().contains("no release"));
}
