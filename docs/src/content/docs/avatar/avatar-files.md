---
title: Avatar Files
description: Add, import, and replace DuckAgent profile avatar files.
draft: false
---

The TUI can show a startup avatar from the active profile.

Lookup order:

```text
avatar.png
avatar.jpg
avatar.jpeg
avatar.webp
avatar.gif
```

Place avatar files in:

```text
~/.duckagent/profiles/<name>/
```

## Import through profile setup

Run:

```bash
duck profiles
```

When adding a profile, the avatar prompt accepts a local path or `http`/`https` URL. Supported formats are PNG, JPG, JPEG, WEBP, and GIF. The import limit is 20 MiB.

## Replace manually

Copy a supported avatar file into the profile directory. If several avatar files exist, the lookup order above decides which one is displayed.
