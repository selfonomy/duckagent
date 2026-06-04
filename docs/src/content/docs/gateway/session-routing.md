---
title: Session Routing
description: Understand how Gateway maps external conversations to DuckAgent sessions.
draft: false
---

Gateway routes each external conversation to a real DuckAgent session.

The route key includes:

- Channel name.
- Conversation, chat, room, group, or channel id.
- Thread, topic, or message-thread id when the platform has one.

This prevents collisions such as Telegram `chat-a`, Lark `chat-a`, and two topics inside the same group sharing one session by accident.

## State

Gateway mappings live in:

```text
~/.duckagent/profiles/<name>/gateway/state/sessions.jsonl
```

Full messages live in the normal session directory:

```text
~/.duckagent/profiles/<name>/sessions/<session-id>/messages.jsonl
```

Gateway mapping state is append-only. `/new` and `/resume` append new route mappings instead of editing old records.

## Shared commands

```text
/new [title]
/resume
/resume <number>
/rewind
/rewind <number>
```

`/rewind` appends a session rewind event and can restore tracked file snapshots from built-in file mutation tools when the current file checksum still matches the recorded after-state. See [Session Rewind](/reference/session-rewind/).
