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

## Notifications

`[notifications]` decides what is pushed to the owner through the delivery channels: runs that wait for a decision,
finished runs and new alerts from a severity on. Without `deliver` targets, only the apps hear about it through live
events. See [api.md](api.md#pushes).

## Publishing

`[publishing]` names the connector tool that publishes posts. It must be a `write` tool, so every post waits for your
approval. See [publishing.md](publishing.md#publishing).

## Connectors

| Key | Meaning |
|---|---|
| `transport` | `http` or `stdio` |
| `read_tools` | Allowlist of read-only tools |
| `draft_tools` | Allowlist of reversible tools without external effect. Every tool on neither list counts as `write` |
| `url`, `auth` | For `http`. `auth` is `none`, `bearer` or `oauth` |
| `client_id`, `scopes` | Optional OAuth settings, for servers without dynamic client registration |
| `allow_private_addresses` | Allows OAuth discovery on private addresses |
| `command`, `args` | For `stdio` |
| `secret_env` | For `stdio`: environment variables filled from vault entries |

For OAuth connectors, `server.public_url` must be reachable from your browser. Run `lht connect <connector>` and open
the printed URL. The redirect goes to `<public_url>/oauth/callback`. Tokens are stored in the vault and refreshed
automatically. A pending authorization lives in the server's memory, so finish it before the server restarts.

## API

`GET /openapi.json` describes the API. Everything except `/health`, `/openapi.json`, `/auth/status`, `/auth/login` and
`/oauth/callback` needs a device token or the admin token as a bearer token.

Other endpoints: `/agents`, `/connectors`, `/skills` and `/costs`. See [api.md](api.md) for live events, tasks and the
other parts of the owner's desk.

To serve the web app from the same address, build `lighthousetools/lighthouse-web` and set `paths.web` to its `dist`
directory.

## Sign-in

Lighthouse has one owner with one password of at least 12 characters. Set it once with the admin token:

```sh
curl -X POST "$LIGHTHOUSE_URL/auth/setup" \
  -H "Authorization: Bearer $LIGHTHOUSE_SERVER__ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"password": "..."}'
```

`POST /auth/login` then issues one token per device. Tokens are stored only as a hash and can be revoked one by one
through `DELETE /auth/devices/{id}`. Setting a new password signs out every device. After five failed attempts, sign-in
pauses for a minute.

Credentials can also be managed through `/credentials`. The API accepts values but never returns them.
