# pyre MCP skill (copy into your agent config)

Use **pyre-mcp** as a stdio MCP server. Point your client at the `pyre-mcp`
binary built from this repository.

## Tools

| Tool | Purpose |
|------|---------|
| `session_spawn` | New session + pane |
| `pane_open` | Extra pane in a session |
| `pane_send_keys` | Inject input |
| `pane_capture` | Ring buffer (`source: ring`) or last block stdout (`block-last`) |
| `wait_pane_state` | Wait for `blocked` / `working` / `done` (maps to daemon states) |
| `pane_set_state` | Self-report state from an integration hook |
| `block_search` | Full-text search across all command history |
| `session_close` | Tear down a session |

## Resources

- `pane://<session-prefix>/<pane-prefix>` — JSON pane metadata
- `pane://<session-prefix>/<pane-prefix>/output` — recent terminal text
- `block://<block-uuid>` — stdout of one finalized command

## Multi-agent layout

Run `pyred` with `process_model = "hybrid"` and use **one session per agent**.
Worker crash isolation keeps other agents alive. See [AGENTS.md](AGENTS.md).

## Example

```json
{
  "mcpServers": {
    "pyre": {
      "command": "/path/to/pyre-mcp",
      "env": {
        "PYRE_SOCK": "/run/user/1000/pyre.sock"
      }
    }
  }
}
```

Mutating tools require `[mcp].allow_mutations = true` in pyred config when enforced.
