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
| Sessions | New session, resume, rewind, tracked file restore, compaction, runtime metadata inspection, and long-running goals. |
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
| `/goal` | Show the current goal, if any. |
| `/goal <objective>` | Set a persistent goal and start automatic continuation turns. |
| `/goal pause` | Pause automatic continuation for the current goal. |
| `/goal resume` | Resume automatic continuation for a paused, blocked, or budget-limited goal. |
| `/goal clear` | Clear the current goal. |

See [Session Rewind](/reference/session-rewind/) for exact file restore behavior and limits.

## Long-running goals

Goals are stored in session metadata and are scoped to the current session. When
a goal is active, DuckAgent continues the agent loop after each completed turn
by injecting goal-continuation context until the model marks the goal
`complete` or `blocked`, the user pauses or clears it, or a configured token
budget is reached.

The goal tools exposed to the model are `get_goal`, `create_goal`, and
`update_goal`. `update_goal` only accepts `complete` or `blocked`; pause,
resume, clear, and budget-limited transitions are controlled by the user or
runtime.
