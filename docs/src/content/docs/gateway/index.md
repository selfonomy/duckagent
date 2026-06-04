---
title: Gateway Overview
description: Connect external chat channels to the same DuckAgent Agent Loop.
draft: false
---

Gateway lets external chat surfaces send messages into DuckAgent and receive responses from the same Agent Loop used by the TUI. It is profile-scoped, configuration-driven, and shares session-control commands with the terminal UI.

## What Gateway provides

| Area | Behavior |
| --- | --- |
| Channel adapters | Each platform has a dedicated adapter under `src/gateway/channels/`. |
| Shared dispatch | Polling, websocket, callback, and bridge-backed adapters submit inbound messages through the same gateway path. |
| Session routing | Sessions are keyed by channel, conversation, and thread so multiple platforms do not collide. |
| Media handoff | Attachments are copied through gateway attachment storage and staged under `$TMPDIR` for tool use. |
| Approvals | Native buttons or cards are used when possible; text fallback uses `/approve` and `/deny`. |
| Service lifecycle | `duck gateway service start`, `log`, and `stop` manage the profile gateway service. |

## Gateway state

Gateway runtime state belongs to the active profile:

```text
~/.duckagent/profiles/<name>/gateway/
  state/
  attachments/
  service/
```

Gateway does not use `cache/` for durable routing state. Cache should remain disposable.

## User flow

1. Run `duck gateway channels` to inspect available channels.
2. Configure a channel through guided setup or profile config.
3. Start the service with `duck gateway service start`.
4. Watch newly routed messages with `duck gateway service log`.
5. Stop the service with `duck gateway service stop`.

When channel config is missing, service start should guide the user into setup instead of requiring raw bind or channel flags.

Use [Configure Channels](/gateway/configure-channels/) for field defaults, `auth.json` boundaries, stable bind behavior, and manual config examples.

## Shared session commands

Gateway channels share these commands with the TUI:

```text
/new [title]
/resume
/resume <number>
/rewind
/rewind <number>
```

These commands append routing changes instead of editing old session files.

`/rewind <number>` also shares the TUI's tracked file restore behavior for files changed through built-in file mutation capabilities. See [Session Rewind](/reference/session-rewind/) for the checksum guard and limits.
