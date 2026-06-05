---
title: Install
description: Install DuckAgent from the selfonomy/duckagent installer script.
draft: false
---

Install DuckAgent with the installer script:

```bash
curl -fsSL https://raw.githubusercontent.com/selfonomy/duckagent/main/scripts/install.sh | bash
```

On Windows, run the PowerShell installer:

```powershell
irm https://raw.githubusercontent.com/selfonomy/duckagent/main/scripts/install.ps1 | iex
```

After installation, choose an entry point:

```bash
# TUI: local chat, approvals, model/profile switching, and workspace work.
duck

# Service: Gateway channels, API clients, webhooks, and scheduled tasks.
duck gateway service start
```

The first `duck` run opens setup if no model is configured yet. Start with the TUI when you want the fastest local experience. Start the Gateway service when you want Telegram, Slack, Discord, Matrix, Signal, email, SMS, WhatsApp, Home Assistant, voice bridges, webhooks, or API clients to reach DuckAgent in the background.

The installers download the latest GitHub Release archive for the current OS
and CPU, verify its SHA-256 checksum, install the command as `duck`, and add the
install directory to PATH when needed.

## Next steps

1. Use [Getting Started](/start/getting-started/) for the first run.
2. Use [TUI or Service?](/start/tui-or-service/) to choose the right entry point.
3. Use [Configuration Basics](/start/configuration/) before editing config files manually.
