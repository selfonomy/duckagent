---
title: TUI Reference
description: Terminal UI feature support.
draft: false
---

| Area | Supported behavior |
| --- | --- |
| Startup | Opens chat when a model is configured; opens setup when no model is usable. |
| Rendering | Markdown, code blocks, streaming assistant output, tool status, errors, and final responses. |
| Input | Multiline editing, bracketed paste, running-input submission, and non-blocking user submissions. |
| Models | `duck model` manager and first-run provider setup. |
| Profiles | `duck profiles`, active profile switching, avatar display, and profile-scoped state. |
| Sessions | New session, resume, rewind, tracked file restore, compaction, and runtime metadata inspection. |
| Gateway parity | Shared `/new`, `/resume`, and `/rewind` semantics with Gateway routes. |
| Approvals | Filesystem, shell, process, MCP, network, and sandbox-related approval UI. |
| Sandbox setup | Windows elevated setup flow when required. |
| Web setup | Web search and web extract provider selection. |

## Chat commands

| Command | Behavior |
| --- | --- |
| `/new` | Start a fresh session. |
| `/resume` | List recent sessions. |
| `/resume <number>` | Resume one session from the latest list. |
| `/rewind` | List user-turn rewind points. |
| `/rewind <number>` | Rewind to before that turn and restore tracked file changes when safe. |
| `/model` | Open model management. |

See [Session Rewind](/reference/session-rewind/) for exact file restore behavior and limits.
