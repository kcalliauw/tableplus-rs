#!/usr/bin/env bash
# Creates a config directory with saved connections to the docker test databases
# (docker/compose.yml) and a seeded SQLite file. Usage:
#   scripts/demo-home.sh /tmp/tp-home && TABLEPLUS_RS_HOME=/tmp/tp-home cargo run --release
set -euo pipefail
HOME_DIR="${1:-/tmp/tableplus-demo}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$HOME_DIR"
SQLITE="$ROOT/testdata/shop.sqlite"
if [ ! -f "$SQLITE" ]; then
  cargo run -q --release -p tableplus-core --example make_sqlite -- "$SQLITE"
fi
KEY="$ROOT/docker/sshd/test_key"
cat > "$HOME_DIR/connections.json" <<JSON
{
  "connections": [
    { "id": "00000000-0000-4000-8000-000000000001", "name": "Shop (Postgres)", "driver": "Postgres", "color": "Green",
      "env": "Local", "group": "", "host": "127.0.0.1", "port": 54329, "user": "tp", "database": "shop",
      "save_password": true, "tls": { "mode": "Prefer" } },
    { "id": "00000000-0000-4000-8000-000000000002", "name": "Shop (MySQL 8)", "driver": "MySql", "color": "Blue",
      "env": "Development", "host": "127.0.0.1", "port": 33069, "user": "tp", "database": "shop",
      "save_password": true, "tls": { "mode": "Require" } },
    { "id": "00000000-0000-4000-8000-000000000003", "name": "Billing (MariaDB)", "driver": "MariaDb", "color": "Orange",
      "env": "Staging", "host": "127.0.0.1", "port": 33070, "user": "tp", "database": "shop",
      "save_password": true, "tls": { "mode": "Disable" } },
    { "id": "00000000-0000-4000-8000-000000000004", "name": "Shop (SQLite)", "driver": "Sqlite", "color": "Purple",
      "env": "Local", "path": "$SQLITE" },
    { "id": "00000000-0000-4000-8000-000000000005", "name": "Prod replica (via SSH)", "driver": "Postgres", "color": "Red",
      "env": "Production", "host": "postgres", "port": 5432, "user": "tp", "database": "shop", "save_password": true,
      "tls": { "mode": "Require" },
      "ssh": { "enabled": true, "host": "127.0.0.1", "port": 22229, "user": "tunnel", "auth": "PrivateKey", "key_path": "$KEY" } }
  ]
}
JSON
cat > "$HOME_DIR/secrets.json" <<JSON
{
  "00000000-0000-4000-8000-000000000001:password": "tp_pass",
  "00000000-0000-4000-8000-000000000002:password": "tp_pass",
  "00000000-0000-4000-8000-000000000003:password": "tp_pass",
  "00000000-0000-4000-8000-000000000005:password": "tp_pass"
}
JSON
chmod 600 "$HOME_DIR/secrets.json"
echo "$HOME_DIR"
