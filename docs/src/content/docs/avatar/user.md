---
title: USER.md
description: Use USER.md to describe durable user preferences for a profile.
draft: false
---

`USER.md` is the profile's durable user-context file.

```text
~/.duckagent/profiles/<name>/USER.md
```

Use it for stable user preferences:

- Preferred answer style.
- Project conventions that follow the user across workspaces.
- Repeated constraints.
- Background information the agent should remember for this profile.

New profiles create `USER.md` as a blank editable file. DuckAgent does not guess who the user is by default.

Example:

```md
The user prefers English documentation.
The user values production-grade architecture and clear file responsibilities.
Avoid vague first-version thinking.
```

Do not put secrets in `USER.md`.
