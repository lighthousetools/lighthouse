# Configuration

Lighthouse reads `lighthouse.toml`, or the file named by `LIGHTHOUSE_CONFIG`. See
[`lighthouse.example.toml`](../lighthouse.example.toml) for a complete example.

Any value can be overridden with an environment variable: `LIGHTHOUSE_<SECTION>__<KEY>`, for example
`LIGHTHOUSE_DATABASE__URL`. Only variables whose first part names a section of the file count; others with the
`LIGHTHOUSE_` prefix, such as `LIGHTHOUSE_CONFIG`, are ignored.

### Extensions

Programs built on the crates (see [architecture.md](architecture.md#extension-points)) keep their own settings under
`[extensions.<name>]`. Lighthouse ignores these tables and passes them on as they are:

```toml
[extensions.example]
limit = 3
```

The program reads them with `config.extension::<T>("example")` into its own type, or as raw values from
`config.extensions`. Environment variables reach them too, for example `LIGHTHOUSE_EXTENSIONS__EXAMPLE__LIMIT=5`.

## Secrets

Secrets are not part of the configuration file. Store them on the running instance with `lht keys set <name>`, which
writes to the vault of the default tenant:

| Entry | Used for |
|---|---|
| `<provider>` | API key of a model provider, for example `anthropic`. Each tenant may store its own, see [Tenancy](#tenancy) |
| `mcp:<connector>` | Bearer token of a connector with `auth = "bearer"` |
| `smtp` | SMTP password |
| `slack:<alias>` | Incoming webhook URL, for example `slack:#weekly` |

Keys take effect on the next run without a restart. Set the admin token with `LIGHTHOUSE_SERVER__ADMIN_TOKEN`. The
master key comes from `vault.master_key_file` or `LIGHTHOUSE_VAULT__MASTER_KEY`. Back it up, because the vault cannot be
read without it.

## Models

Model names and prices live in the configuration, never in code. A provider lists its models per task, and `routing`
selects the default provider and fallback of a task. Prices are in EUR per million tokens.

The provider `kind` is `anthropic`, `openai`, `mock` (echoes, for tests) or `demo`. The demo provider needs no key
and no model: it calls the read tools of an agent once and then writes a plausible answer, such as a brief with a
structured document, a rewrite or a deck. It exists for the local demo in `demo/` and is useless for real work.

## Notifications

`[notifications]` decides what is pushed to the owner through the delivery channels: runs that wait for a decision,
finished runs and new alerts from a severity on. Without `deliver` targets, only the apps hear about it through live
events. See [api.md](api.md#pushes).

## Publishing

`[publishing]` names the connector tool that publishes posts. It must be a `write` tool, so every post waits for your
approval. `service`, such as `linkedin`, names the network for logos in the apps. See [publishing.md](publishing.md#publishing).

## Talk

`[talk] agent` names the agent that answers conversations in the apps. Without it, the Second Brain answers. See
[api.md](api.md#talk).

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

`GET /openapi.json` describes the API. Everything except `/health`, `/openapi.json`, `/oauth/callback` and the public
sign-in endpoints needs a device token or the admin token as a bearer token.

Other endpoints: `/agents`, `/connectors`, `/skills` and `/costs`. See [api.md](api.md) for live events, tasks and the
other parts of the owner's desk.

To serve the web app from the same address, build `lighthousetools/lighthouse-web` and set `paths.web` to its `dist`
directory. The [release image](release-image.md) already contains the web app and sets `paths.web` for you.

## Sign-in

People sign in with an email address and a password of at least 12 characters. On the first start, the configured
`[owner]` becomes the first user and the owner of the default tenant. Set their password once with the admin token:

```sh
curl -X POST "$LIGHTHOUSE_URL/auth/setup" \
  -H "Authorization: Bearer $LIGHTHOUSE_SERVER__ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"password": "..."}'
```

`POST /auth/login` then issues one token per device. Tokens are stored only as a hash and can be revoked one by one
through `DELETE /auth/devices/{id}`. Setting a new password signs out every device of the user. After five failed
attempts from one address or for one email address, sign-in pauses for a minute. See [api.md](api.md#users-and-tenants).

Credentials can also be managed through `/credentials`. The API accepts values but never returns them.

## Tenancy

`[tenancy]` decides who may join and how tenants use model keys:

| Key | Default | Meaning |
|---|---|---|
| `signup` | `closed` | `closed`: nobody signs up and nobody is invited, as in a single-user install. `invite`: owners and admins invite people by email. `open`: anyone signs up and owns a new tenant; invitations work too |
| `allow_instance_keys` | `true` | Tenants without their own key of a model provider use the instance's key, the one stored in the default tenant's vault (`lht keys set anthropic`). Hosted instances set `false`, so every tenant brings its own key through `PUT /credentials/<provider>` |
| `data_dir` | `tenants` | One subdirectory per further tenant for its Brain and the agents and automations created in the apps. The default tenant keeps `[brain] path` and the `[paths]` directories |
| `require_verified_email` | `false` | People who signed up themselves must confirm their email address before they sign in. Sign-up then answers 202 without a token, and `POST /auth/login` answers 403 until the link in the mail was opened; `POST /auth/verify/request` sends a new one. The configured owner and invited people are not affected, and devices signed in before stay signed in. Needs mail; the server does not start without it |

`server.trust_forwarded_for` (default `false`) counts the first address of `X-Forwarded-For` as the client for rate
limits. Turn it on only behind a reverse proxy that sets this header.

Verification and invitation mails go through `[delivery.smtp]` with the `smtp` password of the default tenant's vault,
or through the mailer of a program built on the crates (see [architecture.md](architecture.md#extension-points)).
Without SMTP, no verification mail is sent, and `POST /tenant/invitations` returns the accept link to the inviting
admin instead, to pass on in another way.

Agent files in `paths.agents` and `paths.automations`, skills, connectors, sources and model routing apply to every
tenant. Each tenant has its own vault, so connector tokens and provider keys are its own. `[owner] accounts` only apply
to the default tenant; in other tenants, agents work for the tenant's first owner.

### Command line

Server commands of `lht` use the admin token, in the default tenant or the one `--tenant <id>` names. With a user's
device token in `LHT_TOKEN` (or `--token`), they act as that user in the token's tenant instead. `lht keys` and
`lht runs` work on the database directly, also in the tenant `--tenant` names.

| Command | What it does |
|---|---|
| `lht login [--email <email>]` | Asks for the password and prints a device token to export as `LHT_TOKEN` |
| `lht tenants` | Lists the tenants of the signed-in user, the current one marked |
| `lht switch <tenant>` | Prints a token for another tenant of the user |
| `lht invite <email> [--role admin]` | Invites someone to the current tenant and prints the link if no mail is configured |
| `lht --tenant <id> keys set anthropic` | Stores a tenant's own provider key |

### Upgrading from 0.1

Nothing needs to change. On the first start of 0.2, the configured owner becomes a user with their `email`, keeps the
password and every signed-in device, and becomes the owner of the default tenant, which keeps all data. The Mac app
and other clients that sign in with the password alone keep working while the instance has one user. Once you invite a
second person, sign-in needs the email address.
