---
title: Supported Channels
description: Gateway channel names, aliases, and support expectations.
draft: false
---

The gateway registry exposes channel adapters by name and alias. Each production channel should keep platform-specific code inside `src/gateway/channels/<name>.rs` and share common behavior through gateway core.

## Channel names

| Group | Channels |
| --- | --- |
| Team chat | `slack`, `discord`, `mattermost`, `msteams`, `teams`, `googlechat`, `google_chat`, `nextcloud-talk`, `nextcloud_talk`, `synology-chat`, `synology_chat`, `tlon`, `irc` |
| Messaging | `telegram`, `signal`, `whatsapp`, `line`, `sms`, `zalo`, `zalouser`, `weixin`, `qqbot`, `yuanbao`, `nostr` |
| Enterprise China | `feishu`, `lark`, `feishu_comment`, `lark_comment`, `dingtalk`, `wecom`, `wecom_callback` |
| Apple and local bridges | `bluebubbles`, `imessage` |
| Email and cloud | `email`, `msgraph_webhook` |
| Automation and voice | `homeassistant`, `home_assistant`, `voice-call`, `voice_call`, `talk-voice`, `talk_voice` |
| API | `api_server` |

Internal and test-only adapters include `webhook`, `qa-channel`, and websocket helper paths. They are useful for development and fixtures but should not be presented as normal user-facing chat software in setup pickers.

## How to configure a channel

Use the channel manager:

```bash
duck gateway channels
```

Choose Add Channel, pick the platform, and follow the prompts. The setup flow writes non-secret channel config to profile `config.json` and secret material to profile `auth.json`.

The table below summarizes what setup asks for. Field names and credentials are intentionally platform-specific; use [Configure Channels](/gateway/configure-channels/) for the shared schema.

| Channel | Setup inputs |
| --- | --- |
| `telegram` | BotFather bot token, optional allowed Telegram user ids. Setup uses Bot API polling by default. |
| `slack` | `xoxb-` bot token, `xapp-` Socket Mode app token, optional signing secret, allowed channels/users. |
| `signal` | `signal-cli` HTTP daemon URL, account/number, DM policy, optional allowed users and groups. |
| `matrix` | Homeserver URL, access token, bot user id if token validation cannot detect it, allowed rooms/users. |
| `feishu`, `lark` | Scan-to-create or existing app id/app secret, WebSocket or webhook mode, optional verification/encrypt keys for webhook mode. |
| `feishu_comment`, `lark_comment` | App id/app secret, document allowlist, comment policy, WebSocket or webhook mode. |
| `discord` | Bot token, allowed guild/channel/user ids, mention policy. |
| `mattermost` | Server URL, bot or personal access token, allowed channels/users. |
| `api_server` | Optional bearer token. Exposes OpenAI-compatible HTTP endpoints through Gateway. |
| `whatsapp` | WhatsApp Cloud API credentials and webhook verification fields, or an external bridge transport when selected. |
| `dingtalk` | Client id/app key and client secret/app secret. Setup uses Stream Mode by default. |
| `wecom` | Enterprise WeChat AI Bot id and secret. Setup uses AI Bot WebSocket by default. |
| `wecom_callback` | Enterprise WeChat encrypted callback credentials and independent callback namespace. |
| `weixin` | Weixin iLink QR login flow. Runtime stores the returned token and API base. |
| `bluebubbles`, `imessage` | BlueBubbles server URL and password, DM/group policies, webhook path. `imessage` keeps a separate namespace. |
| `email` | IMAP host/account/password, SMTP host/account/password, polling interval, attachment policy. |
| `sms` | Twilio-compatible account SID, auth token, from number, webhook URL, optional JSON bridge secret. |
| `msgraph_webhook` | Graph bridge URL, notification URL, optional bridge token, optional clientState secret, accepted resource filters. |
| `msteams`, `teams` | Bot Framework app id/client secret, optional tenant id, messaging endpoint, allowed conversations/users. |
| `googlechat`, `google_chat` | Bearer access token, Google project id, Pub/Sub subscription, REST/PubSub settings. |
| `line` | Channel access token, channel secret, webhook URL, group policy. |
| `irc` | Server, port, TLS flag, nickname, channel list, optional server/NickServ passwords. |
| `nextcloud-talk`, `nextcloud_talk` | Nextcloud server URL, bot username/app password, optional webhook secret, room allowlist. |
| `nostr` | Private key or `nsec`, relay URLs, allowed pubkeys. Bridge mode has separate bridge URL/token prompts. |
| `synology-chat`, `synology_chat` | Incoming webhook URL, optional bot username, outgoing webhook secret, allowed channels/users. |
| `tlon` | External Tlon/Urbit bridge URL, optional bearer token and webhook secret, conversation allowlist. |
| `twitch` | OAuth token, username, channel list, mention policy. |
| `zalo` | External Zalo Official Account/business bridge URL, app/account ids, allowlists, optional secrets. |
| `zalouser` | External Zalo user/session bridge URL, app/session ids, allowlists, optional secrets. |
| `homeassistant`, `home_assistant` | Home Assistant base URL, long-lived access token, notify service, watched domains/entities, cooldown, optional command webhook secret. |
| `qqbot` | QQ Bot app id and app secret, markdown support flag, DM/group policy. |
| `yuanbao` | Yuanbao app id/app secret, bot accounts, direct WebSocket/API domain settings, DM/group policy. |
| `voice-call`, `voice_call` | External voice bridge URL, optional bearer token and webhook secret, provider label, call/caller allowlists. |
| `talk-voice`, `talk_voice` | External talk-voice bridge URL, optional bearer token and webhook secret, provider label, conversation/participant allowlists. |

## Alias policy

Aliases must route to one implementation while preserving the configured namespace. For example, `teams` and `msteams` share the Microsoft Teams adapter, and `imessage` uses the BlueBubbles backend while keeping an iMessage-facing channel name.

## Minimum expectations

Each production adapter should document and test:

- Required config and auth fields.
- Inbound text to Agent Loop.
- Outbound text delivery.
- Session key derivation for DM, group, channel, thread, and topic surfaces.
- Attachment handoff and media limits.
- Typing behavior, even if the platform only supports a no-op.
- Approval behavior through native UI or text fallback.
- Dedupe behavior for platform event ids.
