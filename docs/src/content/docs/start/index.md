---
title: Overview
description: Start here to understand what DuckAgent is and which path to follow first.
draft: false
---

DuckAgent is a local AI agent runtime. You can use it directly in a terminal, or run it as a gateway service so chat apps and API clients can reach the same Agent Loop.

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
- [Capabilities](/capabilities/) explains what DuckAgent can do: filesystem, process, shell, web, memory, scheduled tasks, skills, and MCP.
- [Benchmark](/benchmark/) explains the guarded recoverable context policy, cache-friendly append-only prompt design, and benchmark results.
- [Gateway](/gateway/) connects external chat channels and API surfaces.
- [Sandbox](/sandbox/) describes the execution boundary for files, network, environment, tools, and shell commands.
- [Reference](/reference/) keeps exact CLI and config details.
