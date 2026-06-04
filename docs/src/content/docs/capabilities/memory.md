---
title: Memory
description: Understand DuckAgent memory storage, review requests, and MemoryAgent editing.
draft: false
---

DuckAgent separates task execution from memory editing. MainAgent can request a memory review; MemoryAgent reads and edits memory records.

## Storage

```text
~/.duckagent/profiles/<name>/memories/global.jsonl
~/.duckagent/profiles/<name>/memories/workspaces/<workspace-id>.jsonl
```

Global memory follows the profile. Workspace memory is tied to the current project.

## What users need to do

Most users do not need to manage memory files manually. Use normal chat. DuckAgent can request review when durable preferences, project facts, or recurring constraints should be remembered.

Do not store secrets in memory.

## MemoryAgent capabilities

```text
get_memory
add_memory
patch_memory
forget_memory
```
