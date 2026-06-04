---
title: Channel Matrix
description: Gateway channel support matrix for inbound, outbound, media, typing, approvals, and setup.
draft: false
---

| Channel | Inbound | Outbound | Media | Typing | Approval |
| --- | --- | --- | --- | --- | --- |
| `telegram` | Polling/webhook | Bot API | Yes | Yes | Buttons or text |
| `slack` | Socket Mode/events | Web API | Yes | Limited | Blocks or text |
| `signal` | HTTP daemon/SSE | HTTP daemon | Yes | Limited | Text |
| `matrix` | Sync | Client API | Yes | Yes | Text |
| `feishu`, `lark` | WebSocket/callback | REST | Yes | Limited | Cards or text |
| `discord` | Gateway | REST | Yes | Yes | Components or text |
| `mattermost` | REST/websocket | REST | Yes | Limited | Text |
| `api_server` | HTTP | HTTP/outbox | External | No-op | API-mediated |
| `whatsapp` | Cloud API/bridge | Graph/bridge | Yes | Limited | Text |
| `dingtalk` | Stream/callback | Webhook/API | Yes | Limited | Cards or text |
| `wecom`, `wecom_callback` | WebSocket/callback | API/callback | Yes | Limited | Cards or text |
| `weixin` | iLink polling | iLink/CDN | Yes | Limited | Text |
| `bluebubbles`, `imessage` | BlueBubbles | BlueBubbles | Yes | Limited | Text |
| `email` | IMAP/provider | SMTP/provider | Attachments | No-op | Text |
| `sms` | Twilio webhook | Twilio REST | MMS | No-op | Text |
| `msteams`, `teams` | Bot Framework | Bot Framework | Yes | Cards | Cards or text |
| `googlechat`, `google_chat` | Pub/Sub/callback | REST | Yes | Cards | Cards or text |
| `line` | Callback | Reply/push | Yes | Limited | Templates or text |
| `irc` | IRC | IRC | Links | No-op | Text |
| `nextcloud-talk`, `nextcloud_talk` | Webhook | OCS API | Yes | Limited | Text |
| `nostr` | Relay | Relay | Links | No-op | Text |
| `synology-chat`, `synology_chat` | Outgoing webhook | Incoming webhook | Yes | No-op | Text |
| `tlon` | Bridge | Bridge | Yes | No-op | Text |
| `twitch` | IRC/API | IRC/API | Links | Limited | Text |
| `zalo`, `zalouser` | Bridge | Bridge | Yes | Bridge | Text |
| `homeassistant`, `home_assistant` | WebSocket/events | REST notify | Links | No-op | Text |
| `qqbot` | Gateway | REST | Yes | Limited | Text |
| `yuanbao` | WebSocket/proto | API/proto | Yes | Limited | Text |
| `voice-call`, `voice_call` | Voice bridge | Voice bridge | Audio | Status | Text |
| `talk-voice`, `talk_voice` | Voice bridge | Voice bridge | Audio | Status | Text |
