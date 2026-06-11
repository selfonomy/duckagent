---
title: Getting Started
description: Run DuckAgent for the first time and choose between local TUI usage and Gateway service usage.
draft: false
---

Run DuckAgent locally:

```bash
duck
```

That starts the TUI: local chat, streaming output, approvals, model/profile switching, slash commands, persistent goals, and hands-on workspace work. If the active profile has no usable model, DuckAgent opens setup first. Setup collects the provider, auth, model, optional context window, and web provider choices. After setup, DuckAgent enters the terminal UI.

DuckAgent ships with built-in web search. The default search route uses Exa MCP so you can try search immediately; add an Exa key later if you want your own quota.

## TUI or service?

Most users should start with the TUI:

```bash
duck
```

Use Gateway service when you want external apps or API clients to reach DuckAgent:

```bash
duck gateway service start
```

The TUI is a foreground terminal experience. Gateway service is the background service surface for Telegram, Slack, Discord, Matrix, Signal, email, SMS, WhatsApp, Home Assistant, API Server, voice bridges, webhooks, automations, and other channels.

## First useful commands

```bash
duck
duck model
duck profiles
duck gateway channels
duck gateway service start
duck sandbox check workspace
```

Use `--profile` to run one process with a specific profile:

```bash
duck --profile work
```

Use `--sandbox` to temporarily select a sandbox preset for one process:

```bash
duck --sandbox readonly
```

For work that should continue across multiple agent turns, start a goal:

```text
/goal migrate the docs site to the new theme
```

DuckAgent keeps pursuing the goal until the agent marks it complete or blocked,
or you run `/goal pause` or `/goal clear`.

## What setup writes

Sensitive values go to profile `auth.json`. Normal runtime choices go to profile `config.json`. Each profile also owns its sessions, memories, skills, Gateway config, `SOUL.md`, `USER.md`, optional `AGENTS.md`, and avatar files. The root config only stores global state such as the active profile and sandbox preset.
