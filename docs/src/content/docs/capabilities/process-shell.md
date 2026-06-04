---
title: Process & Shell
description: Run local processes and shell commands with sandbox and approval policy.
draft: false
---

Process and shell capabilities let DuckAgent run local commands for development, testing, build verification, and automation.

The sandbox controls:

- Files the process can read or write.
- Ordinary network access.
- Inherited environment variables.
- Shell command allow, ask, or deny policy.
- Whether the agent can request additional access.

Process starts with an explicit working directory can also trigger path-local `AGENTS.md` discovery.

Shell selection is platform-specific. On Windows, DuckAgent uses `COMSPEC` when set and falls back to `cmd.exe /d /C`. On Unix-like systems, DuckAgent uses `$SHELL` only when it points to a supported POSIX-compatible shell, then falls back through available `zsh`, `bash`, and `sh` executables. `bash` and `zsh` run with `-lc`; `sh` and `dash` run with `-c`.

Use [Tool & Shell Permissions](/sandbox/tool-shell-permissions/) to understand approval behavior.
