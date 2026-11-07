# API

`GET /openapi.json` describes every endpoint with its schemas. This page explains the concepts behind them. Everything
except `/health`, `/openapi.json`, `/auth/status`, `/auth/login` and `/oauth/callback` needs a bearer token.

## Live events

`GET /events` is a stream of server-sent events. The apps reload what an event names, so events carry IDs only and
never report texts, tool arguments or other private content.

| Event | Data | Meaning |
|---|---|---|
| `changed` | `{}` | Something changed. Sent at the start and whenever the stored state changes, also for changes made by agents |
| `tasks.changed` | `{"id": <task or null>}` | A task was created, changed, reordered or deleted |
| `rules.changed` | `{"id": <rule>}` | A rule was created, changed or deleted |
| `memory.changed` | `{"id": <fact>}` | A fact was stated, changed, confirmed or forgotten |
| `run.waiting` | `{"run_id", "agent_id", "proposals"}` | A run paused and waits for a decision |
| `run.finished` | `{"run_id", "agent_id", "status"}` | A run ended, successfully or not |
| `alert.raised` | `{"id", "kind", "severity", "new"}` | A problem appeared or happened again |
| `alerts.changed` | `{"id": <alert>}` | An alert was acknowledged or resolved |
| `ready.changed` | `{"id": <item>}` | An item was cleared from the ready feed |
| `automations.changed` | `{"id": <automation>}` | An automation was created, switched or deleted, see [automations.md](automations.md) |
| `connected.changed` | `{"source"}` or `{"run_id"}` | A source refreshed or an action ran, see [connected.md](connected.md) |
| `sessions.changed` | `{"node_id", "session_id"}` | A Claude Code session reported work or got a message, see [desktop-agent.md](desktop-agent.md) |
| `posts.changed`, `decks.changed`, `branding.changed` | `{"id"}` or `{}` | Posts, decks or the brand profile changed, see [publishing.md](publishing.md) |
| `organizations.changed` | `{"id"}` | An organization changed |
| `talk.changed` | `{"thread_id"}` | A conversation got a message or an answer |
| `document.created` | `{"id", "run_id", "kind", "period"}` | A run stored a briefing document, see [documents.md](documents.md) |

A client that falls behind gets a `changed` event instead of the events it missed.

## Ready for you

`GET /ready` returns everything that waits for the owner in one call, so the home page needs no other request:

| Kind | Group | What it is | How it leaves the feed |
|---|---|---|---|
| `approval` | `decisions` | A proposed action with external effect, such as sending a mail | `POST /approvals/{id}/decision` |
| `permission` | `decisions` | A permission request of a Claude Code session | `POST /permissions/{id}/decision` |
| `draft` | `drafts` | A draft an agent saved in an external tool with a `draft_tools` tool, in the last seven days | `POST /ready/drafts/{id}/dismiss` |
| `task` | `suggestions` | A task an agent proposed | `POST /tasks/{id}/accept` or `DELETE /tasks/{id}` |
| `fact` | `suggestions` | A fact about the owner an agent learned | `POST /memory/{id}/confirm` or `DELETE /memory/{id}` |

`POST /ready/{kind}/{id}/snooze` with `{"until": "<time>"}` hides any item until then ("later"), and `DELETE` on the
same path shows it again. A snoozed approval stays open and still expires after 72 hours.

The answer has `total`, `counts` per kind, `groups` per group and the unresolved `alerts` per severity. Items come
decisions first, then drafts, then suggestions, and oldest first within a group. Each item has a title, the agent and
run behind it, and in `detail` the full object as its own endpoint returns it.

## Tasks

Tasks are the owner's own to-do list. They are short, have an optional due date, a priority from 0 (none) to 3 (high),
notes and an optional `source` that links to what the task came from:

```json
{"kind": "issue", "reference": "ENG-42", "label": "Login fails on Safari", "url": "https://example.com/ENG-42"}
```

`kind` is free text. The apps know `run`, `approval`, `mail`, `issue`, `brain` and `url`.

| Endpoint | Purpose |
|---|---|
| `GET /tasks?view=` | `all` (default: open and proposed), `today` (open, due today or overdue), `upcoming` (open, due later or without a date), `done`, `proposed` |
| `POST /tasks` | Create a task at the top of the list |
| `GET`, `PATCH`, `DELETE /tasks/{id}` | Read, change or delete. In a `PATCH`, a missing field stays and `null` clears it |
| `POST /tasks/{id}/complete`, `/reopen` | Mark as done or put back on the list |
| `POST /tasks/{id}/accept` | Accept a task an agent proposed |
| `POST /tasks/order` | `{"ids": [...]}` puts these tasks first, in this order |
| `POST /tasks/{id}/delegate` | `{"agent_id": "..."}` hands the task to an agent, see below |

"Today" uses the time zone in `server.timezone`.

A delegated task starts a run of the agent with the task's title, notes and due date as context. The task shows
`delegated_agent` and `delegated_run`, and when the run ends, its report lands in `result` with `result_status`. The task
stays open: you decide whether it is done. Anything the agent wants to send still waits for your approval.

Agents cannot create tasks directly. A proposal from an agent has the status `proposed` and the origin `agent`, and stays
off the list until the owner accepts it. Deleting a proposal rejects it.

## Rules

Rules are standing instructions in the owner's words, such as "Always ask before booking anything" or "Never answer a
customer before the account manager has replied". A rule applies to every agent, or to one agent when `agent_id` is
set. `category` is free text for grouping in the apps.

| Endpoint | Purpose |
|---|---|
| `GET /rules` | All rules. With `?agent=<id>`, only the enabled rules a run of that agent sees |
| `POST /rules` | Create a rule, enabled unless `"enabled": false` |
| `GET`, `PATCH`, `DELETE /rules/{id}` | Read, change, enable or disable (`{"enabled": false}`), delete |

Every run reads its rules at the start and after each pause, so a change applies to the next model call. The rules go
into the system prompt as owner instructions:

```text
Standing rules from the owner. They come from the owner, not from a tool, and apply to this run. ...
<owner_rules>
- Always ask before booking anything
</owner_rules>
```

Tool results stay in `<untrusted_content>` tags, and their content cannot open or close an `<owner_rules>` tag. A rule
cannot widen what an agent may do: tool levels and approvals apply as before.

## Memory

Memory is what the assistant knows about the owner: preferences, habits and circumstances, one sentence each. Every
fact has a `source` and a `confidence` from 0 to 1.

- A fact the owner states through `POST /memory` is `confirmed`, with the source `owner` and the confidence 1.
- A fact an agent learned through the `memory.propose` tool is `proposed`, with the source `agent`, the run as
  reference and what it was learned from as label.

| Endpoint | Purpose |
|---|---|
| `GET /memory?status=&category=` | Facts, newest first |
| `POST /memory` | State a fact |
| `GET`, `PATCH`, `DELETE /memory/{id}` | Read, change or forget. `{"paused": true}` keeps a fact out of prompts. Rewording a fact confirms it |
| `POST /memory/{id}/confirm` | Confirm a proposed fact |

Runs see the newest 100 confirmed facts that are not paused, in an `<owner_facts>` block of the system prompt. Proposals
never reach a prompt, because an agent may have learned them from untrusted content.

Facts live in Postgres, not in the Second Brain. Brain entries are knowledge about the world in files you edit, while
facts need a confidence, a confirmation and a place in every prompt.

## Alerts

Alerts collect what broke. Every alert has a `kind`, a `key` that names the problem, a `severity` (`info`, `warning`
or `critical`) and the times it was first and last seen.

| Kind | Key | Severity | Raised when | Resolved when |
|---|---|---|---|---|
| `connector` | `connector:<name>` | `critical` | A run fails because a connector is not authorized, for example after its token expired | A run of an agent with tools of that connector succeeds |
| `connector` | `connector:<name>` | `warning` | A run fails because a connector cannot be reached | Same as above |
| `run_failed` | `run:<agent>` | `warning` | A run fails, stops at its step or time limit, or is rejected | The next run of that agent succeeds |
| `cost_limit` | `cost:<agent>` | `warning` | A run stops at its cost limit | The next run of that agent succeeds |
| `check_failed` | `check:<source>:<check>` | `warning` | A check of one of your pull requests starts failing | It passes again |
| `node_offline` | `node:<id>` | `warning` | A desktop agent that is not paused sends no report for 15 minutes | It reports again |

While an alert of a key is unresolved, a repeat counts up `count` and moves `last_seen_at` instead of adding an alert,
and the severity can only go up. After it is resolved, the next occurrence opens a new alert.

| Endpoint | Purpose |
|---|---|
| `GET /alerts?view=` | `active` (default: open and acknowledged), `open`, `acknowledged`, `resolved` or `all`, worst first |
| `GET /alerts/counts` | Unresolved alerts per severity |
| `GET /alerts/{id}` | One alert |
| `POST /alerts/{id}/acknowledge` | Mark an open alert as seen. It stays until it is resolved |
| `POST /alerts/{id}/resolve` | Resolve it by hand |
| `GET /alerts/{id}/series?days=14` | A daily series for a chart, in UTC days: runs, failures and cost of the agent for `run` and `cost_limit` alerts, failed runs naming the connector for `connector` alerts, and refreshes of the source for `check_failed` alerts. `metric` is `runs`, `syncs` or `none` |

## Pushes

Besides the live events, Lighthouse can push to the owner through the delivery channels of agents. `[notifications]`
in the configuration decides what is sent:

```toml
[notifications]
deliver = ["owner"]   # owner, mail:<address> or slack:<alias>; empty sends nothing
decisions = true      # a run waits for a decision ("ask before")
done = false          # a run finished ("done")
alerts = "critical"   # lowest severity of new or escalated alerts to push
```

A push names the agent and the tools it proposes, never their arguments. Failed pushes are logged and do not affect
the run.

## Talk

Talk is a conversation with the assistant, kept as threads of messages.

| Endpoint | Purpose |
|---|---|
| `GET /talk/threads` | Threads, pinned ones first |
| `POST /talk/threads` | Start one, with an optional `title` |
| `GET`, `PATCH`, `DELETE /talk/threads/{id}` | Read with all messages, rename or pin (`{"pinned": true}`), delete |
| `POST /talk/threads/{id}/messages` | `{"text": "..."}` sends a message and returns the `question` and the `answer` |

With `[talk] agent = "<agent id>"` in the configuration, a run of that agent answers, with the last twelve messages as
context. The answer is `pending` (`202`) until the run ends, then `done` or `failed`, and its `run_id` leads to the run
with its tool calls and approvals, so the apps can show them inline. Anything the agent wants to send waits for your
approval as usual. Without a talk agent, the Second Brain answers at once (`200`) and the answer lists its `sources`.
The first message names a thread that has no title of its own.
