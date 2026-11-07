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
- **Connected work:** a searchable cache of mail, calendar, issues and pull requests through your MCP connectors, with
  a unified inbox and approval-gated replies, comments and reviews. See [docs/connected.md](docs/connected.md).
- **Business and publishing:** deals, bank, invoices, contracts, deal rooms and holdings per company with key numbers,
  and posts, decks and a brand profile. Nothing is paid, signed, sent or published without your approval. See
  [docs/connected.md](docs/connected.md) and [docs/publishing.md](docs/publishing.md).
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

## Try it locally

The demo shows the whole product with fictional data: two companies, Acme Ltd and Globex Inc, with mail, calendar,
issues, pull requests, deals, bank accounts, invoices, contracts, deal rooms and a portfolio. A demo MCP server serves
that data, and a demo provider answers instead of a model, so agents, briefs, recaps, conversations and rewrites work
without any account or API key.

```sh
docker compose -f demo/compose.yml up --build
```

Open http://localhost:8088 and sign in with the password `lighthouse-demo`. The web app is served from
`dist` of `../lighthouse-web` if you built it next to this repository; set `LIGHTHOUSE_WEB` to another checkout of the
web app to use that one, and `LIGHTHOUSE_DEMO_PORT` to serve on another port than 8088.
The API answers to the bearer token `demo-admin-token`. Run the agents `morning-brief`, `weekly-report` and `inbox`
from the web app, or with `curl -X POST -H 'Authorization: Bearer demo-admin-token' localhost:8088/agents/morning-brief/run`;
the inbox agent stops at an approval before it "sends" a reply. At start, the demo runs what is missing (`lht demo warm`): the agent `ready` while Ready for you is empty, and the
Morning Brief and weekly report while there is no brief or recap. So Ready for
you already holds mail replies with the original quoted, an issue comment, a pull request review, a calendar move and a
post idea, each with its service, counterpart and reason. Afterwards `demo/verify.sh` checks that every list has
data.

Remove the demo and all its data with `docker compose -f demo/compose.yml down -v`. Everything in it is made up, and
its keys protect nothing: never use `demo/lighthouse.demo.toml` for a real instance.

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
# --rm also removes the volume, with every test database, when the container stops
docker run -d --rm --name lighthouse-test-pg -e POSTGRES_PASSWORD=postgres -p 5432:5432 pgvector/pgvector:pg17
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres

cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
docker stop lighthouse-test-pg
```

Each test creates its own database and drops it when it ends. Databases that a crashed run left behind are dropped
by the next run after two hours.

## License

[MIT](LICENSE)
