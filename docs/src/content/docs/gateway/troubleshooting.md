---
title: Troubleshooting
description: Troubleshoot Gateway setup, service startup, credentials, bind ports, and routing.
draft: false
---

## Service starts setup again

The active profile does not have a usable Gateway config or a configured channel is missing credentials. Run:

```bash
duck gateway channels
```

and reconfigure the channel.

## Credential missing

Channel config lives in `config.json`, but secrets live in `auth.json`. If config was copied manually, the matching auth entry may be missing.

## Callback channel does not receive events

Webhook and callback channels need a stable bind and a reachable public or private reverse-proxy URL. Confirm the platform is pointed at the endpoint shown during setup.

## Log shows no sessions

`duck gateway service log` only follows gateway-routed sessions. Send a new message through a configured channel after starting the log command.

## Wrong conversation resumes

Check whether the channel, conversation id, and thread/topic id are all present in the platform event. Gateway route keys must not collapse multiple threads into one session.
