---
title: Tool & Shell Permissions
description: Configure allow, ask, and deny policy for exposed tools and shell commands.
draft: false
---

Permissions refine access inside hard sandbox boundaries.

| Field | Purpose |
| --- | --- |
| `permissions.tools` | Allow, ask, or deny exposed tools such as `context7_search` or wildcard patterns like `context7_*`. |
| `permissions.shell` | Allow, ask, or deny shell commands or command classes. |
| `permissions.tools.request_filesystem_access` | Controls whether the agent may request extra filesystem access. |

Permissions can require approval or deny an action. They cannot widen filesystem or network hard boundaries.

## Built-in shell rules

`workspace` and `readonly` ship with the same shell permission table:

| Pattern | Action |
| --- | --- |
| `bash -c` | `ask` |
| `bash -lc` | `ask` |
| `chmod` | `ask` |
| `chown` | `ask` |
| `dd` | `ask` |
| `find -delete` | `ask` |
| `git push` | `ask` |
| `git reset --hard` | `ask` |
| `mkfs` | `ask` |
| `node -e` | `ask` |
| `python -c` | `ask` |
| `python3 -c` | `ask` |
| `rm -fr` | `deny` |
| `rm -r` | `ask` |
| `rm -rf` | `deny` |
| `sh -c` | `ask` |
| `sudo` | `ask` |
| `zsh -c` | `ask` |
| `zsh -lc` | `ask` |

`danger` ships with no default shell permission rules.

If `request_filesystem_access` is not configured, the agent may request access, but the user still has to approve and protected paths remain protected.

## Example

```json
{
  "permissions": {
    "tools": {
      "request_filesystem_access": "ask",
      "context7_*": "allow"
    },
    "shell": {
      "npm run build": "allow",
      "curl *": "ask"
    }
  }
}
```
