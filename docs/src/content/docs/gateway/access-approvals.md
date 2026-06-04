---
title: Access And Approvals
description: Configure Gateway allowlists, pairing, mention gating, and approval commands.
draft: false
---

Gateway has two separate safety layers: which external messages may enter DuckAgent, and who can approve risky actions after a message has entered.

## Access fields

Common channel fields include:

| Field | Purpose |
| --- | --- |
| `allowed_users` | Sender ids that may use the channel. |
| `allowed_chats` | Group, room, channel, topic, or conversation ids that may route to DuckAgent. |
| `dm_access` | Direct-message access mode. |
| `group_access` | Group or channel access mode. |
| `require_mention` | Require a mention or wake pattern in noisy rooms. |
| `pairing` | Require owner approval before unknown users can chat. |

## Direct-message modes

| Mode | Meaning |
| --- | --- |
| `open` | Reachable DM users can enter DuckAgent. |
| `allowlist` | Only `allowed_users` can enter. |
| `pairing` | Unknown DM users receive a one-time pairing code that must be approved. |
| `disabled` | DMs do not enter DuckAgent. |

Publicly reachable channels should prefer `pairing` or `allowlist`.

## Group modes

| Mode | Meaning |
| --- | --- |
| `mention` | Messages require a bot mention or wake pattern. |
| `open` | Group messages enter by default. |
| `allowlist` | Only `allowed_chats` can enter. |
| `disabled` | Group messages do not enter DuckAgent. |

Mention gating reduces noise. It is not a security boundary by itself.

## Approval commands

When a tool needs approval, Gateway prefers native buttons, cards, or interaction callbacks. If a platform cannot provide that, users can send text commands:

```text
/approve
/approve all
/approve <approval-id>
/deny
/deny all
/deny <approval-id>
```

If the current chat has pending approvals and the user sends a normal new message, DuckAgent denies the pending approvals for that route before treating the message as new input.

Channel adapters translate platform-specific interactions into these shared approval commands. Gateway core owns matching, state updates, and policy decisions.
