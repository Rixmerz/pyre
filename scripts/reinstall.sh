#!/usr/bin/env bash
#
# reinstall.sh — rebuild + install the pyre binaries and verify provenance.
#
#   1. Kill the old pyred daemon (+ workers) and clear stale sockets.
#   2. Rebuild the changed crates (pyred, pyre-tui) in release — CPU-capped
#      (-j) and niced so it never pegs the machine.
#   3. Back up the currently-installed binaries, then install the fresh ones.
#   4. Verify by SHA-256 that each installed binary is byte-identical to the
#      one just built — proof the new binary is the one that got installed.
#
# Run from a terminal that is NOT inside a pyre session.
#   ./scripts/reinstall.sh          # default JOBS=6
#   JOBS=4 ./scripts/reinstall.sh   # lower build parallelism
#
set -euo pipefail

JOBS="${JOBS:-6}"
STAMP="$(date +%Y%m%d-%H%M%S)"
CARGO_BIN="${HOME}/.cargo/bin"
LOCAL_BIN="${HOME}/.local/bin"
RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

# built-name -> install destination(s). `pyre` is symlinked into ~/.local/bin,
# so only ~/.cargo/bin/pyre needs updating; `pyred` is a real copy in both.
declare -A DEST=(
  [pyre]="${CARGO_BIN}/pyre"
  [pyred]="${CARGO_BIN}/pyred ${LOCAL_BIN}/pyred"
)

log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
err() { printf '\033[1;31mERR\033[0m %s\n' "$*" >&2; }
trap 'err "failed at line ${LINENO}"; exit 1' ERR

cd "$(git rev-parse --show-toplevel)"
log "repo $(pwd) | branch $(git rev-parse --abbrev-ref HEAD) | commit $(git rev-parse --short HEAD)"

# ── 1. kill old daemon ──────────────────────────────────────────────────────
log "killing old pyred (exact name — will not match this script) …"
pkill -9 -x pyred 2>/dev/null || true
sleep 0.3
rm -f "${RUNTIME_DIR}/pyre/"*.sock "${RUNTIME_DIR}/pyre.sock" 2>/dev/null || true
log "pyred still running: $(pgrep -x pyred | wc -l | tr -d ' ')"
if pgrep -x pyre >/dev/null 2>&1; then
  log "note: a 'pyre' TUI is still open — close it, then relaunch after this script."
fi

# ── 2. build (capped + niced) ───────────────────────────────────────────────
log "building pyred + pyre-tui (release, -j ${JOBS}, niced) — heavy step …"
nice -n 10 cargo build --release -j "${JOBS}" -p pyred -p pyre-tui

# ── 3. install (backup, then copy) ──────────────────────────────────────────
for bin in "${!DEST[@]}"; do
  src="target/release/${bin}"
  [ -f "${src}" ] || { err "built binary missing: ${src}"; exit 1; }
  for dst in ${DEST[$bin]}; do
    if [ -f "${dst}" ]; then
      cp -f "${dst}" "${dst}.bak.${STAMP}"
      log "backup ${dst}.bak.${STAMP}"
    fi
    install -m 0755 "${src}" "${dst}"
    log "installed ${src} -> ${dst}"
  done
done

# ── 4. verify provenance (installed == built) ───────────────────────────────
log "verifying installed binaries match the freshly built ones (sha-256) …"
fail=0
for bin in "${!DEST[@]}"; do
  src_sum="$(sha256sum "target/release/${bin}" | awk '{print $1}')"
  for dst in ${DEST[$bin]}; do
    dst_sum="$(sha256sum "${dst}" | awk '{print $1}')"
    if [ "${src_sum}" = "${dst_sum}" ]; then
      printf '  \033[1;32mOK \033[0m %-30s %s…\n' "${dst}" "${src_sum:0:16}"
    else
      printf '  \033[1;31mBAD\033[0m %-30s built=%s… installed=%s…\n' "${dst}" "${src_sum:0:16}" "${dst_sum:0:16}"
      fail=1
    fi
  done
done
[ "${fail}" -eq 0 ] || { err "provenance FAILED — installed binary is NOT the one just built"; exit 1; }

# non-fatal version banner (pyre may not implement --version)
"${CARGO_BIN}/pyred" --version 2>/dev/null || true
"${CARGO_BIN}/pyre"  --version 2>/dev/null || true

log "done — new binaries installed & verified. Launch with: pyre"
