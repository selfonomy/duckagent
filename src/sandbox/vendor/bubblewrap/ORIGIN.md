# Bubblewrap Vendor Origin

This directory vendors `bubblewrap` source code for duckagent's Linux sandbox fallback.

- Upstream: https://github.com/containers/bubblewrap
- Version: 0.11.2
- Release date: 2026-04-23
- License: LGPL-2.0-or-later, see `COPYING`

duckagent compiles this source through `build.rs` and renames the C entrypoint to `duckagent_bwrap_main` so it can be used as an embedded fallback when a system `bwrap` executable is unavailable.
