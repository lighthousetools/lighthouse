# Configuration

Lighthouse reads `lighthouse.toml`, or the file named by `LIGHTHOUSE_CONFIG`. See
[`lighthouse.example.toml`](../lighthouse.example.toml) for a complete example.

Any value can be overridden with an environment variable: `LIGHTHOUSE_<SECTION>__<KEY>`, for example
`LIGHTHOUSE_DATABASE__URL`.

## Secrets

Secrets are not part of the configuration file. Store them on the running instance with `lht keys set <name>`:

| Entry | Used for |
|---|---|
| `<provider>` | API key of a model provider, for example `anthropic` |
| `mcp:<connector>` | Bearer token of a connector with `auth = "bearer"` |
| `smtp` | SMTP password |
| `slack:<alias>` | Incoming webhook URL, for example `slack:#weekly` |

Keys take effect on the next run without a restart. Set the admin token with `LIGHTHOUSE_SERVER__ADMIN_TOKEN`. The
master key comes from `vault.master_key_file` or `LIGHTHOUSE_VAULT__MASTER_KEY`. Back it up, because the vault cannot be
read without it.

## Models

Model names and prices live in the configuration, never in code. A provider lists its models per task, and `routing`
selects the default provider and fallback of a task. Prices are in EUR per million tokens.

## Connectors

| Key | Meaning |
|---|---|
| `transport` | `http` or `stdio` |
| `read_tools` | Allowlist of read-only tools. Every other tool counts as `write` |
| `url`, `auth` | For `http`. `auth` is `none`, `bearer` or `oauth` |
| `client_id`, `scopes` | Optional OAuth settings, for servers without dynamic client registration |
| `allow_private_addresses` | Allows OAuth discovery on private addresses |
| `command`, `args` | For `stdio` |
| `secret_env` | For `stdio`: environment variables filled from vault entries |

For OAuth connectors, `server.public_url` must be reachable from your browser. Run `lht connect <connector>` and open
the printed URL. The redirect goes to `<public_url>/oauth/callback`. Tokens are stored in the vault and refreshed
automatically. A pending authorization lives in the server's memory, so finish it before the server restarts.

## API

`GET /openapi.json` describes the API. Everything except `/health`, `/openapi.json` and `/oauth/callback` needs the admin
token as a bearer token. Without a configured token these endpoints stay closed.
