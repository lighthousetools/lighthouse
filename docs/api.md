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
