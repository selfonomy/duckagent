---
title: Config Reference
description: Root and profile configuration fields supported by DuckAgent.
draft: false
---

DuckAgent uses a small root config and richer profile config.

## Root config

Path:

```text
~/.duckagent/config.json
```

Common fields:

| Field | Type | Default | Purpose |
| --- | --- | --- | --- |
| `active_profile` | string | `default` when no profile is selected | Profile used by `duck` when `--profile` is not provided. |
| `sandbox` | object | `{ "preset": "workspace" }` | Root-scoped sandbox policy. |

Example:

```json
{
  "active_profile": "default",
  "sandbox": {
    "preset": "workspace"
  }
}
```

## Profile config

Path:

```text
~/.duckagent/profiles/<name>/config.json
```

Common fields:

| Field | Type | Purpose |
| --- | --- | --- |
| `provider` | string | Model provider name. |
| `model` | string | Active model id. |
| `base_url` | string | Provider base URL for OpenAI-compatible or custom APIs. |
| `api_mode` | string | Provider API mode, such as responses or chat-compatible mode. |
| `context_window` | number | Optional context-window override. |
| `web` | object | Web search and extraction providers. |
| `gateway` | object | Gateway enablement, service settings, media limits, and channels. |
| `mcpServers` | object | MCP server definitions. |

Example:

```json
{
  "provider": "openai",
  "model": "gpt-5",
  "base_url": "https://api.openai.com/v1",
  "api_mode": "responses",
  "context_window": 200000,
  "web": {
    "search": { "provider": "exa" },
    "extract": { "provider": "local" },
    "browser_fallback": "auto"
  },
  "gateway": {
    "enabled": true,
    "channels": {}
  },
  "mcpServers": {}
}
```

## Auth files

Provider and gateway secrets belong in:

```text
~/.duckagent/profiles/<name>/auth.json
```

MCP OAuth or server-specific auth material belongs in:

```text
~/.duckagent/profiles/<name>/mcp-auth.json
```

Do not store secrets in `config.json`.

## Provider environment overrides

Most provider credentials are saved in `auth.json`. A few advanced provider
integration knobs can also come from the environment:

| Variable | Purpose |
| --- | --- |
| `DUCKAGENT_GEMINI_CLIENT_ID` | Override the public Gemini CLI desktop OAuth client id used for Google Gemini CLI OAuth refresh. |
| `DUCKAGENT_GEMINI_CLIENT_SECRET` | Override the public Gemini CLI desktop OAuth client secret used for Google Gemini CLI OAuth refresh. |

## Strictness

Sandbox config is strict: unsupported fields in fixed-schema objects should fail parsing instead of being silently ignored. This is important for safety because a typo should not look like a working policy.
