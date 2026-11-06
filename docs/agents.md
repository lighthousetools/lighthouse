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

A tool without a suffix is read-only. An agent that names a tool with `draft` or `write`, or a tool its connector does
not list in `read_tools`, does not start. The run appears in the log as `rejected`, with the reason "would have needed
an approval".

Use `lht connectors tools <connector>` to see the real tool names of a server.

## Schedules

Cron expressions use the configured time zone. A run missed during a restart is caught up once if it is at most one
hour old. Older runs are skipped.
