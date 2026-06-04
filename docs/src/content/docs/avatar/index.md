---
title: Overview
description: Understand DuckAgent profiles, identity files, avatar files, SillyTavern cards, and AGENTS.md instructions.
draft: false
---

DuckAgent treats identity as a first-class user feature. A profile can carry its own model config, credentials, memory, skills, gateway channels, persona files, avatar image, and SillyTavern character card.

## What belongs here

| Page | Use it for |
| --- | --- |
| [Profiles](/avatar/profiles/) | Create profiles, switch defaults, and understand profile directories. |
| [SOUL.md](/avatar/soul/) | Define the agent's durable persona, tone, and boundaries. |
| [USER.md](/avatar/user/) | Define durable information about the user and collaboration preferences. |
| [Avatar Files](/avatar/avatar-files/) | Add or replace `avatar.png`, `avatar.jpg`, `avatar.webp`, or `avatar.gif`. |
| [SillyTavern Cards](/avatar/sillytavern-cards/) | Use embedded PNG character-card metadata as profile context. |
| [AGENTS.md Instructions](/avatar/agents-md/) | Add profile, project, and path-local operating instructions. |

## Context order

Profile identity context is injected before the current user message in this order:

1. `[AVATAR CARD]`
2. `[SOUL]`
3. `[USER]`
4. `[USER MESSAGE]`

This keeps the system prompt stable while still making the active profile feel consistent.
