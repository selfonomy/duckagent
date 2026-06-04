---
title: Presets
description: Understand the built-in workspace, readonly, and danger sandbox presets.
draft: false
---

DuckAgent has three built-in presets.

| Preset | Use it for | Filesystem | Network |
| --- | --- | --- | --- |
| `workspace` | Default daily use | Broad read, workspace and temp writes, sensitive paths denied, `.git` read-only. | Proxy-mediated with localhost allowed and private/link-local rules. |
| `readonly` | Inspection, review, and low-risk browsing | Read-only. | Denied. |
| `danger` | Fully trusted environments | Broad read/write. | Direct allow. |

## Active preset

The root config selects the active preset:

```json
{
  "sandbox": {
    "preset": "workspace"
  }
}
```

If `sandbox` or `preset` is missing, the active preset is `workspace`. The `--sandbox <preset>` CLI option overrides the active preset for one process only.

## Runtime hard rule

At runtime, DuckAgent also protects its private state under `~/.duckagent`. Agents should not read, copy, modify, or request grants for that directory. Users can inspect or edit it outside DuckAgent when needed.

## workspace preset

`workspace` is the default. It is designed for normal coding and local automation: read broadly, write to the current workspace and temp directory, route ordinary network through the managed proxy, and ask before risky shell command classes.

```json
{
  "filesystem": {
    "mounts": [
      { "path": "*", "access": "ro" },
      { "path": ".", "access": "rw" },
      { "path": "$TMPDIR", "access": "rw" }
    ],
    "rules": [
      { "path": ".env", "access": "none" },
      { "path": ".env.*", "access": "none" },
      { "path": ".git", "access": "ro" },
      { "path": "*.pem", "access": "none" },
      { "path": "*.key", "access": "none" },
      { "path": "id_rsa", "access": "none" },
      { "path": "id_ed25519", "access": "none" },
      { "path": "*.p12", "access": "none" },
      { "path": "*.pfx", "access": "none" }
    ]
  },
  "network": {
    "mode": "proxy",
    "hosts": {
      "*": "ask",
      "127.0.0.1": "allow",
      "::1": "allow",
      "localhost": "allow"
    },
    "addresses": {
      "127.0.0.0/8": "ask",
      "::1/128": "ask",
      "10.0.0.0/8": "ask",
      "172.16.0.0/12": "ask",
      "192.168.0.0/16": "ask",
      "100.64.0.0/10": "ask",
      "169.254.0.0/16": "deny",
      "0.0.0.0/8": "deny",
      "::/128": "deny"
    }
  },
  "env": {
    "*": "allow"
  },
  "permissions": {
    "tools": {},
    "shell": {
      "bash -c": "ask",
      "bash -lc": "ask",
      "chmod": "ask",
      "chown": "ask",
      "dd": "ask",
      "find -delete": "ask",
      "git push": "ask",
      "git reset --hard": "ask",
      "mkfs": "ask",
      "node -e": "ask",
      "python -c": "ask",
      "python3 -c": "ask",
      "rm -fr": "deny",
      "rm -r": "ask",
      "rm -rf": "deny",
      "sh -c": "ask",
      "sudo": "ask",
      "zsh -c": "ask",
      "zsh -lc": "ask"
    }
  }
}
```

## readonly preset

`readonly` is for inspection. It denies ordinary network access and prevents writes.

```json
{
  "filesystem": {
    "mounts": [
      { "path": "*", "access": "ro" },
      { "path": ".", "access": "ro" },
      { "path": "$TMPDIR", "access": "ro" }
    ],
    "rules": [
      { "path": ".env", "access": "none" },
      { "path": ".env.*", "access": "none" },
      { "path": ".git", "access": "ro" },
      { "path": "*.pem", "access": "none" },
      { "path": "*.key", "access": "none" },
      { "path": "id_rsa", "access": "none" },
      { "path": "id_ed25519", "access": "none" },
      { "path": "*.p12", "access": "none" },
      { "path": "*.pfx", "access": "none" }
    ]
  },
  "network": {
    "mode": "deny",
    "hosts": {
      "*": "deny"
    },
    "addresses": {
      "*": "deny"
    }
  },
  "env": {
    "*": "allow"
  },
  "permissions": {
    "tools": {},
    "shell": {
      "bash -c": "ask",
      "bash -lc": "ask",
      "chmod": "ask",
      "chown": "ask",
      "dd": "ask",
      "find -delete": "ask",
      "git push": "ask",
      "git reset --hard": "ask",
      "mkfs": "ask",
      "node -e": "ask",
      "python -c": "ask",
      "python3 -c": "ask",
      "rm -fr": "deny",
      "rm -r": "ask",
      "rm -rf": "deny",
      "sh -c": "ask",
      "sudo": "ask",
      "zsh -c": "ask",
      "zsh -lc": "ask"
    }
  }
}
```

## danger preset

`danger` is an explicit full-access mode for trusted environments. Use it only when commands and workspace content are trusted.

```json
{
  "filesystem": {
    "mounts": [
      { "path": "*", "access": "rw" }
    ],
    "rules": []
  },
  "network": {
    "mode": "allow",
    "hosts": {
      "*": "allow"
    },
    "addresses": {}
  },
  "env": {
    "*": "allow"
  },
  "permissions": {
    "tools": {},
    "shell": {}
  }
}
```

## Inspect locally

```bash
duck sandbox list
duck sandbox get workspace
duck sandbox get readonly
duck sandbox get danger
duck sandbox check workspace
```
