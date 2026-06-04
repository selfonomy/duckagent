---
title: Localization
description: How the docs reserve multilingual support while keeping English as the source locale.
draft: false
---

DuckAgent documentation is English-first today, with the Starlight project configured so future translations can be added without changing the primary source routes.

## Current locale

`astro.config.mjs` sets:

```js
defaultLocale: 'root',
locales: {
  root: {
    label: 'English',
    lang: 'en-US',
  },
}
```

This makes the root routes the canonical English documentation.

## Adding translations later

When translated content is ready, add a locale entry and mirror the content tree:

```text
docs/src/content/docs/
  getting-started.md
  configuration.md
  zh/
    getting-started.md
    configuration.md
```

Keep slugs aligned across locales where possible. A translated page should cover the same product behavior as the English page, not become a separate source of truth.

## Translation policy

- English remains the source of truth for implementation accuracy.
- Translations should be updated in the same release window as English docs.
- Code, command names, config keys, JSON fields, and file paths should stay literal.
- If a translation falls behind, keep the English page authoritative rather than guessing.
