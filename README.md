# Lighthouse by Lighthouse Tools

Lighthouse is an open-source personal assistant and agent system written in Rust. It runs self-hosted with your own
API keys, reads from your tools through [MCP](https://modelcontextprotocol.io), and delivers a daily brief and a weekly
report by email or Slack. Every run is logged with all tool calls, tokens and costs.

> **Status:** early development. The first version only reads. Agents cannot change anything in your tools.

## Features

- **Agents as files:** Markdown with frontmatter, readable and versionable.
- **Bring your own key:** Anthropic and OpenAI, with a model per task and a fallback.
- **MCP connectors:** stdio and Streamable HTTP, with bearer tokens or the full OAuth flow.
- **Read-only by design:** a tool counts as read-only only if its connector lists it.
- **Skills:** the open `SKILL.md` format, loaded on demand.
- **Encrypted vault:** API keys and OAuth tokens are stored with AES-256-GCM.
- **Audit log:** an append-only record of every step, with cost limits per run.

## Quickstart

You need Docker and a public HTTPS URL if you want to use OAuth connectors.

```sh
cp lighthouse.example.toml lighthouse.toml   # adjust owner, models and connectors
cp .env.example .env                         # set both values
mkdir -p agents && cp agents/examples/*.md agents/
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
