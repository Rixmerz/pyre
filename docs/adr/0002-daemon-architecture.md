# ADR-002: Daemon Process Architecture

**Status:** Decision pending  
**Context:** pyred process model — single daemon vs. per-session daemons vs. supervisor+workers

## Trade-offs

| Trade-off | 1 daemon (actual) | 1 daemon/session | Hybrid (supervisor + workers) |
|---|---|---|---|
| Crash isolation | ❌ Daemon muere → todas sessions perdidas | ✅ Una muere, otras viven | ✅ Worker muere, supervisor reinicia |
| Memoria | ✅ 1× rocksdb + 1× tantivy | ❌ N× todo | ⚠️ Supervisor light + workers light |
| Cross-session search (block_search) | ✅ Funciona out-of-box | ❌ Necesita federación | ⚠️ Supervisor agrega índice |
| Lock contention | ⚠️ Tantivy LockBusy (observado) | ✅ Cada uno su índice | ✅ Workers no comparten |
| Discovery | ✅ 1 socket | ❌ N sockets → discovery service | ✅ 1 socket supervisor |
| Reattach | ✅ Cualquier TUI ve todo | ❌ Complicado | ✅ Via supervisor |
| Resource limits per session | ❌ Imposible | ✅ cgroups directos | ✅ cgroup por worker |
| Per-PTY blast radius (fork bomb) | ❌ Degrada todo | ✅ Aislado | ✅ Aislado |

## Options

**Option A — 1 daemon (current)**  
Single `pyred` process owns all sessions, one RocksDB + one Tantivy index. Simple, but no crash isolation and Tantivy `LockBusy` under concurrent writes.

**Option B — 1 daemon per session**  
Each session spawns its own `pyred`. Full isolation but N× memory, N sockets, cross-session `block_search` requires federation layer.

**Option C — Hybrid: supervisor + workers**  
Thin supervisor on 1 socket; spawns a worker process per session. Workers are isolated (crash, resource, blast radius) while supervisor owns the aggregated index and handles discovery/reattach. Medium complexity.

## Decision

TBD. Current status: Option A in production. Option C is the natural upgrade path once crash isolation or per-session resource limits become requirements.
