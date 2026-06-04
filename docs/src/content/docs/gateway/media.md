---
title: Media & Attachments
description: Understand Gateway attachment handoff, media limits, voice media, and outbound media behavior.
draft: false
---

Gateway adapters normalize inbound attachments before they reach tools.

## Inbound media

Inbound media should enter Gateway attachment storage first:

```text
~/.duckagent/profiles/<name>/gateway/attachments/
```

Files used by tools are staged under `$TMPDIR` and remain constrained by sandbox policy.

Default media download limit:

```json
{
  "media": {
    "max_download_bytes": 26214400,
    "allow_voice": true
  }
}
```

The default is 25 MiB.

## Outbound media

Outbound media uses platform-native delivery when supported. If a platform cannot send a requested media type, the adapter should return a clear unsupported error instead of silently dropping the file.

Voice channels and audio bridges use the same policy idea, but their provider-specific STT, TTS, recording, and call-control behavior belongs to the external bridge.
