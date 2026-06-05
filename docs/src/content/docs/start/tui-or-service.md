---
title: TUI or Service?
description: Decide whether to use the local terminal UI or the Gateway service.
draft: false
---

DuckAgent has two main user-facing entry points.

| Entry point | Command | Use it when |
| --- | --- | --- |
| TUI | `duck` | You want to chat locally, approve actions, switch models, switch profiles, and work in a terminal. |
| Gateway service | `duck gateway service start` | You want external chat apps, webhooks, voice bridges, or API clients to talk to DuckAgent. |

## TUI

The TUI is foreground and interactive. It supports streaming output, Markdown, multiline input, bracketed paste, slash commands, approvals, model setup, profile management, and startup avatars.

Use it for local work:

```bash
duck
```

## Gateway service

Gateway service is profile-scoped. It starts configured channel adapters and routes external messages into the same Agent Loop.

```bash
duck gateway service start
duck gateway service log
duck gateway service stop
```

`service log` observes routed messages. It does not replace the running service.

## Shared session commands

The TUI and Gateway share session-control commands:

```text
/new
/resume
/resume <number>
/rewind
/rewind <number>
```

These commands append session transitions instead of rewriting old session history.

`/rewind <number>` can also restore tracked file changes from built-in file tools when the current file state still matches the recorded post-change checksum. See [Session Rewind](/reference/session-rewind/) for details.
