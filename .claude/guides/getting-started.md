# Getting Started with jig

`jig` is your single MCP entry point. It proxies every other MCP you have configured, exposes ~29 tools at session start (instead of hundreds), and enforces disciplined, phase-gated workflows.

## The three moves

1. **Discover** what tools are available for what you need:
   ```
   proxy_tools_search(query="find function definitions in this codebase")
   ```
   Returns ranked tools across all your proxied MCPs. No subprocess spawned.

2. **Invoke** the right tool:
   ```
   execute_mcp_tool(mcp_name="serena", tool_name="find_symbol", arguments={"name": "MyClass"})
   ```
   The proxy subprocess spawns on demand, stays warm for ~10 minutes, then shuts down.

3. **Enforce process**:
   ```
   graph_activate(graph_id="demo-feature")
   graph_traverse(direction="next")
   ```
   Workflows can block Edit/Write until you've called specific tools — impossible to skip the "think before you modify" phase.

## What's bundled

| Category | How to access |
|----------|---------------|
| Bundled agents (19) | `jig_deploy_agents(stack="python")` |
| Skills (26) | `jig_guide(topic="<skill-name>")` |
| Rules (23) | Copied to `.claude/rules/` by `jig init` |
| Commands (5) | Copied to `.claude/commands/` by `jig init` |
| Workflows | `graph_list_available()` or look in `.claude/workflows/` |

## Next steps

- `jig_guide(topic="create-workflow")` — design your own workflow
- `jig_guide(topic="proxy-design")` — understand the proxy model
- `jig_guide(topic="snapshots")` — use shadow-branch snapshots safely
- `jig_guide(topic="tensions")` — interpret DCC quality gates
