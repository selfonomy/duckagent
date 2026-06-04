---
title: AGENTS.md Instructions
description: Use profile, project, and path-local AGENTS.md instructions.
draft: false
---

`AGENTS.md` files tell DuckAgent how to operate in a profile, project, or path subtree.

## Locations

| Location | Scope |
| --- | --- |
| `~/.duckagent/profiles/<name>/AGENTS.md` | Profile-level operating instructions. |
| `<workspace>/AGENTS.md` | Current workspace instructions. |
| `<workspace>/<subtree>/AGENTS.md` | Path-local instructions discovered when tools touch that subtree. |

## Discovery

Main user messages include profile instructions and the current workspace `AGENTS.md` when present.

Path-aware tools can also discover `AGENTS.md` files upward from the target path for up to five levels. Broader parent instructions are presented before more specific child instructions.

Path discovery applies to capabilities such as file reads, file search, content search, writes, edits, patch application, and process starts with an explicit working directory.

## What to write

Good instructions are concrete:

- File and folder responsibilities.
- Testing expectations.
- Documentation update rules.
- Safety rules for generated files and sessions.
- Project-specific architecture constraints.

Avoid secrets and avoid vague style wishes that do not change behavior.
