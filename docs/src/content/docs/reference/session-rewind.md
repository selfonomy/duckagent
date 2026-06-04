---
title: Session Rewind
description: Use slash commands to start sessions, resume sessions, and rewind turns with tracked file restore.
draft: false
---

DuckAgent session control is exposed through slash commands in both the TUI and Gateway channels.

| Command | Behavior |
| --- | --- |
| `/new [title]` | Start a new session for the current TUI or Gateway route. |
| `/resume` | List recent sessions for the current chat. |
| `/resume <number>` | Resume one session from the latest `/resume` list. |
| `/rewind` | List user-turn rewind points in the current session. |
| `/rewind <number>` | Rewind the visible session history to before that user turn. |

Rewind is append-only. DuckAgent does not edit old session JSONL rows. It appends a `rewound` event containing replacement visible history, restored-file metadata, and warnings.

## File Restore

When built-in file mutation tools run inside a session, DuckAgent records file snapshots:

| Tool | Snapshot behavior |
| --- | --- |
| `write_file` | Captures the old file state before writing full content. |
| `edit` | Captures the old file state before exact string replacement. |
| `apply_patch` | Captures each touched file before patch operations. |

`/rewind <number>` restores snapshots recorded after the selected user turn, in reverse order:

- If a file existed before the tool call, DuckAgent restores the backed-up bytes.
- If a file was newly created by the tool call, DuckAgent deletes it.
- If the current file no longer matches the recorded after-change checksum, DuckAgent skips that restore and reports a warning.

This checksum guard prevents `/rewind` from overwriting later manual edits, editor saves, shell changes, or another Agent turn that changed the same file.

## Limits

Rewind is available after the current Agent loop finishes. It is not run while a turn is still active.

File restore only covers mutations recorded by the built-in `write_file`, `edit`, and `apply_patch` capabilities. It does not roll back shell command side effects, process output, external editor changes, database writes, network effects, or arbitrary files that were never snapshotted.

Backups live under the session runtime directory and are checked with SHA-256 before restore. Missing backups, directories where files are expected, and checksum mismatches are reported as warnings.
