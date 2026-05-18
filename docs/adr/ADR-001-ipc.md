# ADR-001 — IPC Transport: tonic vs tarpc

## Status
Accepted — 2026-05-18.

## Context
pyre is a Rust terminal daemon: `pyred` owns pane state and PTYs, while `pyrec` (CLI) and `pyre-tui` attach as clients over a Unix Domain Socket at `$XDG_RUNTIME_DIR/pyre.sock`. Commands flow client→daemon and pane output streams daemon→client, so the transport must support bidirectional streaming with low local-host latency. IPC choice is load-bearing: it shapes the schema layout in `pyre-proto`, the workspace dep tree, and the cost of adding non-Rust clients later. We need to lock this in at S0 because S1 lands the daemon transport and bench harness, and revisiting after that is expensive.

## Options Considered

### Option A: tonic (gRPC over UDS)
- Pros: cross-language clients trivially possible (future MCP bridge in TS, Go); battle-tested; streaming RPCs first-class; protobuf evolution rules well understood; tooling (grpcurl, Wireshark) ready.
- Cons: HTTP/2 framing overhead on local socket; protobuf codegen toolchain (need `protoc`); larger dep tree (~tower, hyper, h2); schema lives in `.proto` separate from Rust types so duplication with `pyre-proto`.

### Option B: tarpc (Rust-native RPC)
- Pros: zero IDL — protocol IS the Rust trait; reuses `pyre-proto` types directly; minimal deps (~tokio, serde, futures); lightweight framing (bincode or json); fastest path to S1.
- Cons: Rust-only ecosystem (cross-language clients would need re-serialization); fewer eyes / smaller community; schema versioning is on us (no .proto wire-compat rules); harder to introspect on the wire.

## Comparison Table
| Axis | tonic | tarpc |
|---|---|---|
| Latency over UDS (local) | ~µs + HTTP/2 frame overhead | ~µs, minimal framing |
| Codegen ergonomics | external `protoc`, build.rs | pure macro, no toolchain |
| Schema versioning | protobuf rules | hand-rolled w/ serde |
| Dep weight (transitive crates) | ~120 | ~40 |
| Cross-language clients | yes (gRPC) | Rust-only |
| Streaming RPCs | first-class | supported |
| Maturity / community | very large | moderate |

## Decision
**tarpc.**

Rationale:
- S0–S4 is Rust-only — pyred + pyrec + pyre-tui all live in this workspace. Cross-language clients aren't on the roadmap until S5 (MCP bridge), and even there MCP runs over stdio + JSON-RPC, not direct IPC.
- Reusing `pyre-proto` types directly via serde eliminates a duplicate schema in `.proto`.
- Lower dep weight = faster `cargo check`, faster CI, smaller binary — matters because daemon should start fast.
- Schema-versioning risk is acceptable because IPC is a single-host, single-user, same-binary-pair contract; we control both sides.

## Revisit Triggers
- A non-Rust client becomes required (MCP server out-of-process in another language, or a remote-attach feature). → Re-evaluate tonic.
- tarpc upstream goes unmaintained. → Either fork or migrate.
- We need request multiplexing semantics richer than tarpc's stream support.
- IPC framing overhead measurable in latency profiling (>5% of attach roundtrip budget).

## Consequences
- `pyre-proto` is the single source of truth for wire types (already set up in P3).
- Add `tarpc` + `tokio-serde` + `bincode` deps to workspace in S1 when the daemon transport lands.
- Bench harness (criterion) gets added in S1 with a baseline echo RPC to detect regressions if we ever revisit.
