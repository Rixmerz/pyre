#!/usr/bin/env bash
# Install pyre binaries on macOS (from a local checkout).
# Usage: ./dist/macos/install.sh [--hybrid] [--start-daemon]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

INSTALL_HYBRID=0
START_DAEMON=0
for arg in "$@"; do
  case "$arg" in
    --hybrid) INSTALL_HYBRID=1 ;;
    --start-daemon) START_DAEMON=1 ;;
    -h|--help)
      echo "Usage: $0 [--hybrid] [--start-daemon]"
      echo "  Installs pyred, pyrec, pyre, pyre-mcp to ~/.cargo/bin via cargo install --force"
      exit 0
      ;;
  esac
done

echo "==> Building release binaries..."
cargo build --release -p pyred -p pyrec -p pyre-tui -p pyre-mcp

echo "==> Installing to ~/.cargo/bin..."
cargo install --path crates/pyred --force
cargo install --path crates/pyrec --force
cargo install --path crates/pyre-tui --bin pyre --force
cargo install --path crates/pyre-mcp --force

CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/Library/Application Support}/pyre"
mkdir -p "$CONFIG_DIR"
if [[ "$INSTALL_HYBRID" -eq 1 ]]; then
  cat >"$CONFIG_DIR/config.toml" <<'EOF'
[pyred]
process_model = "hybrid"
EOF
  echo "==> Wrote $CONFIG_DIR/config.toml (hybrid)"
elif [[ ! -f "$CONFIG_DIR/config.toml" ]]; then
  cat >"$CONFIG_DIR/config.toml" <<'EOF'
[pyred]
process_model = "single"
EOF
  echo "==> Wrote default $CONFIG_DIR/config.toml (single)"
fi

DATA_DIR="${XDG_DATA_HOME:-$HOME/Library/Application Support}/pyre"
mkdir -p "$DATA_DIR"

UID_NUM="$(id -u)"
SOCK="/tmp/pyre-${UID_NUM}.sock"

if [[ "$START_DAEMON" -eq 1 ]]; then
  echo "==> Restarting pyred..."
  pkill -x pyred 2>/dev/null || true
  sleep 0.3
  rm -f "$SOCK"
  pyred --mode supervisor >>/tmp/pyred.log 2>&1 &
  sleep 0.8
  if [[ -S "$SOCK" ]]; then
    echo "==> pyred listening on $SOCK"
  else
    echo "ERROR: socket $SOCK not found; see /tmp/pyred.log" >&2
    exit 1
  fi
fi

echo ""
echo "Installed:"
command -v pyred pyrec pyre pyre-mcp
echo ""
echo "Run:  pyre          # TUI (+ fire splash)"
echo "      pyrec sessions"
echo "Socket (default): $SOCK"
echo "Config: $CONFIG_DIR/config.toml"
echo "Data:   $DATA_DIR"
