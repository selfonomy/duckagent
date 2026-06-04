---
title: Profiles
description: Create, select, and understand DuckAgent profiles.
draft: false
---

A profile is an isolated identity and runtime workspace.

```text
~/.duckagent/profiles/<name>/
```

Each profile can own model config, auth, memory, skills, sessions, gateway config, audit logs, `SOUL.md`, `USER.md`, optional `AGENTS.md`, avatar files, and `avatar.json`.

When DuckAgent creates or repairs a profile, it copies the bundled default `avatar.png`, `SOUL.md`, and `USER.md` into the profile directory if those files are missing. Empty `SOUL.md` files are initialized from the default persona. Existing non-empty profile files are not overwritten.

## Create a profile

Open the profile manager:

```bash
duck profiles
```

Choose Add Profile. The flow asks for:

1. Profile name.
2. Optional avatar image from a local path or `http`/`https` URL.
3. Optional one-line `SOUL.md`.
4. Optional one-line `USER.md`.

The new profile becomes active after creation. If no avatar is provided, the bundled default duck avatar is copied into the profile. `USER.md` starts blank unless the setup field is filled.

Values entered in Add Profile have priority over bundled defaults. A provided local or URL avatar replaces the default `avatar.png`; provided `SOUL.md` and `USER.md` lines replace the initialized template contents.

## Select a profile

Use the profile manager to switch the default active profile:

```bash
duck profiles
```

Or use one profile for one process:

```bash
duck --profile work
```

`--profile` does not rewrite root `active_profile`.

## Profile name rules

Profile names must be non-empty, cannot be `.` or `..`, and cannot contain path separators or control characters. Unicode names are accepted.
