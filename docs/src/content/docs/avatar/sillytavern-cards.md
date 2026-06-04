---
title: SillyTavern Cards
description: Add SillyTavern PNG character cards with DuckAgent profiles.
draft: false
---

Use a SillyTavern PNG character card by adding it as a DuckAgent profile avatar.
The command you usually want is:

```bash
duck profiles
```

`duck profiles` opens the interactive profile manager. From there you can create
a profile, switch the default active profile, and import an avatar or
SillyTavern PNG card while creating a profile.

## Add a card from the profile manager

Run:

```bash
duck profiles
```

Then:

1. Choose `Add Profile`.
2. Enter a profile name, for example `ada`.
3. At `Avatar image`, enter the SillyTavern PNG card path or an `http(s)` URL.
4. Optionally enter one-line `SOUL.md` and `USER.md` seeds.
5. Finish the flow. The new profile becomes the active profile.

Example `Avatar image` values:

```text
~/Downloads/ada.character.png
/Users/me/cards/ada.png
https://example.com/cards/ada.png
```

The imported card is saved as:

```text
~/.duckagent/profiles/ada/avatar.png
```

On the next model request with that profile, DuckAgent parses the PNG metadata
and uses it as profile context.

## Replace a card on an existing profile

For an existing profile, replace:

```text
~/.duckagent/profiles/<profile-name>/avatar.png
```

For example:

```text
~/.duckagent/profiles/ada/avatar.png
```

If a previous cache exists next to it:

```text
~/.duckagent/profiles/ada/avatar.json
```

you can delete the cache. DuckAgent will rebuild it from `avatar.png` the next
time the profile is used.

## Use a profile for one run

To use a profile only for the current process:

```bash
duck --profile ada
```

This does not change the saved default profile. To switch the default profile,
run:

```bash
duck profiles
```

and select the profile in the picker.

## Supported commands

Supported today:

```bash
duck profiles
duck profile
duck --profile ada
```

`duck profiles` and `duck profile` open the same interactive profile manager.

These non-interactive subcommands do not exist yet:

```bash
duck profiles add ada --avatar ~/Downloads/ada.png
duck profile add ada
duck profiles list
duck profiles use ada
```

## How the card is parsed

DuckAgent supports SillyTavern PNG cards stored at:

```text
~/.duckagent/profiles/<name>/avatar.png
```

When the PNG contains a `tEXt` chunk named `ccv3` or `chara`, DuckAgent decodes
the embedded character-card JSON and caches it as:

```text
~/.duckagent/profiles/<name>/avatar.json
```

`ccv3` is preferred when both chunks exist. If `avatar.png` changes, stale
`avatar.json` cache data is ignored and rebuilt.

## How it is used

The card becomes the `[AVATAR CARD]` dynamic context block for the active
profile. It is injected before `SOUL.md`, `USER.md`, and the user message.

DuckAgent currently includes these fields when present:

- `name`
- `description`
- `personality`
- `scenario`
- `system_prompt`
- `post_history_instructions`
- `first_mes`
- `mes_example`
- enabled Character Book entries with `constant = true`

The card does not replace DuckAgent's real system prompt. It is profile context
added after the stable system prompt so prompt-cache prefixes stay stable.

## Plain avatars

A PNG without SillyTavern metadata is treated as a normal avatar image. Invalid
card metadata is ignored instead of blocking the profile.

The avatar importer also accepts `jpg`, `jpeg`, `webp`, and `gif`, but those
formats are visual avatars only; they do not carry SillyTavern card metadata.

Avatar files must be at most 20 MiB.
