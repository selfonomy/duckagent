---
title: Cache-Friendly Prompt
description: Why DuckAgent treats prompt construction as append-only and cache-friendly.
draft: false
---

DuckAgent's prompt architecture is designed around a stable prefix. The first model message is a single system prompt. Runtime state is then added as structured dynamic context instead of rewriting that first message whenever profile, memory, capability, or sandbox state changes.

This is the idea behind **append-only prompt**:

- keep the system prompt stable;
- append or project dynamic context after the stable prefix;
- keep session and memory records append-only on disk;
- summarize old tool-heavy loops into recoverable prompt blocks instead of mutating historical records;
- preserve handles so exact evidence can be fetched again without reloading huge raw outputs.

## What stays stable

The system prompt defines the invariant runtime protocol: one native tool, `call_capability`, and the rules for how the MainAgent uses runtime capabilities. It should not need to change just because a profile enables a new gateway channel, updates `SOUL.md`, adds memory, or switches sandbox presets.

Keeping that prefix stable makes provider prompt caching more effective. A later turn can reuse the same early prompt bytes while only the later dynamic context changes.

## What is appended or projected

| Surface | Prompt behavior |
| --- | --- |
| Available capabilities | The system prompt says to use `call_capability`; the active capability names are injected as dynamic context. |
| `SOUL.md` | Loaded as profile context, not as a second system prompt. |
| `USER.md` | Loaded as user-profile context after avatar and soul context. |
| Avatar card | SillyTavern card metadata becomes a structured dynamic block for the active profile. |
| Memory | Durable memory records remain separate; the active memory catalog is projected into dynamic context when relevant. |
| Sandbox | The active sandbox summary is injected as runtime context so the model can see current boundaries. |
| Tool history | Completed tool-heavy loops are projected into recoverable summaries before model send. |

This keeps user-visible behavior flexible without constantly rewriting the front of the prompt.

## Why append-only does not mean unbounded

Append-only storage and append-only prompt design still need projection. Long-running sessions would otherwise resend too much raw evidence. DuckAgent combines append-only records with the guarded recoverable context policy:

- recent active work stays raw while it fits;
- old completed loops become `[COMPLETED AGENT LOOP SUMMARY]` blocks;
- large tool outputs become `[TOOL RESULT SUMMARY]` blocks with recovery handles;
- exact file evidence gets small budgets before falling back to path and range handles.

The result is cache-friendly and recoverable: the stable prefix remains stable, the dynamic suffix stays bounded, and exact details can still be requested when needed.
