# Desktop agent

`lighthouse-node` connects Claude Code on your machine to Lighthouse. You see your sessions in the apps and can answer
permission prompts from your phone.

It uses official Claude Code hooks only. It reads no transcripts and opens no public port: hooks reach it on the loopback
address, and it talks to your server over an outbound connection.

## Setup

Create a token with the kind `node` in the apps, or with `POST /auth/tokens`. Endpoints under `/node/` accept only such
tokens and answer `403` to every other token. Then:

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

## Session details

The hooks also report what a session does. `GET /sessions/{node}/{id}` returns:

| Field | From |
|---|---|
| `todos` | The latest `TodoWrite` call, the session's own todo list |
| `plan` | The latest `ExitPlanMode` call, the plan the session presented |
| `files` | Files changed with `Edit`, `MultiEdit`, `Write` and `NotebookEdit`, with the latest change as a diff built from the tool input |
| `timeline` | The latest 200 events: prompts, tool calls with a one-line summary such as the command, file changes and stops |
| `messages` | Messages you sent to the session |

Events reach the server in the background and never hold up a hook. Prompts and summaries are cut at 300 characters,
diffs at 20,000.

## Messages to a session

`POST /sessions/{node}/{id}/messages` with `{"text": "..."}` queues a message. The next time the session stops, the
`Stop` hook answers with `"decision": "block"` and your message as the reason, which is the official way to let Claude
Code go on with a new instruction. The session then continues by itself.

A session that already stopped and waits at its prompt runs no more hooks until someone types, so a message reaches it
only at its next stop. To reach such sessions, set `stop_hold_seconds` (at most 60) in `~/.config/lighthouse/node.toml`.
Every stopped session then waits that long for a message before it hands control back to the terminal. While it waits,
the terminal shows the session as busy. A paused machine hands over no messages.

## Costs and tokens

Claude Code exports metrics through OpenTelemetry. Point it at your server to see cost and tokens per session:

```sh
export CLAUDE_CODE_ENABLE_TELEMETRY=1
export OTEL_METRICS_EXPORTER=otlp
export OTEL_EXPORTER_OTLP_PROTOCOL=http/json
export OTEL_EXPORTER_OTLP_METRICS_ENDPOINT=https://lighthouse.example/node/otel/v1/metrics
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer $LIGHTHOUSE_NODE_TOKEN"
```

The server reads `claude_code.cost.usage` (USD) and `claude_code.token.usage` by type: input, output, cache read and
cache creation. `GET /sessions/{node}/{id}` shows them as `usage`. Increments add up and running totals replace, so
both temporalities work.

## What the official interfaces cannot do

| Wish | Why not, or not yet |
|---|---|
| See every tool call before it runs | Only `PostToolUse` is reported, and the permission hook fires only when a prompt would appear |
| Changes made through the shell | Files changed by `Bash`, such as `git checkout` or `sed`, are not visible. Diffs come from the tool input, not from Git |
| Text and reasoning of the model | Only the last assistant message at a stop arrives. The agent does not read transcripts, by design |
| Type into an idle session at any time | Hooks only run during a turn. `stop_hold_seconds` is the closest official way |
| Start a new turn from outside | Possible with headless mode (`claude -p --resume <id>`), but it would run a second process on the same session next to your terminal. Not done |
| Remote Control | A separate official product with no interface for other programs |
| Context window fill | Claude Code exports tokens, not how full the context window is. `usage` shows the tokens |

## Kill switch

`lighthouse-node stop`, or pausing the machine in the apps, denies every open request and all new ones until you resume
in the apps.

## Security model

The agent reaches your own server directly over TLS, so tool arguments of permission requests are visible to that server.
This fits a personal instance, where you run the server. End-to-end encryption through a relay becomes necessary only for
a hosted service and is not part of this version.

Always use HTTPS for a server outside your own machine, since the token travels with every request.
