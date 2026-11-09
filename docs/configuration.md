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
| `<provider>` | API key of a model provider of `lighthouse.toml`, for example `anthropic`. Each tenant and each person may store their own, see [Layers](#layers) |
| `provider:<name>` | API key of a provider that a tenant or a person added in the apps |
| `mcp:<connector>` | Bearer token of a connector with `auth = "bearer"` |
| `oauth:<connector>` | OAuth tokens of a connector, written by the OAuth flow |
| `smtp` | SMTP password |
| `slack:<alias>` | Incoming webhook URL, for example `slack:#weekly` |

Keys take effect on the next run without a restart. Set the admin token with `LIGHTHOUSE_SERVER__ADMIN_TOKEN`. The
master key comes from `vault.master_key_file` or `LIGHTHOUSE_VAULT__MASTER_KEY`. Back it up, because the vault cannot be
read without it.

## Models

Model names and prices live in the configuration, never in code. A provider lists its models per task, and `routing`
selects the default provider and fallback of a task. Prices are in EUR per million tokens. Tenants and people can add
their own providers, models and routes in the apps on top of these, see [Layers](#layers).

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
| `data_dir` | `tenants` | One subdirectory per further tenant for its Brain. The default tenant keeps `[brain] path`. Agents and automations created in the apps live in the database |
| `allow_private_endpoints` | `false` | Whether providers and connectors that tenants and people add in the apps may use plain HTTP and private, loopback or link-local addresses. Keep it off on a hosted instance. A self-hosted instance whose models or MCP servers run in its own network may turn it on |
| `limits` | see below | How much a tenant and its members may add in the apps |
| `require_verified_email` | `false` | People who signed up themselves must confirm their email address before they sign in. Sign-up then answers 202 without a token, and `POST /auth/login` answers 403 until the link in the mail was opened; `POST /auth/verify/request` sends a new one. The configured owner and invited people are not affected, and devices signed in before stay signed in. Needs mail; the server does not start without it |

`server.trust_forwarded_for` (default `false`) counts the first address of `X-Forwarded-For` as the client for rate
limits. Turn it on only behind a reverse proxy that sets this header.

Verification and invitation mails go through `[delivery.smtp]` with the `smtp` password of the default tenant's vault,
or through the mailer of a program built on the crates (see [architecture.md](architecture.md#extension-points)).
Without SMTP, no verification mail is sent, and `POST /tenant/invitations` returns the accept link to the inviting
admin instead, to pass on in another way.

`[tenancy.limits]` caps what the apps may add:

| Key | Default | Meaning |
|---|---|---|
| `providers` | `10` | Providers per tenant, and again per person |
| `models` | `20` | Models per provider |
| `connectors` | `25` | Connectors per tenant, and again per person |
| `sources` | `10` | Connected sources per tenant, and again per person |
| `files` | `200` | Agents, automations and skills of each kind per tenant, and again per person |
| `tenant_entries` | `2000` | All entries of a tenant and its members together |

Agent files in `paths.agents` and `paths.automations`, skills, sources and the instance's providers, routing and
connectors apply to every tenant as defaults. Each tenant has its own vault, so connector tokens and provider keys are
its own. `[owner] accounts` only apply to the first owner of the default tenant; everyone else sets their own
`accounts` and `addresses` in the apps.

## Layers

Providers, models, routing and MCP connectors come in three layers:

| Layer | Who sets it | Where |
|---|---|---|
| `instance` | The operator | `lighthouse.toml`. Read-only in the apps and the API |
| `tenant` | Owners and admins of a tenant, for all its members | The apps, or `/settings/tenant/...` |
| `personal` | Each member, for themselves in that tenant | The apps, or `/settings/me/...` |

What applies to a request or a run is resolved for its tenant and user: personal over tenant over instance. Every
merged entry names the layer it comes from. A run works for its user, so it uses that person's settings, keys and
connectors; scheduled runs and automations work for the tenant's first owner, and source refreshes see the tenant
layer. An instance with only `lighthouse.toml` behaves exactly as before.

Merge rules:

- **Providers** merge by name. A layer can add a new provider with kind `anthropic` or `openai` and an optional base
  URL. A provider whose name a lower layer has keeps that layer's kind and base URL: a higher layer can only change its
  models. This way no layer sends another layer's key to an endpoint of its own choice.
- **Models** merge per provider and task. The highest layer with a model for a task wins.
- **Routing** merges per task. A tenant route replaces the instance route of that task; a personal route replaces
  both for that person. A route needs a model for its task at the primary and the fallback provider.
- **Connectors** merge by name. Tenants and people add connectors over streamable HTTP only: a connector that starts a
  process on the server exists only in `lighthouse.toml`. A new connector cannot take a name a lower layer has. A
  tenant can switch an instance connector off for itself, and a person can switch an instance or tenant connector off
  for themselves.

Single settings follow the same layers: `[talk] agent`, `[notifications]`, `[publishing]`, the models of `[brain]`,
`slack_channels` of `[delivery]`, the place of `[weather]` and the time zone, plus the language of the apps, which has
no entry in the file. See
[api.md](api.md#providers-models-and-connectors) for their values. Everything operational stays in the file: `[server]`,
`[database]`, `[vault]`, `[tenancy]`, SMTP, `[paths]`, the Brain's directory, the weather service's `base_url` and
`[extensions]`.

Connected sources follow the same layers too, see [connected.md](connected.md#sources-in-layers).

Agents, automations and skills too, see [agents.md](agents.md#agents-in-layers).

Time zones: a person's `timezone` decides their day boundaries in the API, prompts of their runs and the schedules of
what runs for them. Budget months stay per tenant, in the tenant's zone, because the budget is shared.

Keys and tokens never sit in these settings. They stay in the vault:

- A provider's key is read from the person's own vault, then the tenant's, then the instance's if
  `allow_instance_keys` permits it. A vault is only read for a provider whose endpoint its own layer or a lower one
  chose, and a person's own key is never used for a base URL that a tenant set, since owners and admins could change it
  and collect the key. Providers of `lighthouse.toml` keep `<provider>` as entry name; providers added in the apps use
  `provider:<name>`, so they never read another entry of the vault.
- A connector's tokens (`mcp:<connector>`, `oauth:<connector>`) are read from the person's own vault, then the
  tenant's. A personal connector only uses the person's own vault, so no other member can use its tokens. When the URL
  of a tenant connector changes, or it is deleted, members' own tokens for it are removed, so they never reach an
  endpoint those members did not connect to. People
  store their own credentials through `/me/credentials`, and connect OAuth connectors for themselves with
  `POST /connectors/{name}/connect?scope=personal`.

Endpoints that tenants and people enter must be public `https` URLs. The server resolves the host and refuses
loopback, private, link-local, shared and cloud metadata addresses, both when the setting is saved and on every
connection, and it follows redirects only to public `https` URLs. `allow_private_endpoints` lifts this for
self-hosted instances. Changes take effect on the next run without a restart, and each change is recorded with who
made it (`GET /settings/tenant/changes`). A change runs in one database transaction under a lock per tenant, so a
failed step leaves nothing behind and limits hold across server processes. Several server processes on one database pick up each other's changes
within 15 seconds.

The apps prefill forms from `GET /settings/presets`, which lists suggested providers, models with list prices and
connectors. The suggestions live in `crates/settings/presets.toml`. They are data for the forms; the server never
routes by them.

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
