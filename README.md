<p align="center">
  <img src="src/default/favicon.png" alt="DuckAgent logo" width="180" height="180">
</p>

<h1 align="center">DuckAgent</h1>

<p align="center">
  <strong>A local-first AI agent runtime in one compact Rust binary.</strong><br>
  Bring your own model, your own workspace, your own identity, and a sandbox you can actually read.
</p>

<p align="center">
  <a href="https://github.com/selfonomy/duckagent/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/selfonomy/duckagent/ci.yml?branch=main&style=for-the-badge&label=CI&logo=github" alt="CI"></a>
  <a href="https://selfonomy.github.io/duckagent/"><img src="https://img.shields.io/badge/Docs-GitHub%20Pages-0ea5e9?style=for-the-badge&logo=github" alt="Documentation"></a>
  <a href="LICENSE.txt"><img src="https://img.shields.io/badge/License-Apache--2.0-green?style=for-the-badge" alt="License: Apache-2.0"></a>
</p>

DuckAgent is an agent runtime you configure and run locally. It talks to 30+
LLM providers, including Anthropic, OpenAI, Gemini, Bedrock, Copilot,
OpenRouter, DeepSeek, Kimi, Qwen, xAI, Ollama Cloud, Azure Foundry, and custom
OpenAI-compatible endpoints. It reaches people and systems through 30+ Gateway
channels: Telegram, Slack, Discord, Matrix, Signal, email, SMS, WhatsApp,
Home Assistant, voice bridges, webhooks, and an API server. It acts through
capabilities such as filesystem, shell/process, web search/extract, memory,
cron, MCP servers, skills, and custom runtime tools.

Everything runs on your machine, with your keys, in your workspace. Use it as a
fast terminal UI (`duck`) or keep it alive as a service
(`duck gateway service start`) so chat apps, webhooks, and API clients can all
talk to the same Agent Loop.

## ⚡ Quick Install

Linux, macOS, and WSL2:

```bash
curl -fsSL https://raw.githubusercontent.com/selfonomy/duckagent/main/scripts/install.sh | bash
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/selfonomy/duckagent/main/scripts/install.ps1 | iex
```

After installation, choose how you want to run it:

```bash
# Local terminal UI: fastest way to chat, approve tools, switch models, and work.
duck

# Long-running service: Gateway channels, API clients, automations, and webhooks.
duck gateway service start
```

Full docs: [GitHub Pages](https://selfonomy.github.io/duckagent/)

## ✨ Why DuckAgent Feels Different

- 🦆 **One compact Rust runtime**: a native binary for macOS, Linux, Windows,
  and WSL2. No giant app shell required.
- 🧠 **30+ model providers**: Anthropic, OpenAI, Gemini, Bedrock, Copilot,
  OpenRouter, DeepSeek, Kimi, Qwen, xAI, Ollama Cloud, Azure Foundry, and
  custom OpenAI-compatible endpoints.
- 💬 **30+ channels**: use the same Agent Loop from the TUI, Telegram, Slack,
  Discord, Matrix, Signal, email, SMS, WhatsApp, Home Assistant, voice bridges,
  webhooks, and the OpenAI-compatible API Server.
- 🛡️ **Real sandbox policy**: JSON-configured filesystem mounts, path rules,
  network allow/ask/deny, environment handling, tool approvals, and shell
  command permissions.
- 🧩 **MCP + skills**: load MCP servers, built-in capabilities, and profile
  skills backed by `SKILL.md` files.
- 🪪 **Portable identity**: every profile owns its own model config,
  credentials, Gateway config, memory, skills, `SOUL.md`, `USER.md`,
  optional `AGENTS.md`, and avatar files.
- 🎭 **SillyTavern card support**: `avatar.png` can be a SillyTavern PNG card.
  DuckAgent extracts embedded character metadata and injects it as profile
  context.
- 🔁 **Rewind instead of regret**: `/rewind` lists user turns and can restore
  tracked file edits from `write_file`, `edit`, and `apply_patch` snapshots
  when checksums prove it is safe.
- 🧬 **Self-improving memory**: profile-scoped memories, active memory
  catalogs, `SOUL.md`, and `USER.md` let the agent get better at working with
  you without smearing one profile into another.
- 🔎 **Search works out of the box**: `web_search` defaults to Exa MCP, so a new
  install can search immediately. Configure an Exa key later if you want your
  own quota. Extraction defaults to local parsing with optional browser
  fallback.
- 📅 **Automations without OS cron**: ask for reminders or recurring tasks in
  normal language; the Gateway service runs them from append-only job logs.
- 💸 **Cache-friendly prompt design**: one stable system prompt stays first,
  dynamic context is appended after it, and long tool history is projected into
  recoverable summaries instead of being resent forever.

## 📊 Benchmark And Token Savings

DuckAgent ships an offline benchmark suite under `benchmark/` for long-running
Agent Loop context policy. The current runtime uses
`ContextProjectionPolicy::guarded_mid`, reported as
`duckagent_recoverable_decay_guarded_mid`.

The important idea: DuckAgent keeps the active loop rich while there is room,
then compresses completed tool history into recoverable summaries with exact
handles such as path, offset, limit, process id, cursor, mode, and query. The
model can recover the exact detail it needs without paying to resend every raw
tool result forever.

In `benchmark/results/guarded-mid-vs-balanced-combined/report.md`, guarded-mid
completed `1188/1188` simulated turns and saved roughly **108M raw tool
tokens** through structured projection.

| Model profile | Guarded-mid cache hit | Versus balanced |
| --- | ---: | --- |
| `openai-gpt-5.4` | `96.6%` | `22.6%` lower simulated cost, `23.7%` fewer expected tokens |
| `openai-gpt-5.5` | `96.6%` | `22.6%` lower simulated cost, `23.7%` fewer expected tokens |
| `openai-gpt-5.4-mini` | `87.5%` | `8.1%` lower simulated cost, `30.8%` fewer expected tokens |
| `kimi-k2.6` | `87.3%` | `14.5%` lower simulated cost, `30.6%` fewer expected tokens |
| `deepseek-chat` | `91.3%` | Same as balanced on the `128K` prompt profile |
| `deepseek-v4-flash` | `97.0%` | `16.9%` lower simulated cost, `21.3%` fewer expected tokens |
| `deepseek-v4-pro-promo` | `97.0%` | `15.6%` lower simulated cost, `21.3%` fewer expected tokens |

These numbers come from an offline simulator, not a billing guarantee. They are
still useful because they make context policy measurable instead of vibes-only.

## 🛡️ Default Sandbox

DuckAgent defaults to the `workspace` sandbox. It is built for daily agent work:
read broadly, write only to the current workspace and temp directory, hide
common secrets, keep `.git` read-only, route ordinary network through a managed
proxy, and ask before risky shell command classes.

The active preset is stored in `~/.duckagent/config.json`:

```json
{
  "sandbox": {
    "preset": "workspace"
  }
}
```

The default `workspace` preset expands to:

```json
{
  "filesystem": {
    "mounts": [
      { "path": "*", "access": "ro" },
      { "path": ".", "access": "rw" },
      { "path": "$TMPDIR", "access": "rw" }
    ],
    "rules": [
      { "path": ".env", "access": "none" },
      { "path": ".env.*", "access": "none" },
      { "path": ".git", "access": "ro" },
      { "path": "*.pem", "access": "none" },
      { "path": "*.key", "access": "none" },
      { "path": "id_rsa", "access": "none" },
      { "path": "id_ed25519", "access": "none" },
      { "path": "*.p12", "access": "none" },
      { "path": "*.pfx", "access": "none" }
    ]
  },
  "network": {
    "mode": "proxy",
    "hosts": {
      "*": "ask",
      "127.0.0.1": "allow",
      "::1": "allow",
      "localhost": "allow"
    },
    "addresses": {
      "127.0.0.0/8": "ask",
      "::1/128": "ask",
      "10.0.0.0/8": "ask",
      "172.16.0.0/12": "ask",
      "192.168.0.0/16": "ask",
      "100.64.0.0/10": "ask",
      "169.254.0.0/16": "deny",
      "0.0.0.0/8": "deny",
      "::/128": "deny"
    }
  },
  "env": {
    "*": "allow"
  },
  "permissions": {
    "tools": {},
    "shell": {
      "bash -c": "ask",
      "bash -lc": "ask",
      "chmod": "ask",
      "chown": "ask",
      "dd": "ask",
      "find -delete": "ask",
      "git push": "ask",
      "git reset --hard": "ask",
      "mkfs": "ask",
      "node -e": "ask",
      "python -c": "ask",
      "python3 -c": "ask",
      "rm -fr": "deny",
      "rm -r": "ask",
      "rm -rf": "deny",
      "sh -c": "ask",
      "sudo": "ask",
      "zsh -c": "ask",
      "zsh -lc": "ask"
    }
  }
}
```

Inspect or switch sandbox behavior:

```bash
duck sandbox list
duck sandbox get workspace
duck sandbox check workspace
duck --sandbox readonly
duck --sandbox danger
```

## 🧭 Common Commands

```bash
duck
duck --profile work
duck --sandbox readonly

duck model
duck profiles

duck gateway channels
duck gateway service start
duck gateway service log
duck gateway service stop

duck sandbox list
duck sandbox get workspace
duck sandbox check workspace

duck mcp list
duck mcp add docs https://example.com/mcp
```

Scheduled tasks are created from normal chat, for example:

```text
Remind me in five minutes to buy groceries.
Every day at 8 AM, summarize what we talked about before.
Pause that reminder.
Change the summary task to 9 AM.
```

Tasks fire while the profile's long-running service is active:

```bash
duck gateway service start
```

Session control slash commands work in the TUI and Gateway channels:

```text
/new
/resume
/resume 2
/rewind
/rewind 3
```

`/rewind` appends a rewind event instead of rewriting session JSONL. When file
snapshots were recorded after the target turn, DuckAgent restores old file
contents or deletes newly-created files only if the current file still matches
the recorded post-change checksum.

## 🗂️ Configuration Location

```text
~/.duckagent/
  config.json
  profiles/
    <name>/
      config.json
      auth.json
      mcp-auth.json
      sessions/
      memories/
      skills/
      gateway/
      cron/
        jobs.jsonl
        runs.jsonl
      SOUL.md
      USER.md
      AGENTS.md
      avatar.png
```

The root `config.json` stores the active profile and machine-level sandbox
configuration. Profile `config.json` stores non-sensitive runtime settings for
models, Web providers, Gateway, MCP, and related features. Secrets live in
`auth.json` or `mcp-auth.json`.

New profiles receive editable copies of the bundled default `avatar.png`,
`SOUL.md`, and `USER.md`. Empty `SOUL.md` files are initialized from the default
bundled persona. The default `USER.md` is intentionally blank so the agent does
not assume user background or preferences.

## 📚 Documentation

The documentation site lives in `docs/` and uses Astro + Starlight:

```bash
cd docs
pnpm install
pnpm run dev
pnpm run build
```

## 🧱 Repository Structure

| Path | Purpose |
| --- | --- |
| `.github/workflows/` | CI and release automation. |
| `benchmark/` | Offline context-policy benchmark harness and pricing data. |
| `docs/` | Astro + Starlight documentation site. |
| `scripts/` | Linux/macOS and Windows installers. |
| `src/` | Rust runtime, TUI, gateway, capabilities, sandbox, MCP, and providers. |
| `LICENSE.txt` | Apache-2.0 license text. |

## 🚢 CI And Releases

CI validates Rust formatting, repository metadata, docs builds, cross-target
checks, native tests, and sandbox smoke coverage. Release tags matching `v*`
build archive checksums for supported macOS, Linux, and Windows targets.

## License

DuckAgent is licensed under Apache-2.0. See [LICENSE.txt](LICENSE.txt).
