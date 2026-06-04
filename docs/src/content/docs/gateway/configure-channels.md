---
title: Configure Channels
description: Configure Gateway channels with guided setup or profile JSON.
draft: false
---

Use guided setup for normal channel configuration:

```bash
duck gateway channels
```

Choose Add Channel, select a platform, and follow the prompts. Setup writes non-secret channel settings to profile `config.json` and secrets to profile `auth.json`.

`duck gateway service start` also enters setup when the active profile has no usable Gateway config.

## Shared config shape

```json
{
  "gateway": {
    "bind": "127.0.0.1:8788",
    "channels": {
      "telegram": {
        "enabled": true,
        "transport": "polling",
        "allowed_users": ["123456789"],
        "allowed_chats": [],
        "typing": {
          "enabled": true,
          "refresh_seconds": 4
        },
        "media": {
          "max_download_bytes": 26214400,
          "allow_voice": true
        },
        "approval": {
          "mode": "native_with_command_fallback"
        },
        "access": {
          "dm_policy": "open",
          "group_policy": "open",
          "require_mention": true
        }
      }
    }
  }
}
```

## Stable bind

`gateway.bind` defaults to `127.0.0.1:0`. Webhook and callback channels need a stable listener, so guided setup uses `127.0.0.1:8788` when needed.

Polling, websocket, relay, and direct API channels often do not need stable bind.

## Secrets

Store tokens, app secrets, webhook secrets, OAuth tokens, app passwords, bridge bearer tokens, and signing secrets in:

```text
~/.duckagent/profiles/<name>/auth.json
```

Do not store secrets in profile `config.json`.

## Manual config

Manual config is useful for review and automation, but guided setup is safer because it validates obvious inputs, stores credentials in the right place, and removes stable bind when no configured channel needs it.
