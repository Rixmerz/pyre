# ADR-002: Daemon Process Architecture

## Status
Accepted — 2026-05-18. Option A in production; Option C reserved as upgrade path.
Supersedes the earlier 0002-daemon-architecture.md stub.

## Context

`pyred` is the single daemon process that owns all PTY state for a pyre user session. It binds one Unix Domain Socket at `$XDG_RUNTIME_DIR/pyre.sock` (mode 0700) and multiplexes two connection modes off that socket: `0x01` for tarpc control (bincode, length-delimited) and `0x02` for raw bidirectional stream frames per pane. Clients — `pyrec`, `pyre-tui`, and future MCP bridges — connect and detach over this socket without restarting the daemon.

The central state object is `SessionRegistry`: a `HashMap<SessionId, Arc<SessionState>>` where each `SessionState` owns a `HashMap<PaneId, Arc<PaneState>>`. Each `PaneState` holds the PTY master handle and a `CancellationToken` that fires on child exit. Sessions and block metadata persist in a SQLite database (sqlx 0.8, WAL mode) at `$XDG_DATA_HOME/pyre/state.db`; stdout blobs are stored as zstd-compressed files on disk. A Tantivy 0.26 full-text index at `data_dir/index/` is updated on Block finalization, enabling `block_search` across all sessions.

If the daemon crashes — panic or OOM — all attached clients receive EOF, all live PTYs are orphaned, and there is no auto-recovery on restart. Session metadata survives in SQLite, but the live PTY bridge is gone. This blast radius is acceptable at v0.1 but becomes a user-visible failure mode as session count grows.

The process architecture decision is load-bearing for resource budgets, search architecture, and the feasibility of per-session cgroup limits. The eight forces below shape the trade-off space.

Forces bearing on the decision:
- **Crash isolation**: daemon panic currently kills every session simultaneously. Users lose all live PTY bridges in one event.
- **Memory footprint**: SQLite WAL pool, Tantivy writer heap, and zstd index are paid once with a single daemon vs. N times with per-session daemons. On a 16 GiB laptop with 8 sessions this difference is non-trivial.
- **Cross-session search**: `block_search` spans all sessions; a single index makes this free without any federation protocol.
- **Lock contention**: Tantivy `LockBusy` observed under concurrent Block finalization; WAL + 50 ms batching is the current mitigation but does not eliminate contention under sustained load.
- **Discovery**: one well-known socket path is trivial for clients; N sockets require either a naming convention (fragile) or a registry service (complexity).
- **Reattach semantics**: any `pyrec` or `pyre-tui` instance can attach to any session because they all talk to the same daemon and the same `SessionRegistry`.
- **Per-session resource limits**: cgroup v2 assignment maps cleanly to a process boundary; impossible to enforce inside a monolithic multi-session daemon.
- **Per-PTY blast radius**: a fork-bomb in one PTY saturates the tokio thread pool and degrades I/O for every unrelated session in the same process.

## Options

### Option A — Single daemon (current)

One `pyred` process owns all sessions, all PTYs, one UDS, one SQLite pool, one Tantivy writer. This is the current implementation.

Pros: simplest model, no discovery service, cross-session search works out of the box, single sqlite + tantivy index, lowest memory footprint.

Cons: no crash isolation (daemon panic kills all sessions), impossible to assign per-session cgroups, Tantivy `LockBusy` risk under heavy concurrent indexing degrades unrelated sessions.

### Option B — One daemon per session

Each session spawns its own `pyred-session` process with its own socket, its own SQLite shard, and its own Tantivy index.

Pros: full crash isolation (one session dies, others survive), each process maps cleanly to a cgroup, no lock contention across sessions.

Cons: N× memory (N sqlite pools + N tantivy writers + N index directories), N sockets require a discovery service or a directory scan on attach, cross-session `block_search` requires index federation across all shard daemons.

### Option C — Hybrid (supervisor + workers)

A thin supervisor process binds the single public socket and owns the aggregated Tantivy index and `SessionRegistry` metadata. Each session is a worker process spawned by the supervisor, holding the PTY and a local block cache. Workers stream `BlockEvent` to the supervisor for indexing.

Pros: worker crash kills only its session (supervisor restarts it), per-worker cgroup assignment, single entry socket, supervisor-owned index keeps cross-session search unified.

Cons: medium implementation complexity — supervisor/worker IPC layer, state handoff on worker restart, two-tier process lifecycle management.

### Comparison

| Trade-off | 1 daemon (A) | 1 daemon/session (B) | Hybrid (C) |
|---|---|---|---|
| Crash isolation | No — daemon dies, all sessions lost | Yes — one dies, others survive | Yes — worker dies, supervisor restarts |
| Memory | 1× sqlite + 1× tantivy | N× everything | Supervisor light + workers light |
| Cross-session search (block_search) | Works out of box | Needs index federation | Supervisor aggregates index |
| Lock contention | Tantivy LockBusy (observed) | Per-instance index, no sharing | Workers don't share index |
| Discovery | 1 socket | N sockets → discovery service | 1 supervisor socket |
| Reattach | Any client sees everything | Complicated — must locate the right socket | Via supervisor |
| Per-session resource limits | Impossible | cgroups direct | cgroup per worker |
| Per-PTY blast radius | Degrades everything | Isolated | Isolated |

## Decision

Accept Option A for v0.1.

Rationale:
- At v0.1 user load (single user, handful of sessions), the blast-radius downside of Option A is bounded and recoverable: daemon restart is fast, SQLite survives, and the user re-attaches.
- Cross-session `block_search` is a first-class feature (SPEC.md). Deferring federation complexity to a post-MVP phase is the right call.
- Memory footprint matters: pyre runs on developer workstations alongside many other processes; N× sqlite + tantivy is an unreasonable cost for early adopters.
- The single-socket + single-registry model keeps the tarpc transport surface trivial.

Migration triggers — move to Option C when any of these become true:
- Tantivy `LockBusy` becomes user-visible: search blocked more than 100 ms under normal load.
- Any user requests per-session cgroup limits (resource-constrained environments, CI isolation).
- A single fork-bomb in one PTY measurably degrades unrelated sessions (manual repro confirms I/O starvation).
- A daemon-panic blast radius is cited in any filed bug report (not just hypothetical).

## Consequences

**Positive:** simplest model, one socket, cross-session search free, single SQLite + Tantivy index, no discovery service, no federation layer, no IPC between supervisor and workers.

**Negative:** no crash isolation — daemon panic evicts all sessions simultaneously; no per-session resource caps; Tantivy lock contention risk under heavy concurrent Block finalization.

**Mitigations:**
- Retry-on-`LockBusy` for Tantivy index writes (exponential backoff, short window — 3 attempts at 10/50/200 ms before surfacing the error to the caller).
- Document blast radius in SPEC.md under "Operational notes" so users know to `screen` or `tmux` long-running sessions externally if they need PTY survival across daemon restarts.
- Prepare systemd unit with `Restart=on-failure` and `RestartSec=1s` so daemon auto-restarts in production installs (future work, tracked in ROADMAP.md).
- Existing 50 ms Block write batching (ROADMAP.md risk mitigation) already reduces Tantivy contention frequency by collapsing high-frequency OSC 133 events into fewer index commits.
- Session metadata durability is handled by SQLite WAL; clients that reconnect after a daemon restart can re-query history immediately without re-indexing.

## Upgrade path to Option C

The hybrid model requires these structural changes:

1. Extract a thin **supervisor** process that binds `/run/user/$UID/pyre.sock` (the current public socket), owns `SessionRegistry` (session metadata only, no PTYs), and owns the aggregated Tantivy index writer.
2. Each session becomes a **worker** process spawned by the supervisor; per-worker socket at `/run/user/$UID/pyre/session-<id>.sock`; worker owns the PTY, the `PaneState` map, and a local block cache (in-memory ring buffer).
3. Worker streams `BlockEvent { session_id, block_id, fields }` to the supervisor over a private UDS pipe; supervisor writes the Tantivy document and the zstd blob. Workers never touch the shared index.
4. `block_search` RPC arrives at the supervisor socket; supervisor queries its own index across all sessions — same code path as today, different process boundary.
5. Opt-in migration via config flag: `pyred.process_model = "single" | "hybrid"` (default `"single"`). Workers inherit `PYRE_DATA_DIR` and `PYRE_SESSION_ID` from supervisor environment.
6. Crash recovery: supervisor detects worker exit via `waitpid`; replays session pane list from SQLite; respawns worker; new worker re-opens PTY (shells survive as orphaned processes, reattachable via PTY inheritance if OS supports it).
7. cgroup assignment: supervisor calls `cgroup_v2_move_pid(worker_pid, session_id)` after `fork`; per-session memory and CPU limits become first-class config knobs.
8. The tarpc `PyreDaemon` trait stays unchanged from the client's perspective; the supervisor transparently proxies session-scoped RPCs (`open_pane`, `write_input`, `kill_pane`) to the correct worker socket. Clients need no changes.
9. Block stream mode (`0x02`) is rerouted at the supervisor: incoming attach requests are forwarded to the worker's per-session socket, which then streams frames back through the supervisor to the client — or directly, if the client negotiates a worker-redirect at attach time.

## Open questions deferred to Option C design

- Whether supervisor-to-worker IPC reuses the existing tarpc/bincode framing or a lighter pipe protocol (MPSC channels over anonymous UDS).
- Whether workers share the zstd blob store directory or each write to a per-session subdirectory (affects concurrent write safety).
- Whether a worker crash that orphans a PTY should attempt PTY re-adoption (Linux `ioctl TIOCSCTTY` semantics) or simply record the session as detached in SQLite.

These are not blocking for v0.1; they are recorded here so the Option C design phase starts with the right questions.

## References

- [SPEC.md](../../SPEC.md)
- [ARCHITECTURE.md](../../ARCHITECTURE.md)
- [docs/adr/ADR-001-ipc.md](ADR-001-ipc.md)
- Workflow: `.claude/workflows/adr-002-daemon-arch.yaml`
