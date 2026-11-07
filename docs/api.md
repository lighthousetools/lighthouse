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

A client that falls behind gets a `changed` event instead of the events it missed.

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

"Today" uses the time zone in `server.timezone`.

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
