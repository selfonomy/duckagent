---
title: Filesystem Tools
description: Read, search, write, edit, and patch files under sandbox policy.
draft: false
---

Filesystem capabilities let DuckAgent inspect and change workspace files when policy allows it.

Common actions include:

- Read a file.
- Search file names.
- Search file contents.
- Write a file.
- Edit a file.
- Apply a patch.

Path-aware filesystem actions can trigger `AGENTS.md` discovery for the target path. Sandbox filesystem rules still decide whether the operation is allowed, needs approval, or is denied.

## Rewind Snapshots

When `write_file`, `edit`, or `apply_patch` changes a file inside a session, DuckAgent records a before/after snapshot entry in the session log. The user can later run `/rewind` to list rewind points and `/rewind <number>` to return to before a user turn.

Tracked file changes are restored only when the current file still matches the recorded after-change checksum. If the file was edited later by a user, shell command, editor, or another Agent turn, DuckAgent skips that restore and reports a warning instead of overwriting the newer state.

## User expectation

You usually do not need to configure filesystem tools directly. Configure the [Sandbox](/sandbox/) when you want to change where tools may read or write.
