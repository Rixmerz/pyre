# pyre — unified build / install / run task runner.
# Two build systems (cargo workspace + Tauri/pnpm GUI) behind one command.
# `just up` = rebuild BOTH sides, ETXTBSY-safe install, restart daemon, launch GUI.
# Requires `just` (pacman -S just) and a prior `pnpm install` in gui/.

set shell := ["bash", "-euc"]

root := justfile_directory()
bindir := root / "target" / "release"
gui_bin := root / "gui" / "src-tauri" / "target" / "release" / "pyre-gui"
prefix := env_var('HOME') / ".local" / "bin"

# Show available recipes (bare `just`).
default:
    @just --list

# Build BOTH sides: cargo daemon+CLI, then the Tauri GUI — never one without the other (a PROTO bump must rebuild both).
build:
    cargo build --release -p pyred -p pyrec
    cd "{{ root }}/gui" && pnpm tauri build

# ETXTBSY-safe install over possibly-running binaries: copy to .new, then atomic mv.
install:
    mkdir -p "{{ prefix }}"
    cp -f "{{ bindir }}/pyred" "{{ prefix }}/pyred.new" && mv -f "{{ prefix }}/pyred.new" "{{ prefix }}/pyred"
    cp -f "{{ bindir }}/pyrec" "{{ prefix }}/pyrec.new" && mv -f "{{ prefix }}/pyrec.new" "{{ prefix }}/pyrec"
    cp -f "{{ gui_bin }}" "{{ prefix }}/pyre-gui.new" && mv -f "{{ prefix }}/pyre-gui.new" "{{ prefix }}/pyre-gui"

# Stop the daemon (SIGTERM cleans its socket). Sessions persist in SQLite; `-` so a no-match isn't a failure.
daemon-stop:
    -pkill -x pyred

# Start pyred detached (it runs foreground, no self-fork), then wait for its UDS socket to bind (≤5s).
daemon-start:
    #!/usr/bin/env bash
    set -euo pipefail
    sock="${XDG_RUNTIME_DIR:-/tmp}/pyre.sock"
    [ -n "${XDG_RUNTIME_DIR:-}" ] || sock="/tmp/pyre-$(id -u).sock"
    setsid "{{ prefix }}/pyred" >"${XDG_RUNTIME_DIR:-/tmp}/pyred.log" 2>&1 &
    for _ in $(seq 1 50); do
      [ -S "$sock" ] && { echo "pyred ready: $sock"; exit 0; }
      sleep 0.1
    done
    echo "pyred: socket $sock did not appear within 5s (see ${XDG_RUNTIME_DIR:-/tmp}/pyred.log)" >&2
    exit 1

# Full cycle: rebuild both, install, restart daemon, launch GUI detached.
up: build install daemon-stop daemon-start
    setsid "{{ prefix }}/pyre-gui" >"${XDG_RUNTIME_DIR:-/tmp}/pyre-gui.log" 2>&1 &
    @echo "pyre up — GUI launched (log: ${XDG_RUNTIME_DIR:-/tmp}/pyre-gui.log)"

# Frontend-only dev loop (Vite mock at http://127.0.0.1:1420 — no daemon, no rebuild).
mock:
    cd "{{ root }}/gui" && pnpm dev:mock

# Full quality gate: clippy (workspace) + GUI typecheck + GUI tests.
check:
    cargo clippy --workspace --all-targets -- -D warnings
    cd "{{ root }}/gui" && pnpm exec tsc --noEmit && pnpm test
