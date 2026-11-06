# Desktop agent

`lighthouse-node` connects Claude Code on your machine to Lighthouse. You see your sessions in the apps and can answer
permission prompts from your phone.

It uses official Claude Code hooks only. It reads no transcripts and opens no public port: hooks reach it on the loopback
address, and it talks to your server over an outbound connection.

## Setup

Create a token with the kind `node` in the apps, or with `POST /auth/tokens`. Then:

```sh
export LIGHTHOUSE_NODE_TOKEN=lht_...
lighthouse-node pair --server https://lighthouse.example --name "MacBook"
lighthouse-node install     # adds hooks to ~/.claude/settings.json, keeps a backup
lighthouse-node run
```

`lighthouse-node uninstall` removes exactly these hooks. `lighthouse-node status` shows what the agent knows.

## Permission requests

When Claude Code would show a permission prompt, the hook holds the request open and forwards it to the server. The
first decision from any device wins.

| Situation | Answer to Claude Code |
|---|---|
| Allowed | `allow` |
| Denied, or the machine is paused | `deny` |
| No decision in time, or the server cannot be reached | No decision |

No answer ever allows a command by itself. The [spike](spikes/permission-hook.md) shows that headless sessions then fail
closed. Whether an interactive session falls back to its terminal prompt still needs a check in a real terminal.

The hook only fires when a prompt would appear. Commands your settings already allow never reach it.

## Kill switch

`lighthouse-node stop`, or pausing the machine in the apps, denies every open request and all new ones until you resume
in the apps.

## Security model

The agent reaches your own server directly over TLS, so tool arguments of permission requests are visible to that server.
This fits a personal instance, where you run the server. End-to-end encryption through a relay becomes necessary only for
a hosted service and is not part of this version.

Always use HTTPS for a server outside your own machine, since the token travels with every request.
