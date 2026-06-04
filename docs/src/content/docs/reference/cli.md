---
title: CLI Reference
description: DuckAgent command-line surfaces for TUI, profiles, models, sessions, runtime, gateway, MCP, and sandbox.
draft: false
---

The default command opens the TUI:

```bash
duck
```

Global options:

```bash
duck --profile <name>
duck --sandbox <preset>
```

## Commands

| Command | Purpose |
| --- | --- |
| `duck profiles` | Manage profiles, active profile, profile files, and identity-related state. |
| `duck model` | Manage saved model/provider entries for the active profile. |
| `duck session compact` | Append a compaction transition for a session. |
| `duck session get-all-messages` | Inspect full session messages. |
| `duck session set-runtime` | Set or adjust session runtime metadata. |
| `duck session show-runtime` | Show runtime metadata for a session. |
| `duck runtime resolve` | Resolve the active runtime configuration. |
| `duck runtime list-models` | List models for the configured provider when supported. |
| `duck runtime capabilities` | Show capabilities available in the current runtime. |
| `duck gateway channels` | List registered gateway channels. |
| `duck gateway service start` | Start the configured gateway service for the active profile. |
| `duck gateway service log` | Tail gateway-routed session activity. |
| `duck gateway service stop` | Stop the gateway service. |
| `duck mcp add` | Add an MCP server to profile config. |
| `duck mcp list` | List configured MCP servers. |
| `duck mcp get` | Show one MCP server config. |
| `duck mcp remove` | Remove an MCP server. |
| `duck mcp auth` | Authenticate an MCP server when required. |
| `duck mcp logout` | Remove MCP auth material. |
| `duck sandbox list` | List built-in and configured sandbox presets. |
| `duck sandbox get <preset>` | Print one sandbox preset. |
| `duck sandbox check <preset>` | Check whether the platform can enforce a preset safely. |
| `duck sandbox setup-windows` | Run elevated Windows sandbox setup. |
| `duck sandbox windows-setup-status` | Inspect Windows sandbox setup state. |

Some advanced gateway pairing and low-level sandbox/process commands are intentionally hidden from normal help output. User-facing flows should prefer TUI or guided setup.
