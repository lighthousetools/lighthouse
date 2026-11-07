# Automations

An automation is a trigger and a few ordered steps, stored as a Markdown file with frontmatter like an agent. The body
describes it for people. Lighthouse reads files from `paths.automations`, which you manage, and from
`paths.created_automations`, where automations created in the apps land. Changes take effect without a restart.

```markdown
---
name: Triage failed briefs
id: triage-failed-briefs        # optional, derived from the name
enabled: true                   # optional, the apps can override it
trigger:
  event: run.failed
  match:
    agent_id: morning-brief
steps:
  - agent: morning-brief
  - if:
      status: failed
  - notify:
      to: owner
      subject: "The Morning Brief failed twice"
      message: "The retry failed as well: {{output}}"
---
Retries the Morning Brief once when it fails, and tells you if the retry fails too.
```

See [`automations/examples`](../automations/examples) for more. Unknown fields are errors.

## Triggers

Exactly one of:

| Trigger | Runs when |
|---|---|
| `cron: "0 7 * * MON-FRI"` | The schedule fires, in `server.timezone`, with the same catch-up rules as agents |
| `event: <name>` | An event of that name happens |
| `webhook: true` | `POST /automations/{id}/webhook` is called |
| `manual: true` | Only on request |

Events and webhooks can have a `match` map. Every field must be in the payload and contain the given text, ignoring
case. A dotted key reads a nested field, such as `run.agent_id`.

Lighthouse sends these events itself:

| Event | Payload |
|---|---|
| `run.failed` | `run_id`, `agent_id`, `status` (`failed`, `limit_exceeded` or `rejected`), `reason`, `trigger` |
| `run.succeeded` | The same, for successful runs |
| `run.waiting` | The same, for runs that wait for a decision |

Connected sources send `mail.received`, `issue.assigned`, `review.requested` and `check.failed`, see
[connected.md](connected.md#refresh). Other events come from outside through `POST /automations/events` with `{"name": "mail.received", "payload": {...}}`,
for example from a connector or a mail filter that reports new mail. Names starting with `run.` are reserved. Runs that
an automation started never send `run.*` events, so automations cannot trigger each other in a loop.

Webhooks and events need a bearer token like every endpoint. Issue a device token for the service that calls them.

## Steps

| Step | Does |
|---|---|
| `agent: <id>` | Runs the agent to its end. It gets the event and the output of the previous step as untrusted context |
| `tool: {name, input}` | Calls a tool. A `read` or `draft` tool runs directly. Every other tool proposes the call and the run waits for your approval, like an agent's |
| `notify: {to, subject, message}` | Sends a message to `owner`, `mail:<address>` or `slack:<alias>` |
| `if: {contains: <text>}` | Goes on only if the previous output contains the text, ignoring case. Also `not_contains: <text>` and `status: succeeded` or `status: failed` |

### Branches

An `if` with `then` and `else` takes one of two ways and then goes on with the next step:

```yaml
  - if:
      contains: urgent
      then:
        - notify:
            message: "Urgent: {{output}}"
      else:
        - agent: archive-helper
```

An `if` without `then` and `else` stops the automation when the condition does not hold. Branches can be nested.

An automation has 1 to 20 steps, branches included. Each step sees the output and status of the one before. Strings in `input`, `subject`
and `message` can use placeholders:

| Placeholder | Value |
|---|---|
| `{{output}}` | Output of the previous step: the report of an agent, or the result of a tool |
| `{{status}}` | `succeeded` or `failed` |
| `{{automation}}` | Name of the automation |
| `{{event}}` | The whole payload as JSON |
| `{{event.<field>}}` | One field of the payload |

## Runs

An agent step that pauses for an approval makes the automation wait too. When the agent run ends, the automation goes
on with its report, at the latest one minute later.

An automation run is an ordinary run with the agent ID `automation:<id>`. Its steps are in the log: `agent`, `tool`,
`delivery` and `condition`. A run ends as `succeeded`, or as `failed` when its last step failed. A condition that does
not hold ends the run as `succeeded` with a report that says where it stopped. A denied approval ends it the same way,
without acting.

Approvals of automations appear in `GET /approvals` and `GET /ready` like those of agents. The apps see the run through
`run.waiting` and `run.finished` events, and failed automation runs raise alerts like failed agent runs.

## API

| Endpoint | Purpose |
|---|---|
| `GET /automations` | All automations with trigger, steps (with `needs_approval`), `enabled`, next run and file |
| `GET /automations/{id}` | One automation |
| `POST /automations` | Create or replace one from `{"source": "<file>"}` |
| `PATCH /automations/{id}` | `{"enabled": false}` switches it off without changing the file |
| `DELETE /automations/{id}` | Delete one created in the apps |
| `GET /automations/{id}/runs` | Its runs, newest first |
| `POST /automations/{id}/run` | Start it now with an optional `{"payload": {...}}`, also when it is switched off |
| `POST /automations/{id}/webhook` | Call its webhook with a JSON payload |
| `POST /automations/events` | Report an event from outside |

| `POST /automations/dry-run` | Show what it would do with `{"id"` or `"source", "payload"}`: whether the payload matches the trigger, and every step with its placeholders filled in and whether it needs approval. Nothing runs |
| `POST /automations/draft` | Write an automation file from `{"description": "..."}` for you to review and save. The model sees the agents and their tools |

`GET /events` sends `automations.changed` when one is created, switched or deleted.
