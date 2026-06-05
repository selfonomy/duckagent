---
title: Overview
description: Start here to understand what DuckAgent is and which path to follow first.
draft: false
---

DuckAgent is a local-first AI agent runtime: one compact Rust binary, 30+ model providers, 30+ Gateway channels, profile-scoped identity, MCP and `SKILL.md` capabilities, built-in search, rewindable sessions, and a JSON sandbox you can inspect before the agent touches your machine.

You can use it directly in a terminal, or run it as a Gateway service so chat apps, webhooks, automations, and API clients can reach the same Agent Loop.

## Start fast

```bash
# TUI: local chat, approvals, model/profile switching, and hands-on workspace work.
duck

# Service: Gateway channels, API clients, webhooks, and scheduled tasks.
duck gateway service start
```

The first run opens setup if the active profile has no usable model yet. Web search is available out of the box through Exa MCP; configure an Exa key later if you want your own quota.

## Choose your path

| Goal | Start with |
| --- | --- |
| Chat with DuckAgent locally | [Getting Started](/start/getting-started/) |
| Install the binary | [Install](/start/install/) |
| Decide between terminal UI and background service | [TUI or Service?](/start/tui-or-service/) |
| Resume or rewind a session | [Session Rewind](/reference/session-rewind/) |
| Understand config files and secrets | [Configuration Basics](/start/configuration/) |
| Find where profiles, sessions, memories, and gateway state live | [Where Files Live](/start/files/) |

## Product areas

- [Avatar & Identity](/avatar/) covers profiles, `SOUL.md`, `USER.md`, avatar files, SillyTavern cards, and `AGENTS.md`.
- [Capabilities](/capabilities/) explains filesystem, process, shell, web, memory, scheduled tasks, skills, MCP, and runtime tools.
- [Benchmark](/benchmark/) explains the guarded recoverable context policy, cache-friendly append-only prompt design, and the 108M raw tool token savings report.
- [Gateway](/gateway/) connects external chat channels, voice bridges, webhooks, automations, and API surfaces.
- [Sandbox](/sandbox/) describes the execution boundary for files, network, environment, tools, and shell commands.
- [Reference](/reference/) keeps exact CLI and config details.
