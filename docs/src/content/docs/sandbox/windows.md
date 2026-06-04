---
title: Windows Setup
description: Understand elevated Windows sandbox setup and fail-closed behavior.
draft: false
---

On Windows, non-`danger` presets require elevated setup. DuckAgent creates dedicated sandbox users, configures ACL boundaries, and installs Windows Firewall or WFP network rules.

If setup is missing, stale, or mismatched with the active preset, DuckAgent fails closed instead of silently running weaker policy.

## Normal user flow

Users do not need to remember setup commands for normal first run.

1. Run `duck`.
2. Complete provider and model setup if needed.
3. DuckAgent checks whether the active sandbox can be enforced.
4. If Windows setup is required, DuckAgent prompts with three choices:
   - set up the default sandbox with Administrator permissions;
   - run without sandbox by switching to `danger`;
   - quit without changing sandbox state.

## Manual commands

These commands are for preflight, repair, CI images, and troubleshooting:

```bash
duck sandbox windows-setup-status
duck sandbox setup-windows
```

`windows-setup-status` prints whether the setup marker exists and where it is stored. `setup-windows` runs the elevated setup helper.

## What setup prepares

- Dedicated sandbox users.
- Filesystem ACL boundaries for the selected sandbox style.
- Windows Firewall or WFP network rules.
- Proxy support when the selected preset uses `network.mode = "proxy"`.

If the active preset changes in a way that requires different enforcement, DuckAgent checks the setup again and fails closed when it cannot safely apply the requested policy.
