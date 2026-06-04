---
title: Scheduled Tasks
description: Durable scheduled reminders and automations managed by DuckAgent capabilities.
draft: false
---

DuckAgent exposes cron-style scheduled tasks as built-in capabilities. The scheduler is program-internal: it runs inside the long-lived DuckAgent service process and does not create operating system cron entries.

Scheduled tasks are real Agent work, not passive notifications. When a task becomes due, DuckAgent writes run state, injects a scheduled event into the target session, and starts a normal Agent loop. That loop can call capabilities, read the prior conversation, update files, ask for approvals, and send a final user-facing reply through the same session or gateway route.

## What It Supports

Scheduled tasks can represent reminders and automations:

| User request | Capability shape |
| --- | --- |
| "Remind me in five minutes to buy groceries." | `cron_create` with a `once` schedule using an absolute RFC3339 timestamp. |
| "Every day at 8 AM, summarize what we talked about before." | `cron_create` with a `daily` schedule at `08:00` and an agent prompt. |
| "Pause that reminder." | `cron_pause` for the job id. |
| "Change it to 9 AM." | `cron_update` with the latest job revision. |

The model receives a `CURRENT TIME` dynamic context block on every request. It uses that block to convert relative scheduling requests into durable schedules. For example, "in five minutes" becomes a `once` schedule with an absolute RFC3339 timestamp, while "every morning at 8" becomes a `daily` schedule with `time: "08:00"` and `timezone: "local"` unless the user named another timezone.

## Capabilities

| Capability | Purpose |
| --- | --- |
| `cron_create` | Create a durable scheduled task. |
| `cron_list` | List jobs with next run time, revision, and recent run state. |
| `cron_get` | Read one job by id. |
| `cron_update` | Append a new job revision. |
| `cron_delete` | Append a tombstone; old records remain in JSONL. |
| `cron_pause` | Disable a job without deleting it. |
| `cron_resume` | Re-enable a paused job. |

Typical capability shapes:

```json
{
  "name": "Morning conversation summary",
  "schedule": {
    "kind": "daily",
    "time": "08:00",
    "timezone": "local"
  },
  "prompt": "Summarize what we talked about before and send the user a concise update."
}
```

```json
{
  "name": "Buy groceries reminder",
  "schedule": {
    "kind": "once",
    "at": "2026-06-01T08:05:00+08:00"
  },
  "prompt": "Remind the user to buy groceries."
}
```

## Storage

Cron state lives under the active profile:

```text
~/.duckagent/profiles/<name>/cron/jobs.jsonl
~/.duckagent/profiles/<name>/cron/runs.jsonl
```

Both files are append-only.

- `jobs.jsonl` records create, update, pause, resume, and delete events.
- `runs.jsonl` records started, finished, failed, and skipped runs.
- Deletes are tombstones; old records remain available for audit and recovery.
- Updates use revisions so stale changes can be rejected instead of silently replacing newer state.
- The replayed state is derived from the JSONL log; old rows are not edited in place.

## Scheduling Model

The scheduler keeps one program timer armed for the nearest enabled task. When the timer fires, it runs due jobs, records run state, recomputes the next due time, and rearms the timer.

Supported schedule kinds:

| Kind | Fields |
| --- | --- |
| `once` | `at`, an RFC3339 timestamp. |
| `interval` | `every_seconds`, with optional `anchor`. |
| `daily` | `time` plus optional `timezone`. |
| `weekly` | `weekdays`, `time`, and optional `timezone`. |

Timezone values support `local`, `UTC`, and fixed offsets like `+08:00`. Named timezone strings are accepted as labels and evaluated with the process-local timezone.

Each job also has execution policy:

| Policy | Default | Meaning |
| --- | --- | --- |
| `overlap` | `skip` | If the previous run is still active, skip the new due run instead of running the same job concurrently. `parallel` allows overlap. |
| `missed_run` | `run_once` | If DuckAgent was not running when a task became due after the job was created, run the latest missed occurrence once. `skip` advances to the next future occurrence. |
| `timeout_seconds` | `1800` | Mark an asynchronous run as failed if it does not finish before the timeout. |

## Service Runtime

The scheduler starts with:

```bash
duck gateway service start
```

Tasks fire while that service is running. If the service is stopped, no OS-level cron entry wakes DuckAgent. On the next service start, missed jobs follow their `missed_run` policy.

A task can target a normal session, but gateway-created sessions can also recover their channel route so the final response is delivered back to the same conversation after a service restart.

Approvals are not bypassed. If a scheduled task needs approval and the target route is available, the approval prompt is delivered through that channel. If no route exists, the approval request is denied rather than silently escalating permissions.

If the target session is already running an Agent loop when a scheduled event fires, DuckAgent queues the cron event as a separate user input for that session. It does not merge the scheduled task into an unrelated user turn, and it keeps the cron-specific approval provider with that queued input.
