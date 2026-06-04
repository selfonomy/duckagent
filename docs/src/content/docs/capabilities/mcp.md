---
title: MCP
description: Configure MCP servers, auth files, environment grants, and sandbox tool permissions.
draft: false
---

MCP servers are configured per profile in `config.json`:

```json
{
  "mcpServers": {
    "docs": {
      "command": "docs-mcp",
      "args": [],
      "env": {
        "DOCS_API_KEY": "${DOCS_API_KEY}"
      }
    }
  }
}
```

MCP auth material belongs in:

```text
~/.duckagent/profiles/<name>/mcp-auth.json
```

## Commands

```bash
duck mcp add
duck mcp list
duck mcp get <name>
duck mcp remove <name>
duck mcp auth <name>
duck mcp logout <name>
```

## Sandbox relationship

`mcpServers.<name>.env` is an explicit grant to that MCP server. It is different from parent environment inheritance controlled by `sandbox.env`.

Tool-level sandbox policy can match exposed MCP tool names such as `context7_search` or `context7_*`.
