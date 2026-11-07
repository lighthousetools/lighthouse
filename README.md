# Lighthouse by Lighthouse Tools

Lighthouse is an open-source personal assistant and agent system written in Rust. It runs self-hosted with your own
API keys, reads from your tools through [MCP](https://modelcontextprotocol.io), keeps a Second Brain of what you know, and
watches your Claude Code sessions. Every run is logged with all tool calls, tokens and costs.

> **Status:** early development. Connections to real providers and services are not fully tested yet.

## Features

- **Agents as files:** Markdown with frontmatter, or written for you from a description.
- **Approvals:** anything with external effect pauses the run until you decide, from any device.
- **Your desk:** tasks, standing rules, what the assistant knows about you, alerts and one feed of what waits for you.
  See [docs/api.md](docs/api.md).
- **Automations:** a trigger and a few steps as a file, with approvals for anything that acts. See
  [docs/automations.md](docs/automations.md).
- **Briefings as data:** the Morning Brief and the weekly recap as structured documents for the apps. See
  [docs/documents.md](docs/documents.md).
- **Second Brain:** Markdown files with hybrid search, covering text, PDF, images and audio. See [docs/brain.md](docs/brain.md).
- **Claude Code:** see sessions and answer permission prompts remotely, using official hooks. See
  [docs/desktop-agent.md](docs/desktop-agent.md).
- **Bring your own key:** Anthropic and OpenAI, with a model per task and a fallback.
- **MCP connectors:** stdio and Streamable HTTP, with bearer tokens or the full OAuth flow.
- **Skills:** the open `SKILL.md` format, loaded on demand.
- **Encrypted vault:** API keys and OAuth tokens are stored with AES-256-GCM.
- **Audit log:** an append-only record of every step, with cost limits per run.

## Apps

| Repository | Content |
|---|---|
| [`lighthouse-web`](https://github.com/lighthousetools/lighthouse-web) | Web app, also installable as a PWA |
| [`lighthouse-apple`](https://github.com/lighthousetools/lighthouse-apple) | Mac app with menu bar item, iOS app |

## Quickstart

You need Docker and a public HTTPS URL if you want to use OAuth connectors.

```sh
cp lighthouse.example.toml lighthouse.toml   # adjust owner, models and connectors
cp .env.example .env                         # set both values
mkdir -p agents automations && cp agents/examples/*.md agents/
docker compose build

alias lht='docker compose run --rm lighthouse lht'
lht keys init                # creates the master key, back it up
docker compose up -d

lht keys set anthropic       # asks for the key
lht connect linear           # prints the URL that authorizes the connector
lht connectors tools linear  # shows the real tool names for your allowlist
lht agents check
lht run morning-brief
lht runs list
```

See [configuration](docs/configuration.md), [agent files](docs/agents.md) and [architecture](docs/architecture.md).

## Development

You need stable Rust and a local Postgres for the integration tests.

```sh
docker run -d -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:17
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres

cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

## License

[MIT](LICENSE)
