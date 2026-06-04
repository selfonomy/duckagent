---
title: Gateway Config Reference
description: Exact Gateway config fields and defaults.
draft: false
---

Gateway config lives in profile `config.json` under `gateway`.

## Gateway fields

| Field | Default | Purpose |
| --- | --- | --- |
| `bind` | `127.0.0.1:0` | Local listener bind address. Callback channels usually need stable bind. |
| `channels` | `{}` | Map of channel name to channel config. |

## Channel fields

| Field | Default | Purpose |
| --- | --- | --- |
| `enabled` | `true` | Start this channel with Gateway service. |
| `transport` | unset | Channel transport mode. |
| `api_base` | unset | Platform API base URL or bridge base URL. |
| `allowed_users` | `[]` | Allowed sender ids. |
| `allowed_chats` | `[]` | Allowed chat, room, group, channel, thread, or resource ids. |
| `home` | unset | Optional default outbound target. |
| `extra` | `{}` | Channel-specific non-secret settings. |
| `typing.enabled` | `true` | Enable typing/status refresh when supported. |
| `typing.refresh_seconds` | `4` | Typing refresh interval. |
| `media.max_download_bytes` | `26214400` | Inbound media download limit. |
| `media.allow_voice` | `true` | Allow voice/audio media handoff where supported. |
| `approval.mode` | `native_with_command_fallback` | Approval prompt behavior. |
| `access.dm_policy` | `open` | Direct-message access policy. |
| `access.group_policy` | `open` | Group/channel access policy. |
| `access.require_mention` | `true` | Mention or wake-pattern gating where supported. |

Secrets belong in profile `auth.json`.
