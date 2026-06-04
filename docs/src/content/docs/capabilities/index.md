---
title: Overview
description: Understand what DuckAgent can do through built-in capabilities, memory, skills, MCP, and web tools.
draft: false
---

Capabilities are the actions DuckAgent can request at runtime. They are shaped by the active profile, workspace, MCP servers, and sandbox policy.

## Capability areas

| Page | What it explains |
| --- | --- |
| [Built-in Capabilities](/capabilities/builtin/) | The default capability groups DuckAgent can expose. |
| [Filesystem Tools](/capabilities/filesystem/) | Read, search, write, edit, and patch files. |
| [Process & Shell](/capabilities/process-shell/) | Start processes and run shell commands under sandbox policy. |
| [Web Search & Extract](/capabilities/web-search-extract/) | Default Exa search, local extraction, and browser fallback. |
| [Memory](/capabilities/memory/) | Durable profile and workspace memory. |
| [Scheduled Tasks](/capabilities/cron/) | Durable reminders and automations backed by program-internal scheduling. |
| [Skills](/capabilities/skills/) | Profile-local `SKILL.md` workflows. |
| [MCP](/capabilities/mcp/) | External tools exposed through MCP servers. |

Sandbox policy decides what can run, what needs approval, and what must be denied.
