---
title: Filesystem Rules
description: Configure sandbox filesystem mounts and path rules.
draft: false
---

Filesystem policy applies to local tools, shell/process execution, built-in tools, and stdio MCP processes.

## Mounts and rules

`filesystem.mounts` define the broad access map. Mount access can be `ro` or `rw`.

`filesystem.rules` refine that map. Rules can use `none`, `ro`, or `rw`, and are commonly used to deny secrets inside otherwise readable trees.

Built-in `workspace` filesystem config:

```json
{
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
}
```

That means:

- Broad read access.
- Write access to the current workspace.
- Write access to platform temp directories.
- Sensitive paths denied.
- `.git` read-only.

See [Presets](/sandbox/presets/) for the full `workspace`, `readonly`, and `danger` configs.

## Path expansion

Filesystem paths support:

```text
~
$CWD
${CWD}
$HOME
$TMPDIR
$TEMP
$TMP
```

Existing environment variables can also be expanded.

## Protected private state

`~/.duckagent` is private user state. The agent should not read, copy, modify, or request access to it. Users can inspect it outside DuckAgent when needed.

Permissions can make access stricter, but they cannot bypass hard filesystem boundaries.
