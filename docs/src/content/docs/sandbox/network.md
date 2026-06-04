---
title: Network Rules
description: Understand ordinary network sandbox behavior.
draft: false
---

Network policy controls ordinary network access for shell, process, and built-in tools.

Built-in behavior:

| Preset | Network |
| --- | --- |
| `workspace` | Proxy-mediated. Localhost is allowed. Private destinations may ask. Link-local is denied. |
| `readonly` | Denied. |
| `danger` | Allowed. |

In proxy mode, DuckAgent checks both the requested host and resolved addresses. A deny on either side blocks the request. An ask on either side triggers approval.

## Proxy environment

When `network.mode` is `proxy`, DuckAgent starts a managed local proxy and passes proxy settings to sandboxed child processes.

The proxy env covers common HTTP, HTTPS, WebSocket, npm, and Yarn clients:

```text
HTTP_PROXY
HTTPS_PROXY
ALL_PROXY
WS_PROXY
WSS_PROXY
http_proxy
https_proxy
all_proxy
ws_proxy
wss_proxy
NPM_CONFIG_PROXY
NPM_CONFIG_HTTP_PROXY
NPM_CONFIG_HTTPS_PROXY
YARN_HTTP_PROXY
YARN_HTTPS_PROXY
```

No-proxy variables are cleared so requests do not bypass DuckAgent's policy:

```text
NO_PROXY
no_proxy
NPM_CONFIG_NO_PROXY
NPM_CONFIG_NOPROXY
npm_config_no_proxy
npm_config_noproxy
YARN_NO_PROXY
yarn_no_proxy
GLOBAL_AGENT_NO_PROXY
global_agent_no_proxy
```

The proxy evaluates the target host and resolved IP address before forwarding. If the parent process already uses an upstream proxy, DuckAgent can forward through that upstream while still enforcing sandbox policy first.

## Env-backed network requests

Sandbox `env` can define secret-backed requests. In that mode, the child process does not receive the real token. It receives:

- a placeholder token value such as `duckagent-secret:OPENAI_API_KEY`;
- a rewritten base URL such as `http://127.0.0.1:<port>/__duckagent_secret/OPENAI_API_KEY`.

Requests to that local URL are reverse-proxied to the configured upstream URL. DuckAgent injects the real secret into the configured header before sending the upstream request.

See [Environment & Secrets](/sandbox/environment-secrets/) for the full config example.

MCP servers that connect through their own transport are not controlled only by ordinary network rules. Use MCP config and `permissions.tools` for those boundaries.
