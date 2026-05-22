# ADR-0004 — Remote attach

**Status:** Proposed
**Date:** 2026-05-21
**Sprint:** M5 (v0.2 UX)

## Context

`pyred` binds a Unix Domain Socket only — `$XDG_RUNTIME_DIR/pyre.sock`,
mode 0700, single-user (ADR-001, ADR-002). The control channel is tarpc
+ bincode with a `PROTO_VERSION=2` handshake after a 1-byte mode tag
(`0x01` control, `0x02` stream). Both `pyre-tui` and `pyrec` resolve
the socket path the same way (`XDG_RUNTIME_DIR/pyre.sock`, falling back
to `/tmp/pyre-$uid.sock`).

The most common agent-fleet workflow is **laptop ↔ remote server**:
agents run on a beefy box, the user attaches from a thin client.
Herdr advertises "local or remote attach" but their published docs do
not specify a wire format — observable behaviour is consistent with
SSH-tunneled UDS rather than a native TCP listener (no documented
port, no TLS material in their config schema). pyre v0.1 ships
local-only; M5 of the v0.2 UX sprint must close the gap.

This ADR picks the v0.2 mechanism and stakes out where v0.3 may go.
No Rust changes are in scope for this ADR — it documents the
decision before any implementation lands.

## Options

### A — SSH tunnel UDS (recommended for v0.2)

User runs `ssh -L $XDG_RUNTIME_DIR/pyre.sock:/run/user/1000/pyre.sock
user@host` (or `-R` reversed). Local `pyre --socket <path>` already
accepts an override (see `pyre-tui::default_socket` and the `--socket`
flag), so this is **zero pyred changes** — pure docs + a thin wrapper
script.

- Auth: SSH (keys, agent forwarding, known_hosts). Already solved.
- Encryption: SSH transport. Already solved.
- Discovery: user knows the remote path or sets `XDG_RUNTIME_DIR`.
- Failure mode: tunnel dies → client sees EOF, same as a local
  daemon restart. Reattach path is unchanged.

### B — Native TCP + TLS

`pyred` grows an optional `[network]` block:

```toml
[network]
tcp = "0.0.0.0:7711"
tls_cert = "/etc/pyre/cert.pem"
tls_key  = "/etc/pyre/key.pem"
```

rustls terminates TLS, the mode-tag + tarpc framing rides on top
unchanged. Cert provisioning is the unsolved piece: self-signed first
run + pin-on-first-use, mutual TLS with a user-managed CA, or
integration with an OS keystore (libsecret, macOS Keychain) — all
viable, none implemented, all with a real UX bill.

- Auth: client cert or a bearer token; both need a new config surface.
- Encryption: rustls.
- Discovery: hostname + port; trivial.
- Failure mode: same as A from the client's perspective; on the
  daemon side it adds an internet-facing listener that did not exist
  before, which expands threat surface materially.

### C — Custom multiplexer (yamux-over-SSH or QUIC)

Multiplex control + N stream connections over one transport. Buys
fewer file descriptors on the remote host and one round-trip on
reattach. Cost is a new transport crate, a new framing spec, and
more code in the security path. Defer; revisit if A measurably
hurts large-fleet (≥10 panes) reattach latency.

## Comparison

| Axis                       | A — SSH tunnel UDS | B — Native TCP+TLS | C — Mux  |
|----------------------------|--------------------|--------------------|----------|
| pyred code changes         | None               | Listener + rustls + config | New transport |
| Auth                       | SSH (solved)       | New (cert or token)| Same as B|
| Encryption                 | SSH                | rustls             | rustls   |
| Threat surface added       | None (SSH already exposed) | New internet listener | Same as B|
| Cert/secret UX             | None               | Unsolved           | Unsolved |
| Discovery                  | User-driven        | host:port          | host:port|
| Latency overhead per RPC   | SSH+UDS (negligible local-net) | TLS handshake once | One handshake, then multiplexed |
| Time to ship               | Days (docs+flag)   | Weeks (UX design + impl) | Months |
| Sprint fit (M5 of v0.2)    | Yes                | No                 | No       |

## Decision

**Adopt A (SSH tunnel UDS) for v0.2.** Ship `--socket <path>` as the
supported remote-attach surface, document the `ssh -L` recipe in
`docs/AGENTS.md` and the README, and add a `pyrec remote` helper that
wraps the tunnel lifecycle (optional polish; not blocking).

**Flag B (native TCP+TLS) for v0.3** behind a `[network]` config
section that defaults to disabled. Do not implement until cert
provisioning UX has its own ADR — bundling that design into M5 would
sink the sprint. The supervisor (ADR-002 Option C) is the natural
binding point if/when B lands; workers stay UDS-only.

**Defer C** until A or B shows concrete latency or fd-pressure pain.

## Consequences

**Positive**
- Zero pyred changes for v0.2; release risk stays low.
- Inherits SSH's auth, encryption, and operator muscle memory.
- Forces a deliberate, separately-reviewed decision on TLS UX
  rather than smuggling it into a UX sprint.

**Negative**
- Users without SSH access to the remote (rare in target audience,
  but real) have no remote-attach path until v0.3.
- Performance ceiling is whatever SSH gives; large fleets may
  eventually want B or C.
- We do not learn anything about pyre's own wire security until
  v0.3, so the work item does not actually disappear — it is
  scheduled, not solved.

**Follow-ups**
- `docs/AGENTS.md`: add a "Remote attach" section with the `ssh -L`
  recipe and a note that `--socket` already exists today.
- `ROADMAP.md`: mark M5 as "Proposed → SSH tunnel"; create M5.1
  placeholder for the TLS UX ADR.
- Open ADR-0005 stub: "Remote attach TLS provisioning UX" — owner
  unassigned, target v0.3.

## Open questions

1. Does `pyrec remote <host>` belong in v0.2, or is documenting the
   raw `ssh -L` line enough? Helper adds value but also adds surface
   to maintain.
2. How do we surface a clear error when the user attaches to a remote
   `pyred` whose `PROTO_VERSION` differs from local? Today the
   handshake fails with a generic mismatch — adequate, but worth a
   doc line.
3. Does the supervisor (ADR-002 Option C, hybrid) need any change
   to make remote attach feel native — e.g. forwarding worker
   sockets through the tunnel — or does the existing single-socket
   proxy already cover it? Suspected: covered. Verify before
   closing M5.

## References

- [ADR-001 — IPC Transport](ADR-001-ipc.md)
- [ADR-002 — Daemon Process Architecture](0002-daemon-process-architecture.md)
- [ADR-003 — Render backend swap](0003-render-backend.md)
- [docs/AGENTS.md](../AGENTS.md)
- `crates/pyred/src/main.rs::socket_path`
- `crates/pyre-tui/src/main.rs::default_socket`, `control_client`
