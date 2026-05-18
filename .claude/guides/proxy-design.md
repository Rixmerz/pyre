# Proxy Design

## Why proxies

Every MCP you register in `.mcp.json` ships its entire tool catalog into Claude Code's context at session start. With 7-10 MCPs, that's easily 200+ tool schemas the model must parse before it does anything useful.

`jig` proxies MCPs internally: your `.mcp.json` has just `jig`, and jig handles the rest. At session start Claude sees ~29 tools. It discovers the rest via `proxy_tools_search` on demand.

## What to proxy

Proxy **local** MCPs (anything with a `command` in `.mcp.json`):
- Serena, Sequential Thinking, Context7, Playwright, filesystem, git, etc.

Do NOT proxy **remote** MCPs (anything with `url` / `type: "http"`):
- Canva, Gmail, Zapier, Cloudflare — these are served over HTTP and can't be subprocess-proxied.

## Lifecycle

```
proxy_add(name, command, args, env)    → registered + warmed up (tools embedded)
                                          subprocess NOT spawned yet
execute_mcp_tool(name, tool, args)     → subprocess spawns, stays warm 10 min
proxy_list()                            → see status across all proxies
(after 10 min idle)                     → proxy shuts itself down; next call re-spawns
proxy_reconnect(name)                  → force restart (use after MCP upgrade)
proxy_refresh(name)                    → re-embed tools (after MCP tool catalog changes)
proxy_remove(name)                      → unregister + stop + purge cache
```

## Secrets

API keys live in `~/.config/jig/proxy.toml` under the proxy's `env` dict. File permissions: keep it `0600` on multi-user machines. Environment variables on the host machine override the config.

## Debugging

- `proxy_list()` — shows `connected`, `tool_count`, `last_error`
- Proxy refuses to start: check `command` resolves on `$PATH`
- `tools_search` returns nothing for a proxy: the cache is empty — call `proxy_refresh(name)` to re-embed
- Tool hangs: raise `JIG_PROXY_TIMEOUT` env var (seconds), default 120s (360s for DCC visualizations)

## When NOT to use proxies

If an MCP exposes ≤3 tools AND you want those tools visible at session start, register it directly in `.mcp.json` alongside `jig`. The proxy is overhead for tiny tool counts.
