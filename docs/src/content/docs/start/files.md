---
title: Where Files Live
description: Find DuckAgent profiles, config, auth, sessions, memories, skills, gateway state, audit logs, and avatar files.
draft: false
---

DuckAgent state lives under:

```text
~/.duckagent/
```

## Root

```text
~/.duckagent/config.json
```

Root config stores the active profile and the root sandbox policy.

## Active profile

```text
~/.duckagent/profiles/<name>/
  config.json
  auth.json
  mcp-auth.json
  sessions/
  memories/
  skills/
  cache/
  audit/
  gateway/
  SOUL.md
  USER.md
  AGENTS.md
  avatar.png
  avatar.json
```

Most runtime behavior belongs to the active profile: model config, credentials, memory, skills, sessions, gateway channels, audit logs, identity files, and avatar files.

`SOUL.md`, `USER.md`, and `avatar.png` are initialized from bundled defaults when missing. Empty `SOUL.md` files receive the bundled default persona. Existing non-empty files are preserved so users can edit them safely.

## Project instructions

Project instructions live in the workspace:

```text
<workspace>/AGENTS.md
```

Path-specific `AGENTS.md` files can also live in subdirectories.
