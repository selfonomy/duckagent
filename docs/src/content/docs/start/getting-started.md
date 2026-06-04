---
title: Getting Started
description: Run DuckAgent for the first time and choose between local TUI usage and Gateway service usage.
draft: false
---

Run DuckAgent locally:

```bash
duck
```

If the active profile has no usable model, DuckAgent opens setup first. Setup collects the provider, auth, model, optional context window, and web provider choices. After setup, DuckAgent enters the terminal UI.

## TUI or service?

Most users should start with the TUI:

```bash
duck
```

Use Gateway service when you want external apps or API clients to reach DuckAgent:

```bash
duck gateway service start
```

The TUI is a foreground terminal experience. Gateway service is the background service surface for Telegram, Slack, Matrix, email, SMS, Home Assistant, API Server, voice bridges, and other channels.

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

## What setup writes

Sensitive values go to profile `auth.json`. Normal runtime choices go to profile `config.json`. The root config only stores global state such as the active profile and sandbox preset.
