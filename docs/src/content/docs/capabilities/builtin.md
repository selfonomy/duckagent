---
title: Built-in Capabilities
description: Built-in capability groups available to DuckAgent.
draft: false
---

DuckAgent projects available capabilities into the agent context for each run.

| Group | Examples |
| --- | --- |
| Filesystem | Read files, search files, search content, write files, edit files, apply patches. |
| Process | Start local processes, inspect output, interrupt processes, and run shell commands. |
| Web | Web search, web extraction, and optional browser fallback. |
| Memory | Request memory review from MainAgent; edit memory through MemoryAgent. |
| Scheduled Tasks | `cron_create`, `cron_list`, `cron_get`, `cron_update`, `cron_delete`, `cron_pause`, and `cron_resume` for durable reminders and automations. |
| Skills | Load a skill manifest and read files inside a loaded skill. |
| MCP | Exposed MCP tools from configured servers. |
| Gateway | Approval and routing state for external channels. |
| Sandbox | Policy summaries and approval boundaries for files, network, environment, tools, and shell. |

Use:

```bash
duck runtime capabilities
```

to inspect what the active runtime exposes.
