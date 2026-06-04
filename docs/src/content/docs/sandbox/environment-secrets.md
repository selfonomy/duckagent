---
title: Environment & Secrets
description: Understand environment inheritance, secret placeholders, and MCP env grants.
draft: false
---

Sandbox `env` controls variables inherited from the DuckAgent parent process.

The built-in presets use:

```json
{
  "env": {
    "*": "allow"
  }
}
```

That means ordinary parent environment variables are inherited by default. You can make this stricter with `deny` or `ask` rules.

## Env rule actions

| Action | Meaning |
| --- | --- |
| `allow` | Pass matching parent environment variables into the sandboxed process. |
| `ask` | Ask for approval before passing matching variables. Approved values can be allowed once, for the session, or always. |
| `deny` | Do not pass matching variables. |

Example:

```json
{
  "sandbox": {
    "preset": "workspace-with-env-review",
    "presets": {
      "workspace-with-env-review": {
        "extends": "workspace",
        "env": {
          "*": "deny",
          "PATH": "allow",
          "HOME": "allow",
          "CI_*": "ask"
        }
      }
    }
  }
}
```

## Secret-backed network requests

A variable can be marked as a sandbox secret. This is for tools that need to call an HTTP API with a token, while keeping the real token out of the child process environment.

Secret entries require `network.mode = "proxy"`.

```json
{
  "sandbox": {
    "preset": "api-workspace",
    "presets": {
      "api-workspace": {
        "extends": "workspace",
        "network": {
          "mode": "proxy",
          "hosts": {
            "api.openai.com": "allow"
          }
        },
        "env": {
          "OPENAI_API_KEY": {
            "type": "secret",
            "inject": {
              "url": "OPENAI_BASE_URL",
              "header": "Authorization",
              "format": "Bearer {}"
            }
          }
        }
      }
    }
  }
}
```

The parent process must have both variables:

```bash
export OPENAI_API_KEY="sk-..."
export OPENAI_BASE_URL="https://api.openai.com"
```

The sandboxed child sees:

```text
OPENAI_API_KEY=duckagent-secret:OPENAI_API_KEY
OPENAI_BASE_URL=http://127.0.0.1:<port>/__duckagent_secret/OPENAI_API_KEY
```

When the child sends a request to `OPENAI_BASE_URL`, DuckAgent reverse-proxies the request to the real upstream URL and injects:

```text
Authorization: Bearer <real OPENAI_API_KEY>
```

The path and query are preserved. For example:

```text
http://127.0.0.1:<port>/__duckagent_secret/OPENAI_API_KEY/v1/responses?q=1
```

is forwarded to:

```text
https://api.openai.com/v1/responses?q=1
```

The secret reverse proxy supports ordinary request bodies with `Content-Length`. Chunked request bodies are rejected.

## MCP env is different

`mcpServers.<name>.env` is an explicit user grant to that MCP server:

```json
{
  "mcpServers": {
    "docs": {
      "command": "docs-mcp",
      "env": {
        "DOCS_API_KEY": "${DOCS_API_KEY}"
      }
    }
  }
}
```

Do not treat explicit MCP env grants as accidental inherited environment.
