# Agent files

An agent is a Markdown file in the agents directory. The frontmatter configures it, and the body holds its instructions.
Agents are read at server start. Run `lht agents check` to validate them.

```markdown
---
name: Morning Brief
id: morning-brief
scope: org
trigger:
  cron: "0 7 * * MON-FRI"
tools:
  - linear.list_issues
skills: [morning-brief]
models:
  text: anthropic
  fallback: openai
limits:
  max_cost_eur: 0.50
  timeout: 10m
  max_steps: 15
deliver: [owner]
---
Write today's Morning Brief.
```

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Display name, also the email subject |
| `id` | no | Stable ID, otherwise derived from the name |
| `scope` | yes | `org`, `personal:<user>` or `team:<team>` |
| `trigger` | yes | `cron: "<expression>"` or `manual: true` |
| `tools` | no | Qualified names, `connector.tool` |
| `skills` | no | Skills the agent may load |
| `models` | no | Provider and fallback, overriding the routing |
| `limits` | no | `max_cost_eur` (default 1.0), `timeout` (10m), `max_steps` (20) |
| `deliver` | no | `owner`, `mail:<address>` or `slack:<alias>` |

Unknown fields are errors, so an unsupported feature is never silently ignored.

## Tools

An agent file declares the level of each tool, and the connector configuration decides the real one:

| Level | Connector | What happens |
|---|---|---|
| `read` | Listed in `read_tools` | Runs directly |
| `draft` | Listed in `draft_tools` | Runs directly. Use it for reversible actions without external effect |
| `write` | Everything else | The run pauses and proposes the call. It continues after your decision |

A tool without a suffix is `read`. An agent that declares a tool below its real level does not start, so a file can
never hide that it acts. The run appears in the log as `rejected`, with the reason "would have needed an approval".

Proposals appear under `GET /approvals` and in the apps. You can approve, deny or edit the arguments. If you approve, the
tool runs with the final arguments. If you deny, the model is told that the action did not happen and continues. Open
proposals count as denied after 72 hours. `quotes_external` marks proposals from runs that had read external content, so
check those with extra care.

### Reasons and context

Every `draft` and `write` tool, and the built-in proposal tools, accept two extra arguments that the model sees in the
tool's schema:

| Argument | Meaning |
|---|---|
| `_reason` | One sentence for the owner: why this is worth approving, such as "John asked for the offer by Friday" |
| `_context` | The item the call answers: the ID of a connected item, or its external ID such as a mail thread ID |

Lighthouse keeps both for the review in `GET /ready` and removes them before an external tool sees the arguments. The
reason of a proposed task becomes its notes when it has none, and that of a proposed post is stored with the post. When
a call has no `_reason`, the review falls back to what the model said before it made the call.

Use `lht connectors tools <connector>` to see the real tool names of a server.

### Built-in tools

| Tool | Level | Purpose |
|---|---|---|
| `brain.search`, `brain.get` | `read` | Search and read the Second Brain, see [brain.md](brain.md) |
| `brain.remember`, `brain.link` | `draft` | Store and link Brain entries |
| `tasks.propose` | `draft` | Propose a task for the owner's list. It waits as `proposed` until the owner accepts it |
| `posts.propose` | `draft` | Propose a post as an idea. The owner edits, plans and publishes it. Agents with this tool see the owner's brand profile in their prompt |
| `memory.propose` | `draft` | Propose a fact about the owner with a `confidence` from 0 to 1. It reaches prompts only once the owner confirms it |

The bundled Morning Brief and weekly report agents propose facts about the owner with `memory.propose` when a tool
result shows a stable habit.

A briefing agent can end its report with a structured document for the apps, see [documents.md](documents.md).

Every run also sees the owner's enabled [rules](api.md#rules) and confirmed [facts](api.md#memory) in its system prompt.

## Schedules

Cron expressions use the configured time zone. A run missed during a restart is caught up once if it is at most one
hour old. Older runs are skipped.

## Creating agents in the apps

`POST /agents/draft` turns a description into an agent file with the model, without saving it. The model only sees tool
names, and a draft that hides a tool's real level is sent back for correction. `POST /agents` validates and stores a file
in `paths.created_agents`, and it takes effect at once, including its schedule. Files in `paths.agents` stay yours: the
apps show them but cannot change them.
