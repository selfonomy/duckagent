---
title: SOUL.md
description: Use SOUL.md to define the agent persona for a profile.
draft: false
---

`SOUL.md` is the profile's durable agent persona file.

```text
~/.duckagent/profiles/<name>/SOUL.md
```

Use it for stable traits:

- Tone and style.
- Collaboration preferences.
- Boundaries and values.
- Domain preferences.
- How the agent should present itself.

New profiles start with DuckAgent's bundled default persona. Edit the file directly to change the profile's persona.

Example:

```md
You are concise, careful, and practical.
Prefer direct technical answers.
Ask only when the decision is genuinely ambiguous.
```

`SOUL.md` is dynamic profile context. It does not become a second system prompt.
