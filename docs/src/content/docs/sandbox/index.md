---
title: Overview
description: Understand DuckAgent's sandbox as the execution boundary for local tools.
draft: false
---

Sandbox is one of DuckAgent's core safety features. It controls local tool execution: filesystem access, ordinary network access, environment inheritance, secret handling, MCP tool policy, and shell command policy.

Sandbox config lives in root config:

```text
~/.duckagent/config.json
```

It is not profile-scoped. Switching profiles should not silently weaken the local execution boundary.

## Default

If no sandbox is configured, DuckAgent uses:

```json
{
  "sandbox": {
    "preset": "workspace"
  }
}
```

The default active preset is `workspace`.

| Preset | When to use | Main behavior |
| --- | --- | --- |
| `workspace` | Daily development and agent work | Broad read access, workspace/temp writes, guarded network proxy, and approval for risky shell commands. |
| `readonly` | Review and inspection | Read-only filesystem, denied network, and the same risky shell approval table. |
| `danger` | Fully trusted local execution | Direct process execution, broad filesystem access, direct network, and no default shell approval table. |

Use `--sandbox <preset>` to override one process:

```bash
duck --sandbox readonly
duck --sandbox danger
```

Use `duck sandbox get <preset>` to inspect the resolved preset JSON that DuckAgent will use.

## Network and env secrets

In `proxy` network mode, DuckAgent starts a managed local proxy and sets proxy-related environment variables for sandboxed child processes. Sandbox `env` can also define secret-backed network requests: the child receives a placeholder value and a local reverse-proxy URL, while the proxy injects the real secret as a request header.

See [Environment & Secrets](/sandbox/environment-secrets/) for the exact config shape.

## Windows behavior

On Windows, users should not normally run setup commands by memory. First run checks the active sandbox after model/provider setup. If a non-`danger` preset needs elevated setup, DuckAgent prompts the user to set up the default sandbox, switch to `danger`, or quit.

The Windows commands are for preflight, repair, and troubleshooting:

```bash
duck sandbox windows-setup-status
duck sandbox setup-windows
```

## Read next

- [Presets](/sandbox/presets/) includes the shipped JSON for `workspace`, `readonly`, and `danger`.
- [Filesystem Rules](/sandbox/filesystem/) explains read/write boundaries.
- [Network Rules](/sandbox/network/) explains proxy, deny, allow, and approval behavior.
- [Environment & Secrets](/sandbox/environment-secrets/) explains inherited variables and secret placeholders.
- [Tool & Shell Permissions](/sandbox/tool-shell-permissions/) explains allow, ask, and deny policy.
- [Windows Setup](/sandbox/windows/) explains elevated setup.
