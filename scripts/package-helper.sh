#!/usr/bin/env bash
# Packages a built helper for a release: a gzip-compressed copy, and the engine list entry
# (crates/core/src/engines.rs) that lets the app install and run exactly this binary.
#
#   scripts/package-helper.sh target/release/tableplus-duckdb macos-aarch64 dist
#
# TAG names the release the file will be attached to; GITHUB_REPOSITORY the repository.
set -euo pipefail
BIN="$1"
PLATFORM="$2"
OUT="$3"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(sed -n '/pub static DUCKDB/,/};/s/^ *version: "\(.*\)",/\1/p' "$ROOT/crates/core/src/engines.rs")"
TAG="${TAG:?set TAG to the release tag}"
REPO="${GITHUB_REPOSITORY:-kcalliauw/tableplus-rs}"
NAME="$(basename "$BIN")-$VERSION-$PLATFORM.gz"
mkdir -p "$OUT"
gzip -9 -n -c "$BIN" > "$OUT/$NAME"
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }
cat > "$OUT/$PLATFORM.entry" <<ENTRY
HelperRelease {
    platform: "$PLATFORM",
    url: "https://github.com/$REPO/releases/download/$TAG/$NAME",
    download_size: $(wc -c < "$OUT/$NAME" | tr -d ' '),
    size: $(wc -c < "$BIN" | tr -d ' '),
    sha256: "$(sha256 "$BIN")",
},
ENTRY
cat "$OUT/$PLATFORM.entry"
