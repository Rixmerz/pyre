# pyre — Agent multiplexer playbook

pyre is a standalone terminal product: persistent sessions, block-level
command history, full-text search, and MCP orchestration. Use this guide
when running multiple coding agents in parallel.

## Recommended layout (hybrid mode)

Enable the hybrid supervisor so each agent session runs in an isolated
worker process:

```toml
# ~/.config/pyre/pyred.toml (or your pyred config path)
process_model = "hybrid"
```

Pattern: **one heavy agent = one session = one worker**.

```bash
systemctl --user enable --now pyred   # or pyred in foreground
pyrec session new --name api --cwd ~/projects/api
pyrec session new --name web --cwd ~/projects/web
pyre   # TUI: sidebar shows per-session rollup
```

If one agent OOMs or panics, only that worker dies; the supervisor and
other sessions keep running. Block metadata and search stay unified in
the supervisor index.

## Pane states (UI labels)

The daemon tracks `PaneStateKind`. The TUI maps them to agent-friendly
labels:

| Daemon state   | UI label   | Meaning |
|----------------|------------|---------|
| `WaitingInput` | blocked    | Prompt visible; agent or shell needs input |
| `Running`      | working    | Active output or command running |
| `Idle`         | idle       | Quiet after a block ended |
| `Done`         | done       | Process exited cleanly |
| `Crashed`      | crashed    | Non-zero exit or shell died |
| `Interactive`  | interactive| Full-screen TUI (vim, less, …) |

Agents can override state via MCP `pane_set_state` or `pyrec` after
installing an integration snippet (`pyrec integration install`).

## Detected agents

pyre heuristically classifies the foreground process per pane:

| Kind        | Typical binary / argv |
|-------------|------------------------|
| ClaudeCode  | `claude` |
| Codex       | `codex` |
| Pi          | `pi` |
| OpenCode    | `opencode` |
| CursorAgent | `cursor`, `cursor-agent` |
| Shell       | bash, zsh, fish, … |
| Unknown     | everything else |

Detection runs on the state-engine poll (~500 ms). Integrations can
report state earlier via `set_pane_state`.

## Orchestration without the TUI

```bash
# Wait until a pane needs input (30s timeout)
pyrec wait-pane --pane <id> --state waiting --timeout 30s

# Read recent ring-buffer lines
pyrec pane read --pane <id> --lines 50 --source ring

# Read stdout of the last finalized block
pyrec pane read --pane <id> --source block-last

# Run a command in a session pane
pyrec pane run --session <id> -- ls -la

# Search all command history
pyrec search "error E0425"
```

## MCP (`pyre-mcp`)

Point any MCP client at `pyre-mcp` (stdio JSON-RPC). Tools include
`session_spawn`, `pane_capture`, `pane_set_state`, `block_search`,
`wait_pane_state`, and resources `pane://` / `block://`.

See [docs/agent-skill.md](agent-skill.md) for a copy-paste skill block.

Mutating tools require `[mcp].allow_mutations = true` in config.

## Moats (why pyre for agents)

1. **Blocks** — every Enter is a searchable record (command, cwd, exit, stdout).
2. **`block_search`** — find failures across all sessions in milliseconds.
3. **Hybrid workers** — crash isolation per agent session.
4. **Reattach + replay** — grid snapshot and last N blocks after disconnect.

## Hooks

Optional `hooks.toml` runs local scripts on `on_block_end` and
`on_pane_state` (e.g. desktop notify when a pane becomes `waiting`).
See [CONFIG.md](CONFIG.md).
