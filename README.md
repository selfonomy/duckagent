# DuckAgent

<p align="center">
  <img src="src/default/favicon.png" alt="DuckAgent logo" width="96" height="96">
</p>

<p align="center">
  <a href="https://github.com/selfonomy/duckagent/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/selfonomy/duckagent/ci.yml?branch=main&style=for-the-badge&label=CI&logo=github" alt="CI"></a>
  <a href="https://selfonomy.github.io/duckagent/"><img src="https://img.shields.io/badge/Docs-GitHub%20Pages-0ea5e9?style=for-the-badge&logo=github" alt="Documentation"></a>
  <a href="LICENSE.txt"><img src="https://img.shields.io/badge/License-Apache--2.0-green?style=for-the-badge" alt="License: Apache-2.0"></a>
</p>

DuckAgent is a Rust-native local AI agent runtime. The default entrypoint is:

```bash
duck
```

It brings the local TUI, model management, profiles, SillyTavern avatar cards,
durable memory, skills, gateway channels, sandbox policy, MCP, and Web
Search/Extract into one runtime.

## Quick Install

Linux, macOS, and WSL2:

```bash
curl -fsSL https://raw.githubusercontent.com/selfonomy/duckagent/main/scripts/install.sh | bash
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/selfonomy/duckagent/main/scripts/install.ps1 | iex
```

After installation:

```bash
duck
```

Full docs: [GitHub Pages](https://selfonomy.github.io/duckagent/)

## Highlights

- **Rust-native Agent Runtime**: MainAgent calls files, processes, search, MCP,
  Skills, and other runtime capabilities through the stable `call_capability`
  interface.
- **Cache-friendly append-only prompt**: one stable system prompt stays first;
  capabilities, `SOUL.md`, `USER.md`, avatar cards, memory, sandbox state, and
  projected history are injected after that stable prefix.
- **Append-only sessions**: session JSONL is never rewritten. Rewind,
  compaction, memory changes, and runtime changes are represented as appended
  events or model-visible projections.
- **Slash-command rewind**: `/rewind` lists user turns and can restore tracked
  file changes from `write_file`, `edit`, and `apply_patch` snapshots when the
  current file state still matches the recorded after-state.
- **Profiles**: each profile owns its model config, credentials, memory, skills,
  gateway config, `SOUL.md`, `USER.md`, optional `AGENTS.md`, and avatar files.
- **SillyTavern card support**: `avatar.png` can be a SillyTavern PNG card.
  DuckAgent extracts embedded character metadata and injects it as dynamic
  context.
- **Gateway**: connect the same Agent Loop to Telegram, Slack, Signal, Matrix,
  Discord, API Server, Email, SMS, WhatsApp, and many other channels.
- **Scheduled tasks**: ask for reminders or recurring automations in normal
  language. The program-internal scheduler stores jobs as append-only JSONL and
  fires them from the long-running gateway service without installing OS cron
  entries.
- **Sandbox**: config-driven filesystem, network, environment, secret
  injection, tool approval, and shell approval policy.
- **Benchmarked context projection**: the current guarded recoverable context
  policy maps to the `recoverable_decay_guarded_mid` benchmark family and keeps
  exact recovery handles instead of blindly summarizing every tool result.
- **Web capability**: defaults to `web_search=exa` and `web_extract=local`,
  with optional local Chrome fallback for JavaScript-heavy pages.

## Benchmark And Context Projection

The benchmark suite under `benchmark/` compares long-running Agent Loop context
policies without calling an LLM or mutating real sessions.

The runtime currently uses `ContextProjectionPolicy::guarded_mid`, which maps
to the guarded recoverable policy shown in reports as
`duckagent_recoverable_decay_guarded_mid`.

The policy is cache-friendly:

- the system prompt remains a stable first message;
- available capabilities are injected as dynamic context for `call_capability`;
- `SOUL.md`, `USER.md`, avatar cards, memory, and sandbox state are appended or
  projected after the stable prefix;
- active tool output stays raw until prompt pressure appears;
- compressed tool history keeps recovery handles such as path, offset, limit,
  process id, cursor, mode, and query.

In `benchmark/results/guarded-mid-vs-balanced-combined/report.md`, guarded-mid
completed `1188/1188` simulated turns and saved roughly `108M` raw tool tokens
through structured projection. Against the balanced policy, the report shows
`22.6%` lower simulated cost on `openai-gpt-5.4`, `14.5%` lower on `kimi-k2.6`,
and `16.9%` lower on `deepseek-v4-flash`.

## Common Commands

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
/new [title]
/resume
/resume 2
/rewind
/rewind 3
```

`/rewind` appends a rewind event instead of rewriting session JSONL. When file
snapshots were recorded after the target turn, DuckAgent restores old file
contents or deletes newly-created files only if the current file still matches
the recorded post-change checksum.

## Configuration Location

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

## Documentation

The documentation site lives in `docs/` and uses Astro + Starlight:

```bash
cd docs
pnpm install
pnpm run dev
pnpm run build
```

## Repository Structure

| Path | Purpose |
| --- | --- |
| `.github/workflows/` | CI and release automation. |
| `benchmark/` | Offline context-policy benchmark harness and pricing data. |
| `docs/` | Astro + Starlight documentation site. |
| `scripts/` | Linux/macOS and Windows installers. |
| `src/` | Rust runtime, TUI, gateway, capabilities, sandbox, MCP, and providers. |
| `LICENSE.txt` | Apache-2.0 license text. |

## CI And Releases

CI validates Rust formatting, repository metadata, docs builds, cross-target
checks, native tests, and sandbox smoke coverage. Release tags matching `v*`
build signed archive checksums for supported macOS, Linux, and Windows targets.

## License

DuckAgent is licensed under Apache-2.0. See [LICENSE.txt](LICENSE.txt).
