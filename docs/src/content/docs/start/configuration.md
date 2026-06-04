---
title: Configuration Basics
description: Understand root config, profile config, auth files, gateway config, MCP config, and sandbox policy.
draft: false
---

DuckAgent separates machine-level state from profile-level state.

| Layer | File or directory | Purpose |
| --- | --- | --- |
| Root config | `~/.duckagent/config.json` | Active profile and root `sandbox` policy. |
| Profile config | `~/.duckagent/profiles/<name>/config.json` | Provider, model, API mode, context window, web config, gateway config, and MCP servers. |
| Provider and gateway auth | `~/.duckagent/profiles/<name>/auth.json` | API keys, web provider keys, gateway tokens, app secrets, and webhook secrets. |
| MCP auth | `~/.duckagent/profiles/<name>/mcp-auth.json` | OAuth or token material for MCP servers. |

Provider-specific OAuth adjunct files also stay under the active profile unless they are owned by an external CLI. For example, Google Gemini CLI OAuth uses `~/.duckagent/profiles/<name>/auth/google_oauth.json` for DuckAgent-owned credentials and may read Gemini CLI's own `~/.gemini/oauth_creds.json`; it does not read legacy product homes such as `~/.hermes`.

Google Gemini CLI OAuth refresh uses Google's public Gemini CLI desktop OAuth client by default. Advanced users can override the client with `DUCKAGENT_GEMINI_CLIENT_ID` and `DUCKAGENT_GEMINI_CLIENT_SECRET`.

## Root config

```json
{
  "active_profile": "default",
  "sandbox": {
    "preset": "workspace"
  }
}
```

Sandbox config is root-scoped so switching profiles cannot silently weaken execution boundaries.

## Profile config

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
    "channels": {}
  },
  "mcpServers": {}
}
```

Keep secrets out of `config.json`.
