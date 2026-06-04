---
title: Sandbox Config Reference
description: Detailed sandbox schema, defaults, and CLI checks.
draft: false
---

Sandbox config lives in root `~/.duckagent/config.json`.

```json
{
  "sandbox": {
    "preset": "workspace"
  }
}
```

## Default

When `sandbox` is missing, DuckAgent behaves as if this config existed:

```json
{
  "sandbox": {
    "preset": "workspace"
  }
}
```

`workspace` is the normal daily-use policy. It allows broad reads, writes only to the workspace and temp directories, guarded network behavior, and approval checks for sensitive operations.

## Fields

| Field | Type | Default | Purpose |
| --- | --- | --- | --- |
| `preset` | string | `workspace` | Active preset name. Built-ins are `workspace`, `readonly`, and `danger`. |
| `presets` | object | `{}` | User-defined preset map. |
| `extends` | string | none | Optional preset inheritance for custom presets. |
| `filesystem` | object | preset-defined | Mount and path rules. |
| `network` | object | preset-defined | Ordinary network policy. |
| `env` | object | preset-defined | Environment inheritance and secret behavior. |
| `permissions` | object | preset-defined | Tool and shell allow, ask, or deny policy. |

`network.ports` and `ipc` are not stable sandbox schema fields. If they appear in config, DuckAgent should reject the config instead of ignoring them.

## Filesystem schema

`filesystem.mounts` are broad access grants. Mount access can be `ro` or `rw`.

```json
{
  "filesystem": {
    "mounts": [
      { "path": "*", "access": "ro" },
      { "path": ".", "access": "rw" },
      { "path": "$TMPDIR", "access": "rw" }
    ],
    "rules": [
      { "path": ".env", "access": "none" }
    ]
  }
}
```

`filesystem.rules` refine or deny paths inside those mounts. Rule access can be `none`, `ro`, or `rw`.

## Network schema

```json
{
  "network": {
    "mode": "proxy",
    "hosts": {
      "*": "ask",
      "localhost": "allow"
    },
    "addresses": {
      "169.254.0.0/16": "deny"
    }
  }
}
```

`mode` can be `proxy`, `deny`, or `allow`. Host and address actions can be `allow`, `ask`, or `deny`.

## Env secret schema

```json
{
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
```

`inject.url` is the name of the base URL environment variable that DuckAgent rewrites to the local secret reverse proxy. `inject.header` is the upstream header to inject. `inject.format` must contain `{}` and is filled with the real parent secret.

## Custom preset example

```json
{
  "sandbox": {
    "preset": "docs-review",
    "presets": {
      "docs-review": {
        "extends": "readonly",
        "filesystem": {
          "mounts": [
            { "path": "$CWD/docs", "access": "rw" },
            { "path": "$TMPDIR", "access": "rw" }
          ]
        },
        "network": {
          "mode": "deny"
        },
        "permissions": {
          "shell": {
            "npm run build": "ask"
          },
          "tools": {
            "request_filesystem_access": "deny"
          }
        }
      }
    }
  }
}
```

This example starts from `readonly`, opens writes only for docs and temp files, keeps ordinary network denied, asks before the docs build command, and prevents extra filesystem access requests.

## CLI checks

```bash
duck sandbox list
duck sandbox get workspace
duck sandbox check workspace
```

`check` reports whether the current platform can enforce the preset without silently weakening it.
