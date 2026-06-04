---
title: Web Search & Extract
description: Configure web search, web extraction, Exa auth, local extraction, and browser fallback.
draft: false
---

Web capabilities are profile-scoped.

Default config:

```json
{
  "web": {
    "search": { "provider": "exa" },
    "extract": { "provider": "local" },
    "browser_fallback": "auto"
  }
}
```

## Search

The default search provider is Exa. Store the Exa API key in profile `auth.json`, not in `config.json`.

## Extract

The default extract provider is local. Local extraction keeps page fetching and parsing inside DuckAgent instead of requiring a remote extraction service.

## Browser fallback

`browser_fallback: "auto"` lets DuckAgent use a local browser fallback when a rendered page is needed. Network and local execution still follow sandbox and approval policy.
