---
title: Skills
description: Create and use profile-local SKILL.md workflows.
draft: false
---

Skills are profile-local workflow bundles. They let DuckAgent load focused instructions only when a task needs them.

## Layout

```text
~/.duckagent/profiles/<name>/skills/<skill-name>/SKILL.md
```

A skill directory can include scripts, references, templates, assets, and other supporting files.

## SKILL.md

```md
---
name: my-skill
description: Use this skill when a request needs the workflow it describes.
---

# My Skill

Instructions for how the agent should perform the workflow.
```

The description should explain when to use the skill. The agent can then load the skill and read supporting files on demand.

## Runtime capabilities

```text
load_skill
read_skill_file
```
