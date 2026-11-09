# Desktop agent

`lighthouse-node` connects Claude Code on your machine to Lighthouse. You see your sessions in the apps and can answer
permission prompts from your phone.

It uses official Claude Code hooks. It opens no public port: hooks reach it on the loopback
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

Events reach the server in the background and never hold up a hook. One-line summaries of tool calls are cut at 300
characters, prompts at 4,000, and the answer at a stop and diffs at 20,000.

## What a session says

Hooks carry only the last answer at a stop. The texts Claude writes while it works, between its tool calls, sit in the
session's transcript under `~/.claude/projects`. The agent reads the new lines of that file at every tool call and stop
and sends the text blocks of the main session as `say` events, at most 20,000 characters each. It never sends tool input
or output, thinking, subagent messages or the history from before it first saw the session. The apps show them in the
session's timeline as Claude writes them. Turn it off with `replies = false` in `node.toml`.

## What the agent finds on the machine

When it starts and every five minutes, the agent looks at the Claude Code folder of its user and reports:

| What | Sent | Where it shows |
|---|---|---|
| Sessions: the transcripts directly in `~/.claude/projects/<folder>/` | Project, folder, branch, the first prompt (200 characters), message count and last activity. Never the conversation | `GET /sessions/history`; the apps resume one with a launch |
| Skills: `~/.claude/skills/*/SKILL.md` and the `.claude/skills` of every project a session worked in | Name, description and folder | `GET /sessions/skills`; the apps schedule one as an automation that launches it |
| Memories: `~/.claude/projects/<folder>/memory/*.md` | The files of every project whose memories changed, with secrets redacted on the machine | One Brain entry per project; memories about you become proposed facts |

Subagent transcripts and the folders of temporary directories are skipped. Transcripts are read only where they grew,
so a scan after the first is quick. Turn the memories off with `memory_sync = false` in `node.toml`.

## Messages to a session

`POST /sessions/{node}/{id}/messages` with `{"text": "..."}` queues a message. A session that is working gets it with
its next tool call: the `PostToolUse` hook adds it as context, so Claude reads it within seconds and goes on with it.
A prompt typed in the terminal carries waiting messages along the same way. When the session stops first, the `Stop`
hook answers with `"decision": "block"` and your message as the reason, the official way to let Claude Code go on with a
new instruction.

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
| Reasoning of the model | Only the texts Claude writes to you arrive, see [What a session says](#what-a-session-says). Thinking stays on the machine |
| Type into an idle session at any time | Hooks only run during a turn. `stop_hold_seconds` is the closest official way |
| Remote Control | A separate official product with no interface for other programs |
| Context window fill | Claude Code exports tokens, not how full the context window is. `usage` shows the tokens |

## Context across sessions and compaction

The agent also installs the `PreCompact` hook, and answers `SessionStart` with what Lighthouse knows. Claude Code calls
`SessionStart` when a session starts, resumes, is cleared and right after a compaction. The agent then asks
`GET /node/sessions/{id}/context?cwd=&source=` and hands the Markdown to Claude Code as `additionalContext`:

| Part | Content | `startup` |
|---|---|---|
| This session | Status and latest summary, plan, todos, the owner's latest prompts and messages, changed files | Left out |
| Second Brain | Entries that link `project:<name>` or `session:<id>`, and the best search hits for the project name | Yes |
| Open tasks | Up to ten open tasks that name the project in their title or notes | Yes |

The project name is the last part of the working directory, as in the session list. The block is cut at 8,000
characters. It also points the session at the MCP tools `search_brain` and `save_to_brain` (see [api.md](api.md#mcp)),
so decisions and the state of work outlive the session. When the server is slow or answers with an error, the session
starts without the block.

## Launches

Lighthouse can start Claude Code sessions on this machine and continue them, for work that runs long or while you are
away. This lets the server run Claude Code on your computer, so it is off until you allow it in `node.toml`:

```toml
launch = true
launch_roots = ["~/code"]   # launched sessions may only work below these directories
claude = "claude"           # the Claude Code binary, optional
```

`POST /sessions/launch` with `{"node", "cwd", "prompt", "resume", "model"}` queues a launch. `node` is the machine's ID
or its name; a name that several machines share picks the one seen last. Automations start launches with a
[`launch` step](automations.md#steps). The agent takes it through `GET /node/launches`, checks that `cwd` lies below one
of the roots after resolving links, and runs `claude -p <prompt> --output-format json` in that directory, with
`--resume <id>` and `--model` when given. A new session gets its ID up front with `--session-id`, so the launch shows
it while it runs. At most three run at once. It reports `running`, then `succeeded` or `failed` with the final answer
(cut at 20,000 characters), the session ID and the costs. The session's own hooks report it like any other session, and its permission prompts reach
your devices as usual. A directory outside the roots fails at once.

`DELETE /sessions/launches/{id}` cancels a queued launch at once. A running one is stopped by the agent with its next
poll, within 30 seconds, and ends as `cancelled`.

A message to a session Lighthouse launched that has already ended resumes it: instead of waiting for a stop that never
comes, the server queues a launch with `--resume` and the message as prompt.

## Kill switch

`lighthouse-node stop`, or pausing the machine in the apps, denies every open request and all new ones until you resume
in the apps.

## Security model

Launches are opt-in per machine and limited to `launch_roots`. Anyone with your sign-in can start Claude Code there
with your Claude Code settings and permissions, so keep the roots narrow and the permission prompts on.


The agent reaches your own server directly over TLS, so tool arguments of permission requests are visible to that server.
This fits a personal instance, where you run the server. End-to-end encryption through a relay becomes necessary only for
a hosted service and is not part of this version.

Always use HTTPS for a server outside your own machine, since the token travels with every request.
