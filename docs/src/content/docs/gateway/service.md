---
title: Gateway Service
description: Start, observe, and stop the DuckAgent gateway service.
draft: false
---

Gateway service commands operate on the active profile and its gateway configuration.

## Commands

```bash
duck gateway service start
duck gateway service log
duck gateway service stop
```

`start` reads profile config, creates enabled channel adapters, and starts the required listener, polling, websocket, or bridge tasks. If there is no usable configuration, DuckAgent should guide the user into setup.

The gateway service also hosts DuckAgent's program-internal scheduled task runner. Scheduled tasks do not install OS cron entries; the service process reads the active profile's cron JSONL state, arms the nearest timer, and injects due tasks into the target session. A scheduled task can start a normal Agent loop and send a final reply through the same gateway conversation.

`log` tails gateway-routed sessions. It reads `gateway/state/sessions.jsonl`, follows new mappings, and shows messages and final assistant replies that arrive after log startup.

`stop` stops the running profile gateway service.

## Configuration-driven startup

Gateway service start is intentionally config-driven. Users should not need to pass `--bind` or `--channel` for normal operation. Channel adapters derive their runtime from profile `config.json` and secrets from `auth.json`.

## State paths

```text
~/.duckagent/profiles/<name>/gateway/state/sessions.jsonl
~/.duckagent/profiles/<name>/gateway/attachments/
~/.duckagent/profiles/<name>/cron/jobs.jsonl
~/.duckagent/profiles/<name>/cron/runs.jsonl
~/.duckagent/profiles/<name>/sessions/
```

`sessions.jsonl` maps gateway conversations to real DuckAgent session ids. The full conversation remains in the normal session directory.

`cron/jobs.jsonl` and `cron/runs.jsonl` are append-only scheduled-task stores. Jobs created from gateway conversations can use the session mapping to deliver scheduled results back to the original channel.

If the service is stopped, scheduled tasks do not fire in the background. On the next `duck gateway service start`, the scheduler replays `jobs.jsonl` and `runs.jsonl`, applies each job's missed-run policy, and continues from the durable state.

## Operational notes

- Gateway sessions must be append-only.
- Scheduled task job and run logs must be append-only.
- Scheduled tasks use the same Agent loop and approval policy surfaces as user-triggered work.
- Group, channel, thread, and topic identifiers must not share a session by accident.
- Inbound messages should be deduplicated when platforms provide event ids.
- Attachments should respect configured media limits.
- Approval state belongs in gateway core, while channel adapters translate platform-specific interactions into approval commands.
