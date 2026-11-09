# API

`GET /openapi.json` describes every endpoint with its schemas. This page explains the concepts behind them.

Everything except `/health`, `/openapi.json`, `/oauth/callback` and the public sign-in endpoints (`/auth/status`,
`/auth/login`, `/auth/signup`, `/auth/verify`, `/auth/verify/request`, `/auth/invitation` and `/auth/accept`) needs a bearer token: a device
token or the admin token.

## Users and tenants

People sign in as **users** with an email address and a password of at least 12 characters. A user belongs to one or
more **tenants** (a team, a company or a person on their own) with a **role**: `owner`, `admin` or `member`. Every
device token belongs to one user and is bound to one tenant, and every request works in that tenant only. Data of
other tenants is never visible, and personal data within a tenant (see [architecture.md](architecture.md#tenancy))
is visible only to its user.

The admin token has no user. It acts as an owner in the default tenant, or in the tenant whose ID the
`X-Lighthouse-Tenant` header names.

### Signing in

| Endpoint | Body | Answer |
|---|---|---|
| `GET /auth/status` | | `{setup_required, signup, email_required, verification_required}`: `signup` is `closed`, `invite` or `open`; `email_required` is true once the instance has more than one user; `verification_required` is `tenancy.require_verified_email` |
| `POST /auth/login` | `{email?, password, device_name, device_kind, tenant_id?}` | `TokenIssued` `{id, token, tenant_id, user_id}`. `email` may be left out while the instance has exactly one user. Without `tenant_id`, the token is bound to the user's oldest membership. With `tenancy.require_verified_email`, 403 for a person who signed up and has not confirmed the email address yet |
| `POST /auth/setup` (admin token) | `{password}` | 204. Sets the password of the first owner of the default tenant and signs out their devices |
| `POST /auth/signup` | `{email, name, password, tenant_name?, device_name, device_kind}` | 201 with `TokenIssued`. Only while `signup = "open"`; creates the user and a new tenant they own, and sends a verification mail if mail is configured. A server with its own sign-up hook may answer 202 with `{error: <message>}` and create nothing yet, see [architecture.md](architecture.md#extension-points). With `tenancy.require_verified_email`, the account is created but the answer is 202 with `{error: <message>}` and no token: the person confirms the address, then signs in |
| `POST /auth/verify` | `{token}` | 204. Confirms the email address with the token from the verification link |
| `POST /auth/verify/resend` | | 204, or 409 without mail |
| `POST /auth/verify/request` | `{email}` | 204 whether or not the address has an account. Sends a new verification mail to an account whose address is not confirmed yet, for people who cannot sign in before they confirm it. 409 without mail; limited to five per client address and per email address and hour (429) |
| `POST /auth/password` | `{current, new}` | 204. Every other device of the user is signed out. A wrong current password is 403, so clients keep the sign-in |
| `POST /auth/logout` | | 204. Signs out the current device |
| `GET /auth/devices` | | The user's devices in every tenant, each with its `tenant_id` |
| `DELETE /auth/devices/{id}` | | 204, or 404 for a device of somebody else |
| `POST /auth/tokens` | `{name, kind}` | `TokenIssued` for the same user and tenant, for a desktop agent or a script |

Failed sign-ins are limited per client address and per email address: after five failures, sign-in pauses for a
minute (429 with `Retry-After`). Sign-ups are limited to five per client address and hour. Behind a reverse proxy, set
`server.trust_forwarded_for` so the limits count the real client.

`device_kind` is `web`, `mac`, `ios`, `node` (desktop agent) or `api`.

### The user and their tenants

| Endpoint | Body | Answer |
|---|---|---|
| `GET /me` | | `{name, timezone, user_id, email, email_verified, tenant_id, tenant_name, role}`. `timezone` is `server.timezone`, which "today" and the days of charts follow |
| `PATCH /me` | `{name}` | 204 |
| `DELETE /me` | `{confirm: <email>}` | 204. Deletes the account and every tenant where the user is the only member, with all its data. 409 if the user is the last owner of a tenant with other members |
| `GET /tenants` | | The user's memberships: `{tenant_id, tenant_name, role, joined_at}`. For the admin token, every tenant |
| `POST /tenants` | `{name, device_name?, device_kind?}` | 201 with a `TokenIssued` for the new tenant. Only while sign-up is open |
| `POST /tenants/{id}/switch` | `{device_name?, device_kind?}` | `TokenIssued` bound to another tenant of the user. The current token stays valid until it signs out |
| `POST /tenants/accept` | `{token, device_name?, device_kind?}` | `TokenIssued` for the joined tenant. Accepts an invitation as the signed-in user, whose email address must match it |

### Administering a tenant

Reading the tenant and its members works for every member; the list of invitations and all changes need the `owner` or `admin` role; only owners grant or take away the owner
role, remove owners or delete the tenant. A tenant always keeps at least one owner (409 otherwise).

| Endpoint | Body | Answer |
|---|---|---|
| `GET /tenant` | | `{id, name, created_at, role, members}` |
| `PATCH /tenant` | `{name}` | The tenant |
| `DELETE /tenant` | `{confirm: <tenant name>}` | 204. Owner only. Deletes the tenant with all its data, files and background work. The default tenant cannot be deleted (409) |
| `GET /tenant/members` | | `{user_id, email, name, role, joined_at}` |
| `PATCH /tenant/members/{user}` | `{role}` | 204 |
| `DELETE /tenant/members/{user}` | | 204. Removes a member and signs out their devices in this tenant. Every member may remove themselves (leave) |
| `GET /tenant/invitations` | | Open invitations |
| `POST /tenant/invitations` | `{email, role}` | 201 `{invitation, mailed, accept_url}`. With mail configured, the invitation goes out by mail and `accept_url` is null. Without mail, `accept_url` is the link to pass on yourself |
| `DELETE /tenant/invitations/{id}` | | 204 |
| `PUT`, `DELETE /credentials/{name}` | `{value}` | Tenant keys, such as a model provider key (`anthropic`). Owner and admin only |
| `GET /costs/users?days=30` | | Run costs per user of the tenant: `{user_id, runs, cost_eur, input_tokens, output_tokens}`, highest first. Owner and admin only. `GET /costs` stays the tenant's cost per day and provider |

Invitations need `signup = "invite"` or `"open"` and are valid for seven days. The links point to the web app:
`<public_url>/#/invite?token=<token>` and `<public_url>/#/verify?token=<token>`. To accept, the web app calls
`POST /auth/invitation` with `{token}`, which answers `{email, role, tenant_name, expires_at, account_exists}`. A new
person then calls `POST /auth/accept` with `{token, name, password, device_name, device_kind}` (201 with a
`TokenIssued`); a person with an account signs in and calls `POST /tenants/accept`.

## Changes in 0.2

Lighthouse 0.2 serves several users and tenants. Clients of 0.1 keep working against a single-user instance, with
these changes:

- `POST /auth/login` takes an optional `email` and `tenant_id`. Without `email`, it works only while the instance has
  one user; otherwise it answers 400. `GET /auth/status` tells whether the email is required (`email_required`) and
  the sign-up mode (`signup`).
- `TokenIssued` has two more fields, `tenant_id` and `user_id`.
- `GET /me` has more fields: `user_id`, `email`, `email_verified`, `tenant_id`, `tenant_name` and `role`. `name` is
  the user's name, no longer always the configured owner.
- `Device` (in `GET /auth/devices`) has a `tenant_id`. The list holds the devices of the signed-in user only.
- `PUT` and `DELETE /credentials/{name}`, `POST /connectors/{id}/connect` and `PUT /settings/budget` need the
  `owner` or `admin` role (403 for members).
- Personal data is narrowed to its user: see [architecture.md](architecture.md#tenancy) for which lists now differ
  between members of the same tenant.
- New endpoints: everything in [Users and tenants](#users-and-tenants).
- An existing single-user instance needs no change: the configured owner becomes the first user with the same
  password and devices. See [configuration.md](configuration.md#upgrading-from-01).

## Live events

`GET /events` is a stream of server-sent events. The apps reload what an event names, so events carry IDs only and
never report texts, tool arguments or other private content.

| Event | Data | Meaning |
|---|---|---|
| `changed` | `{}` | Something changed. Sent at the start and whenever the stored state changes, also for changes made by agents |
| `tasks.changed` | `{"id": <task or null>}` | A task was created, changed, reordered or deleted |
| `rules.changed` | `{"id": <rule>}` | A rule was created, changed or deleted |
| `memory.changed` | `{"id": <fact>}` | A fact was stated, changed, confirmed or forgotten |
| `run.waiting` | `{"run_id", "agent_id", "proposals"}` | A run paused and waits for a decision |
| `run.finished` | `{"run_id", "agent_id", "status"}` | A run ended, successfully or not |
| `alert.raised` | `{"id", "kind", "severity", "new"}` | A problem appeared or happened again |
| `alerts.changed` | `{"id": <alert>}` | An alert was acknowledged or resolved |
| `ready.changed` | `{"id": <item>}` | An item was cleared from the ready feed |
| `automations.changed` | `{"id": <automation>}` | An automation was created, switched or deleted, see [automations.md](automations.md) |
| `connected.changed` | `{"source"}` or `{"run_id"}` | A source refreshed or an action ran, see [connected.md](connected.md) |
| `sessions.changed` | `{"node_id", "session_id"}` | A Claude Code session reported work or got a message, see [desktop-agent.md](desktop-agent.md) |
| `posts.changed`, `decks.changed`, `branding.changed` | `{"id"}` or `{}` | Posts, decks or the brand profile changed, see [publishing.md](publishing.md) |
| `organizations.changed` | `{"id"}` | An organization changed |
| `talk.changed` | `{"thread_id"}` | A conversation got a message or an answer |
| `settings.changed` | `{"key"}` | The budget (`budget`) or providers, models, routing or connectors (`layers`) changed |
| `document.created` | `{"id", "run_id", "kind", "period"}` | A run stored a briefing document, see [documents.md](documents.md) |

A client that falls behind gets a `changed` event instead of the events it missed.

## Ready for you

`GET /ready` returns everything that waits for the owner in one call, so the home page needs no other request:

| Kind | Group | What it is | How it leaves the feed |
|---|---|---|---|
| `approval` | `decisions` | A proposed action with external effect, such as sending a mail | `POST /approvals/{id}/decision` |
| `permission` | `decisions` | A permission request of a Claude Code session | `POST /permissions/{id}/decision` |
| `draft` | `drafts` | A draft an agent saved in an external tool with a `draft_tools` tool, in the last seven days | `POST /ready/drafts/{id}/dismiss` |
| `task` | `suggestions` | A task an agent proposed | `POST /tasks/{id}/accept` or `DELETE /tasks/{id}` |
| `fact` | `suggestions` | A fact about the owner an agent learned | `POST /memory/{id}/confirm` or `DELETE /memory/{id}` |

`POST /ready/{kind}/{id}/snooze` with `{"until": "<time>"}` hides any item until then ("later"), and `DELETE` on the
same path shows it again. A snoozed approval stays open and still expires after 72 hours.

The answer has `total`, `counts` per kind, `groups` per group and the unresolved `alerts` per severity. Items come
decisions first, then drafts, then suggestions, and oldest first within a group. Each item has a title, the agent and
run behind it, and in `detail` the full object as its own endpoint returns it.

Approvals, drafts and posts also read like messages, so the owner can review them without knowing the tool:

| Field | Meaning |
|---|---|
| `summary` | One line, such as the subject, the title, `Re: <item>` or the first line of the text; the `title` uses it too |
| `reason` | Why the agent proposes it: the call's `_reason` (see [agents.md](agents.md#reasons-and-context)), else what the agent said |
| `service` | Where it goes, for the logo: the `service` of the source whose action uses the tool, or `[publishing].service` for posts |
| `counterpart` | `name`, `address` and `org` of the person it is for, from the item it answers or a `to` argument |
| `context_item_id` | The connected item it answers, see `GET /connected/items/{id}` |
| `quote` | That item's `author`, `title`, `text`, `occurred_at` and `url`, to quote the original |
| `preview` | The arguments, readable: `fields` with `label` and `value`, and the `text` to review with its `text_key`. Labels come from the source action's `{{input.<field>}}` names; references to the item are left out |
| `actions` | What the owner can do, such as `approve`, `edit`, `rewrite`, `snooze`, `deny`, `dismiss` and `context` |
| `consulted` | What the run read before, for the source chips: `tool`, a `label` such as `Mail · 3 threads` or `Second Brain`, `service` and `count` of the collection's cached items. Proposals and the tools behind source actions are left out |

Drafts show only the newest per tool and target, and a newer proposal supersedes an equivalent open approval, see
[agents.md](agents.md#tools). `GET /runs/{id}/basis` returns the same labels for any run.

`POST /ready/{kind}/{id}/rewrite` with `{"tone": "shorter"}` returns a variant of the text of an approval, draft or
post in the owner's voice, as `{tone, text, run_id}`. The tone is `shorter`, `warmer`, `formal` or an instruction of
your own, such as "in German". Nothing is saved: `PUT /ready/approval/{id}/text` with `{"text": ...}` puts the owner's
version into the open approval's arguments, so approving sends that text.

## Tasks

Tasks are the owner's own to-do list. They are short, have an optional due date, a priority from 0 (none) to 3 (high),
notes and an optional `source` that links to what the task came from:

```json
{"kind": "issue", "reference": "ENG-42", "label": "Login fails on Safari", "url": "https://example.com/ENG-42"}
```

`kind` is free text. The apps know `run`, `approval`, `mail`, `issue`, `brain` and `url`.

| Endpoint | Purpose |
|---|---|
| `GET /tasks?view=` | `all` (default: open and proposed), `today` (open, due today or overdue), `upcoming` (open, due later or without a date), `done`, `proposed` |
| `POST /tasks` | Create a task at the top of the list |
| `GET`, `PATCH`, `DELETE /tasks/{id}` | Read, change or delete. In a `PATCH`, a missing field stays and `null` clears it |
| `POST /tasks/{id}/complete`, `/reopen` | Mark as done or put back on the list |
| `POST /tasks/{id}/accept` | Accept a task an agent proposed |
| `POST /tasks/order` | `{"ids": [...]}` puts these tasks first, in this order |
| `POST /tasks/{id}/delegate` | `{"agent_id": "..."}` hands the task to an agent, see below |

"Today" uses the time zone in `server.timezone`.

A delegated task starts a run of the agent with the task's title, notes and due date as context. The task shows
`delegated_agent` and `delegated_run`, and when the run ends, its report lands in `result` with `result_status`. The task
stays open: you decide whether it is done. Anything the agent wants to send still waits for your approval.

Agents cannot create tasks directly. A proposal has the status `proposed` and stays off the list until the owner
accepts it. Its `origin` says where it comes from: `agent`, with `agent_id` and `run_id`, or `pipeline` and `holdings`
for the suggestions of those pages, without a run. Deleting a proposal rejects it.

## Rules

Rules are standing instructions in the owner's words, such as "Always ask before booking anything" or "Never answer a
customer before the account manager has replied". A rule applies to every agent, or to one agent when `agent_id` is
set. `category` is free text for grouping in the apps. `applied_count` and `last_applied_at` tell how many runs got a
rule in their prompt and when the last one did.

| Endpoint | Purpose |
|---|---|
| `GET /rules` | All rules. With `?agent=<id>`, only the enabled rules a run of that agent sees |
| `POST /rules` | Create a rule, enabled unless `"enabled": false` |
| `GET`, `PATCH`, `DELETE /rules/{id}` | Read, change, enable or disable (`{"enabled": false}`), delete |

Every run reads its rules at the start and after each pause, so a change applies to the next model call. The rules go
into the system prompt as owner instructions:

```text
Standing rules from the owner. They come from the owner, not from a tool, and apply to this run. ...
<owner_rules>
- Always ask before booking anything
</owner_rules>
```

Tool results stay in `<untrusted_content>` tags, and their content cannot open or close an `<owner_rules>` tag. A rule
cannot widen what an agent may do: tool levels and approvals apply as before.

## Memory

Memory is what the assistant knows about the owner: preferences, habits and circumstances, one sentence each. Every
fact has a `source` and a `confidence` from 0 to 1.

- A fact the owner states through `POST /memory` is `confirmed`, with the source `owner` and the confidence 1.
- A fact an agent learned through the `memory.propose` tool is `proposed`, with the source `agent`, the run as
  reference and what it was learned from as label.

| Endpoint | Purpose |
|---|---|
| `GET /memory?status=&category=` | Facts, newest first |
| `POST /memory` | State a fact |
| `GET`, `PATCH`, `DELETE /memory/{id}` | Read, change or forget. `{"paused": true}` keeps a fact out of prompts. Rewording a fact confirms it |
| `POST /memory/{id}/confirm` | Confirm a proposed fact |

Runs see the newest 100 confirmed facts that are not paused, in an `<owner_facts>` block of the system prompt. Proposals
never reach a prompt, because an agent may have learned them from untrusted content.

Facts live in Postgres, not in the Second Brain. Brain entries are knowledge about the world in files you edit, while
facts need a confidence, a confirmation and a place in every prompt.

## Alerts

Alerts collect what broke. Every alert has a `kind`, a `key` that names the problem, a `severity` (`info`, `warning`
or `critical`) and the times it was first and last seen.

| Kind | Key | Severity | Raised when | Resolved when |
|---|---|---|---|---|
| `connector` | `connector:<name>` | `critical` | A run fails because a connector is not authorized, for example after its token expired | A run of an agent with tools of that connector succeeds |
| `connector` | `connector:<name>` | `warning` | A run fails because a connector cannot be reached | Same as above |
| `run_failed` | `run:<agent>` | `warning` | A run fails, stops at its step or time limit, or is rejected | The next run of that agent succeeds |
| `cost_limit` | `cost:<agent>` | `warning` | A run stops at its cost limit | The next run of that agent succeeds |
| `check_failed` | `check:<source>:<check>` | `warning` | A check of one of your pull requests starts failing | It passes again |
| `node_offline` | `node:<id>` | `warning` | A desktop agent that is not paused sends no report for 15 minutes | It reports again |
| `budget` | `budget:<YYYY-MM>` | `warning`, `critical` at 100% | This month's model costs reach the share of the budget set in [settings](#settings) | A new month starts with a new key |

`value` and `unit` hold the current value of the metric behind an alert: failures in a row (`failures`, counting up with
each repeat) for connectors and failed runs, the cost in `EUR` for cost limits, and `minutes` offline for desktop
agents. `events` is the cause timeline, oldest first: `raised`, `seen` for a repeat, `escalated`, `acknowledged` and
`resolved`, each with the error `text` and `value` of that moment and its time `at`.

While an alert of a key is unresolved, a repeat counts up `count` and moves `last_seen_at` instead of adding an alert,
and the severity can only go up. After it is resolved, the next occurrence opens a new alert.

| Endpoint | Purpose |
|---|---|
| `GET /alerts?view=` | `active` (default: open and acknowledged), `open`, `acknowledged`, `resolved` or `all`, worst first |
| `GET /alerts/counts` | Unresolved alerts per severity |
| `GET /alerts/{id}` | One alert |
| `POST /alerts/{id}/acknowledge` | Mark an open alert as seen. It stays until it is resolved |
| `POST /alerts/{id}/resolve` | Resolve it by hand |
| `GET /alerts/{id}/series?days=14` | A daily series for a chart, in days of `server.timezone`: runs, failures and cost of the agent for `run` and `cost_limit` alerts, failed runs naming the connector for `connector` alerts, and refreshes of the source for `check_failed` alerts. `metric` is `runs`, `syncs` or `none` |

## Agents

`GET /agents` and `GET /agents/{id}` describe each agent with its tools, skills and schedule, and also:

| Field | Meaning |
|---|---|
| `model` | `task`, `provider`, `model`, `fallback` and `fallback_model` of the `text` task, as it applies to the caller (see [Providers, models and connectors](#providers-models-and-connectors)). Display only |
| `autonomy` | `observe` (reads only), `prepare` (also saves drafts) or `propose` (also proposes actions, which always wait for your approval) |
| `max_cost_eur` | The cost limit of one run, from the agent file |
| `corrections` | Its proposals you denied or changed before approving |

## Settings

Settings belong to the tenant. Members read them; changing the budget needs the `owner` or `admin` role. Members,
roles and invitations are managed under [Users and tenants](#users-and-tenants).

| Endpoint | Purpose |
|---|---|
| `GET /settings/budget` | `monthly_eur`, `alert_at_percent` (default 80), `spent_this_month_eur` since the start of the month in your time zone, `remaining_eur`, and `run_limits` with each agent's `max_cost_eur` |
| `PUT /settings/budget` | Sets `{"monthly_eur", "alert_at_percent"}`. `monthly_eur: null` removes the budget |
| `GET /settings/models` | Which `provider` and `model` serve each task for the caller, with the fallback. The full view is `GET /settings/effective` |

The budget never stops runs; the cost limit per run does that. After each run, a `budget` alert (key
`budget:<YYYY-MM>`, `value` in `EUR`) warns once this month's model costs reach `alert_at_percent` of the budget, and
turns `critical` when the budget is used up.

### Providers, models and connectors

Providers, models, routing and MCP connectors come from three layers: `instance` (`lighthouse.toml`, read-only),
`tenant` (owners and admins set it for everyone) and `personal` (each member for themselves). See
[configuration.md](configuration.md#layers) for the merge rules and where keys come from. Paths name the layer:
`tenant`, or `me` for the caller's own. Every member reads the tenant layer and what applies to them; changing the
tenant layer needs the `owner` or `admin` role; every member changes their own layer. Nobody reads another member's
personal layer, and the admin token has none.

| Endpoint | Purpose |
|---|---|
| `GET /settings/effective` | What applies to the caller: `providers` with `models`, `routing` and `connectors`, each with its `source` layer. Providers also carry `defined_at` (the layer that chose kind and URL), `key_name` (the credential that holds the key) and `key` (the layer whose key is used now, or `null`). Routes carry a `problem` if they cannot work. Connectors carry `enabled`, `disabled_by`, `token_name` and `connected` (the layer whose token is used now). Also `tasks`, `can_manage` and `allow_private_endpoints` |
| `GET /settings/presets` | Suggested providers with models and list prices, and connectors, to prefill forms |
| `GET /settings/{layer}/entries` | What one layer stores, without what it inherits |
| `GET /settings/{layer}/changes` | Who changed what in a layer and when. Values are not recorded. The tenant layer's list needs the `owner` or `admin` role |
| `PUT /settings/{layer}/providers/{name}` | Adds a provider or changes it: `{"kind": "anthropic" or "openai", "base_url"}`. A name of a lower layer keeps that layer's kind and URL (409 otherwise) |
| `DELETE /settings/{layer}/providers/{name}` | Deletes a provider of the layer with its models. 409 while a route of the layer needs it |
| `PUT /settings/{layer}/providers/{name}/models/{task}` | Sets `{"model", "input_eur_per_mtok", "output_eur_per_mtok"}`. For a provider of a lower layer, the layer gets its own entry with the same kind and URL |
| `DELETE /settings/{layer}/providers/{name}/models/{task}` | Deletes a model of the layer |
| `PUT /settings/{layer}/routing/{task}` | Sets `{"provider", "fallback"}`. Both need a model for the task |
| `DELETE /settings/{layer}/routing/{task}` | Deletes the layer's route, so the lower one applies again |
| `PUT /settings/{layer}/connectors/{name}` | Adds or replaces a connector over streamable HTTP: `{"url", "auth": "none", "bearer" or "oauth", "client_id", "scopes", "read_tools", "draft_tools", "enabled"}` |
| `DELETE /settings/{layer}/connectors/{name}` | Deletes a connector of the layer |
| `PUT /settings/{layer}/connectors/{name}/enabled` | `{"enabled"}`: switches a connector of the layer, or one of a lower layer for this layer only |
| `GET /me/credentials`, `PUT` and `DELETE /me/credentials/{name}` | The caller's own credentials in this tenant, such as their own provider key or a connector's bearer token. Values are never returned |
| `POST /connectors/{name}/connect?scope=personal` | Starts the OAuth flow for the caller's own account. Without `scope`, an owner or admin connects the tenant's shared account; personal connectors always connect personally |

Names are lower-case identifiers of up to 40 characters. Tasks are the ones the server routes (`text`). Prices are
from 0 to 10000 EUR per million tokens. URLs must be public `https` URLs unless `[tenancy] allow_private_endpoints`
is on; private, loopback, link-local and metadata addresses are refused with 422. Limits of `[tenancy.limits]`
answer 409. A program built on the crates can refuse changes through its entitlements (`Action::ChangeSettings`,
403). Every change emits `settings.changed` and applies to the next run without a restart.

## Pushes

Besides the live events, Lighthouse can push to the owner through the delivery channels of agents. `[notifications]`
in the configuration decides what is sent:

```toml
[notifications]
deliver = ["owner"]   # owner, mail:<address> or slack:<alias>; empty sends nothing
decisions = true      # a run waits for a decision ("ask before")
done = false          # a run finished ("done")
alerts = "critical"   # lowest severity of new or escalated alerts to push
```

A push names the agent and the tools it proposes, never their arguments. Failed pushes are logged and do not affect
the run.

## Talk

Talk is a conversation with the assistant, kept as threads of messages.

| Endpoint | Purpose |
|---|---|
| `GET /talk/threads` | Threads, pinned ones first |
| `POST /talk/threads` | Start one, with an optional `title` |
| `GET`, `PATCH`, `DELETE /talk/threads/{id}` | Read with all messages, rename or pin (`{"pinned": true}`), delete |
| `POST /talk/threads/{id}/messages` | `{"text": "..."}` sends a message and returns the `question` and the `answer`. 409 if a server's entitlements refuse the run or the question; nothing is stored then |

With `[talk] agent = "<agent id>"` in the configuration, a run of that agent answers, with the last twelve messages as
context. The answer is `pending` (`202`) until the run ends, then `done` or `failed`, and its `run_id` leads to the run
with its tool calls and approvals, so the apps can show them inline. Anything the agent wants to send waits for your
approval as usual. Without a talk agent, the Second Brain answers at once (`200`) and the answer lists its `sources`.
The first message names a thread that has no title of its own.
